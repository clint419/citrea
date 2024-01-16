use std::net::SocketAddr;
use std::path::Path;

use chainway_sequencer::ChainwaySequencer;
use const_rollup_config::TEST_PRIVATE_KEY;
use demo_stf::genesis_config::GenesisPaths;
use sov_demo_rollup::MockDemoRollup;
use sov_mock_da::{MockAddress, MockDaConfig, MockDaService};
use sov_modules_api::default_context::DefaultContext;
use sov_modules_api::default_signature::private_key::DefaultPrivateKey;
use sov_modules_rollup_blueprint::{RollupAndStorage, RollupBlueprint};
use sov_modules_stf_blueprint::kernels::basic::{
    BasicKernelGenesisConfig, BasicKernelGenesisPaths,
};
use sov_stf_runner::{
    ProverServiceConfig, RollupConfig, RollupProverConfig, RpcConfig, RunnerConfig,
    SequencerClientRpcConfig, StorageConfig,
};
use tokio::sync::oneshot;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeMode {
    FullNode(SocketAddr),
    SequencerNode,
}

pub async fn start_rollup(
    rpc_reporting_channel: oneshot::Sender<SocketAddr>,
    rt_genesis_paths: GenesisPaths,
    kernel_genesis_paths: BasicKernelGenesisPaths,
    rollup_prover_config: RollupProverConfig,
    node_mode: NodeMode,
    db_path: Option<&str>,
) {
    let mut path = db_path.map(Path::new);
    let mut temp_dir: Option<tempfile::TempDir> = None;
    if db_path.is_none() {
        temp_dir = Some(tempfile::tempdir().unwrap());

        path = Some(temp_dir.as_ref().unwrap().path());
    }

    let rollup_config = RollupConfig {
        storage: StorageConfig {
            path: path.unwrap().to_path_buf(),
        },
        runner: RunnerConfig {
            start_height: 1,
            rpc_config: RpcConfig {
                bind_host: "127.0.0.1".into(),
                bind_port: 0,
            },
        },
        da: MockDaConfig {
            sender_address: MockAddress::from([0; 32]),
        },
        prover_service: ProverServiceConfig {
            aggregated_proof_block_jump: 1,
        },
        sequencer_client: match node_mode {
            NodeMode::FullNode(socket_addr) => Some(SequencerClientRpcConfig {
                url: format!("http://localhost:{}", socket_addr.port()),
            }),
            NodeMode::SequencerNode => None,
        },
    };

    let mock_demo_rollup = MockDemoRollup {};

    let kernel_genesis = BasicKernelGenesisConfig {
        chain_state: serde_json::from_str(
            &std::fs::read_to_string(&kernel_genesis_paths.chain_state)
                .expect("Failed to read chain_state genesis config"),
        )
        .expect("Failed to parse chain_state genesis config"),
    };

    let RollupAndStorage { rollup, storage } = mock_demo_rollup
        .create_new_rollup(
            &rt_genesis_paths,
            kernel_genesis,
            rollup_config,
            rollup_prover_config,
        )
        .await
        .unwrap();

    match node_mode {
        NodeMode::FullNode(_) => {
            rollup
                .run_and_report_rpc_port(Some(rpc_reporting_channel))
                .await
                .unwrap();
        }
        NodeMode::SequencerNode => {
            let da_service = MockDaService::new(MockAddress::new([0u8; 32]));

            let mut sequencer: ChainwaySequencer<DefaultContext, MockDaService, _> =
                ChainwaySequencer::new(
                    rollup,
                    da_service,
                    DefaultPrivateKey::from_hex(TEST_PRIVATE_KEY).unwrap(),
                    storage,
                );
            sequencer
                .start_rpc_server(Some(rpc_reporting_channel))
                .await
                .unwrap();
            sequencer.run().await.unwrap();
        }
    }

    if db_path.is_none() {
        // Close the tempdir explicitly to ensure that rustc doesn't see that it's unused and drop it unexpectedly
        temp_dir.unwrap().close().unwrap();
    }
}
