mod gas_price;

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::Mutex;

#[cfg(feature = "local")]
pub use citrea_evm::DevSigner;
use citrea_evm::{EthApiError, Evm};
use ethers::types::Bytes;
pub use gas_price::fee_history::FeeHistoryCacheConfig;
use gas_price::gas_oracle::GasPriceOracle;
pub use gas_price::gas_oracle::GasPriceOracleConfig;
use jsonrpsee::types::ErrorObjectOwned;
use jsonrpsee::RpcModule;
use reth_primitives::{keccak256, BlockNumberOrTag, B256, U256};
use reth_rpc_types::trace::geth::{
    CallConfig, CallFrame, FourByteFrame, GethDebugBuiltInTracerType, GethDebugTracerConfig,
    GethDebugTracerType, GethDebugTracingOptions, GethTrace, NoopFrame,
};
use reth_rpc_types::FeeHistory;
use rustc_version_runtime::version;
use schnellru::{ByLength, LruMap};
use sequencer_client::SequencerClient;
use serde_json::json;
use sov_modules_api::utils::to_jsonrpsee_error_object;
use sov_modules_api::WorkingSet;
use sov_rollup_interface::services::da::DaService;
use tracing::info;

use crate::gas_price::gas_oracle::convert_u256_to_u64;

const ETH_RPC_ERROR: &str = "ETH_RPC_ERROR";

const MAX_TRACE_BLOCK: u32 = 1000;

#[derive(Clone)]
pub struct EthRpcConfig {
    pub gas_price_oracle_config: GasPriceOracleConfig,
    pub fee_history_cache_config: FeeHistoryCacheConfig,
    #[cfg(feature = "local")]
    pub eth_signer: DevSigner,
}

pub fn get_ethereum_rpc<C: sov_modules_api::Context, Da: DaService>(
    da_service: Da,
    eth_rpc_config: EthRpcConfig,
    storage: C::Storage,
    sequencer_client: Option<SequencerClient>,
) -> RpcModule<Ethereum<C, Da>> {
    // Unpack config
    let EthRpcConfig {
        #[cfg(feature = "local")]
        eth_signer,
        gas_price_oracle_config,
        fee_history_cache_config,
    } = eth_rpc_config;

    // If the node does not have a sequencer client, then it is the sequencer.
    let is_sequencer = sequencer_client.is_none();

    // If the running node is a full node rpc context should also have sequencer client so that it can send txs to sequencer
    let mut rpc = RpcModule::new(Ethereum::new(
        da_service,
        gas_price_oracle_config,
        fee_history_cache_config,
        #[cfg(feature = "local")]
        eth_signer,
        storage,
        sequencer_client,
    ));

    register_rpc_methods(&mut rpc, is_sequencer).expect("Failed to register ethereum RPC methods");
    rpc
}

pub struct Ethereum<C: sov_modules_api::Context, Da: DaService> {
    #[allow(dead_code)]
    da_service: Da,
    gas_price_oracle: GasPriceOracle<C>,
    #[cfg(feature = "local")]
    eth_signer: DevSigner,
    storage: C::Storage,
    sequencer_client: Option<SequencerClient>,
    web3_client_version: String,
    trace_cache: Mutex<LruMap<u64, Vec<GethTrace>, ByLength>>,
}

impl<C: sov_modules_api::Context, Da: DaService> Ethereum<C, Da> {
    fn new(
        da_service: Da,
        gas_price_oracle_config: GasPriceOracleConfig,
        fee_history_cache_config: FeeHistoryCacheConfig,
        #[cfg(feature = "local")] eth_signer: DevSigner,
        storage: C::Storage,
        sequencer_client: Option<SequencerClient>,
    ) -> Self {
        let evm = Evm::<C>::default();
        let gas_price_oracle =
            GasPriceOracle::new(evm, gas_price_oracle_config, fee_history_cache_config);

        let rollup = "citrea";
        let arch = std::env::consts::ARCH;
        let rustc_v = version();

        let git_latest_tag = match get_latest_git_tag() {
            Ok(tag) => tag,
            Err(e) => {
                info!("Failed to get latest git tag: {}", e);
                "unknown".to_string()
            }
        };

        let current_version = format!("{}/{}/{}/rust-{}", rollup, git_latest_tag, arch, rustc_v);

        let trace_cache = Mutex::new(LruMap::new(ByLength::new(MAX_TRACE_BLOCK)));

        Self {
            da_service,
            gas_price_oracle,
            #[cfg(feature = "local")]
            eth_signer,
            storage,
            sequencer_client,
            web3_client_version: current_version,
            trace_cache,
        }
    }
}

// impl<C: sov_modules_api::Context, Da: DaService> Ethereum<C, Da> {
//     fn make_raw_tx(
//         &self,
//         raw_tx: RlpEvmTransaction,
//     ) -> Result<(B256, Vec<u8>), jsonrpsee::core::Error> {
//         let signed_transaction: RethTransactionSignedNoHash = raw_tx.clone().try_into()?;

//         let tx_hash = signed_transaction.hash();

//         let tx = CallMessage { txs: vec![raw_tx] };
//         let message = <Runtime<C, Da::Spec> as EncodeCall<citrea_evm::Evm<C>>>::encode_call(tx);

//         Ok((B256::from(tx_hash), message))
//     }
// }

fn register_rpc_methods<C: sov_modules_api::Context, Da: DaService>(
    rpc: &mut RpcModule<Ethereum<C, Da>>,
    // Checks wether the running node is a sequencer or not, if it is not a sequencer it should also have methods like eth_sendRawTransaction here.
    is_sequencer: bool,
) -> Result<(), jsonrpsee::core::Error> {
    rpc.register_async_method("web3_clientVersion", |_, ethereum| async move {
        info!("eth module: web3_clientVersion");

        Ok::<_, ErrorObjectOwned>(ethereum.web3_client_version.clone())
    })?;

    rpc.register_async_method("web3_sha3", |params, _| async move {
        info!("eth module: web3_sha3");
        let data: Bytes = params.one()?;

        let hash = B256::from_slice(keccak256(&data).as_slice());

        Ok::<_, ErrorObjectOwned>(hash)
    })?;

    rpc.register_async_method("eth_gasPrice", |_, ethereum| async move {
        info!("eth module: eth_gasPrice");
        let price = {
            let mut working_set = WorkingSet::<C>::new(ethereum.storage.clone());

            let suggested_tip = ethereum
                .gas_price_oracle
                .suggest_tip_cap(&mut working_set)
                .await
                .unwrap();

            let evm = Evm::<C>::default();
            let base_fee = evm
                .get_block_by_number(None, None, &mut working_set)
                .unwrap()
                .unwrap()
                .header
                .base_fee_per_gas
                .unwrap_or_default();

            suggested_tip + base_fee
        };

        Ok::<U256, ErrorObjectOwned>(price)
    })?;

    rpc.register_async_method("eth_maxFeePerGas", |_, ethereum| async move {
        let max_fee_per_gas = {
            let mut working_set = WorkingSet::<C>::new(ethereum.storage.clone());

            ethereum
                .gas_price_oracle
                .suggest_tip_cap(&mut working_set)
                .await
                .unwrap()
        };

        Ok::<U256, ErrorObjectOwned>(max_fee_per_gas)
    })?;

    rpc.register_async_method("eth_feeHistory", |params, ethereum| async move {
        info!("eth module: eth_feeHistory");
        let mut params = params.sequence();

        let block_count: String = params.next().unwrap();
        let newest_block: BlockNumberOrTag = params.next().unwrap();
        let reward_percentiles: Option<Vec<f64>> = params.optional_next()?;

        // convert block count to u64 from hex
        let block_count = u64::from_str_radix(&block_count[2..], 16)
            .map_err(|e| to_jsonrpsee_error_object(e, ETH_RPC_ERROR))?;

        let fee_history = {
            let mut working_set = WorkingSet::<C>::new(ethereum.storage.clone());

            ethereum
                .gas_price_oracle
                .fee_history(
                    block_count,
                    newest_block,
                    reward_percentiles,
                    &mut working_set,
                )
                .await
                .unwrap()
        };

        Ok::<FeeHistory, ErrorObjectOwned>(fee_history)
    })?;

    #[cfg(feature = "local")]
    rpc.register_async_method("eth_accounts", |_, ethereum| async move {
        info!("eth module: eth_accounts");

        Ok::<_, ErrorObjectOwned>(ethereum.eth_signer.signers())
    })?;

    // #[cfg(feature = "local")]
    // rpc.register_async_method("eth_sendTransaction", |parameters, ethereum| async move {
    //     info!("eth module: eth_sendTransaction");

    //     let mut transaction_request: TransactionRequest = parameters.one().unwrap();

    //     let evm = Evm::<C>::default();

    //     // get from, return error if none
    //     let from = transaction_request
    //         .from
    //         .ok_or(to_jsonrpsee_error_object("No from address", ETH_RPC_ERROR))?;

    //     // return error if not in signers
    //     if !ethereum.eth_signer.signers().contains(&from) {
    //         return Err(to_jsonrpsee_error_object(
    //             "From address not in signers",
    //             ETH_RPC_ERROR,
    //         ));
    //     }

    //     let raw_evm_tx = {
    //         let mut working_set = WorkingSet::<C>::new(ethereum.storage.clone());

    //         // set nonce if none
    //         if transaction_request.nonce.is_none() {
    //             let nonce = evm
    //                 .get_transaction_count(from, None, &mut working_set)
    //                 .unwrap_or_default();

    //             transaction_request.nonce = Some(nonce);
    //         }

    //         // get current chain id
    //         let chain_id = evm
    //             .chain_id(&mut working_set)
    //             .expect("Failed to get chain id")
    //             .map(|id| id.to::<u64>())
    //             .unwrap_or(1);

    //         // get call request to estimate gas and gas prices
    //         let (call_request, _gas_price, _max_fee_per_gas) =
    //             get_call_request_and_params(from, chain_id, &transaction_request);

    //         // estimate gas limit
    //         let gas_limit = U256::from(
    //             evm.eth_estimate_gas(call_request, None, &mut working_set)?
    //                 .to::<u64>(),
    //         );

    //         let TransactionRequest {
    //             to,
    //             gas_price,
    //             max_fee_per_gas,
    //             max_priority_fee_per_gas,
    //             gas,
    //             value,
    //             input: data,
    //             nonce,
    //             mut access_list,
    //             max_fee_per_blob_gas,
    //             blob_versioned_hashes,
    //             sidecar,
    //             ..
    //         } = transaction_request;

    //         // todo: remove this inlining after https://github.com/alloy-rs/alloy/pull/183#issuecomment-1928161285
    //         let transaction = match (
    //             gas_price,
    //             max_fee_per_gas,
    //             access_list.take(),
    //             max_fee_per_blob_gas,
    //             blob_versioned_hashes,
    //             sidecar,
    //         ) {
    //             // legacy transaction
    //             // gas price required
    //             (Some(_), None, None, None, None, None) => {
    //                 Some(TypedTransactionRequest::Legacy(LegacyTransactionRequest {
    //                     nonce: nonce.unwrap_or_default(),
    //                     gas_price: gas_price.unwrap_or_default(),
    //                     gas_limit: gas.unwrap_or_default(),
    //                     value: value.unwrap_or_default(),
    //                     input: data.into_input().unwrap_or_default(),
    //                     kind: match to {
    //                         Some(to) => RpcTransactionKind::Call(to),
    //                         None => RpcTransactionKind::Create,
    //                     },
    //                     chain_id: None,
    //                 }))
    //             }
    //             // EIP2930
    //             // if only accesslist is set, and no eip1599 fees
    //             (_, None, Some(access_list), None, None, None) => Some(
    //                 TypedTransactionRequest::EIP2930(EIP2930TransactionRequest {
    //                     nonce: nonce.unwrap_or_default(),
    //                     gas_price: gas_price.unwrap_or_default(),
    //                     gas_limit: gas.unwrap_or_default(),
    //                     value: value.unwrap_or_default(),
    //                     input: data.into_input().unwrap_or_default(),
    //                     kind: match to {
    //                         Some(to) => RpcTransactionKind::Call(to),
    //                         None => RpcTransactionKind::Create,
    //                     },
    //                     chain_id: 0,
    //                     access_list,
    //                 }),
    //             ),
    //             // EIP1559
    //             // if 4844 fields missing
    //             // gas_price, max_fee_per_gas, access_list, max_fee_per_blob_gas, blob_versioned_hashes,
    //             // sidecar,
    //             (None, _, _, None, None, None) => {
    //                 // Empty fields fall back to the canonical transaction schema.
    //                 Some(TypedTransactionRequest::EIP1559(
    //                     EIP1559TransactionRequest {
    //                         nonce: nonce.unwrap_or_default(),
    //                         max_fee_per_gas: max_fee_per_gas.unwrap_or_default(),
    //                         max_priority_fee_per_gas: max_priority_fee_per_gas.unwrap_or_default(),
    //                         gas_limit: gas.unwrap_or_default(),
    //                         value: value.unwrap_or_default(),
    //                         input: data.into_input().unwrap_or_default(),
    //                         kind: match to {
    //                             Some(to) => RpcTransactionKind::Call(to),
    //                             None => RpcTransactionKind::Create,
    //                         },
    //                         chain_id: 0,
    //                         access_list: access_list.unwrap_or_default(),
    //                     },
    //                 ))
    //             }
    //             // EIP4884
    //             // all blob fields required
    //             (
    //                 None,
    //                 _,
    //                 _,
    //                 Some(max_fee_per_blob_gas),
    //                 Some(blob_versioned_hashes),
    //                 Some(sidecar),
    //             ) => {
    //                 // As per the EIP, we follow the same semantics as EIP-1559.
    //                 Some(TypedTransactionRequest::EIP4844(
    //                     EIP4844TransactionRequest {
    //                         chain_id: 0,
    //                         nonce: nonce.unwrap_or_default(),
    //                         max_priority_fee_per_gas: max_priority_fee_per_gas.unwrap_or_default(),
    //                         max_fee_per_gas: max_fee_per_gas.unwrap_or_default(),
    //                         gas_limit: gas.unwrap_or_default(),
    //                         value: value.unwrap_or_default(),
    //                         input: data.into_input().unwrap_or_default(),
    //                         kind: match to {
    //                             Some(to) => RpcTransactionKind::Call(to),
    //                             None => RpcTransactionKind::Create,
    //                         },
    //                         access_list: access_list.unwrap_or_default(),

    //                         // eip-4844 specific.
    //                         max_fee_per_blob_gas,
    //                         blob_versioned_hashes,
    //                         sidecar,
    //                     },
    //                 ))
    //             }

    //             _ => None,
    //         };

    //         // get typed transaction request
    //         let transaction_request = match transaction {
    //             Some(TypedTransactionRequest::Legacy(mut m)) => {
    //                 m.chain_id = Some(chain_id);
    //                 m.gas_limit = gas_limit;
    //                 m.gas_price = gas_price.unwrap();

    //                 TypedTransactionRequest::Legacy(m)
    //             }
    //             Some(TypedTransactionRequest::EIP2930(mut m)) => {
    //                 m.chain_id = chain_id;
    //                 m.gas_limit = gas_limit;
    //                 m.gas_price = gas_price.unwrap();

    //                 TypedTransactionRequest::EIP2930(m)
    //             }
    //             Some(TypedTransactionRequest::EIP1559(mut m)) => {
    //                 m.chain_id = chain_id;
    //                 m.gas_limit = gas_limit;
    //                 m.max_fee_per_gas = max_fee_per_gas.unwrap();

    //                 TypedTransactionRequest::EIP1559(m)
    //             }
    //             Some(TypedTransactionRequest::EIP4844(mut m)) => {
    //                 m.chain_id = chain_id;
    //                 m.gas_limit = gas_limit;
    //                 m.max_fee_per_gas = max_fee_per_gas.unwrap();

    //                 TypedTransactionRequest::EIP4844(m)
    //             }
    //             None => return Err(EthApiError::ConflictingFeeFieldsInRequest.into()),
    //         };

    //         // get raw transaction
    //         let transaction = to_primitive_transaction(transaction_request)
    //             .ok_or(SignError::InvalidTransactionRequest)?;

    //         // sign transaction
    //         let signed_tx = ethereum
    //             .eth_signer
    //             .sign_transaction(transaction, from)
    //             .map_err(|e| to_jsonrpsee_error_object(e, ETH_RPC_ERROR))?;

    //         RlpEvmTransaction {
    //             rlp: signed_tx.envelope_encoded().to_vec(),
    //         }
    //     };
    //     let (tx_hash, raw_message) = ethereum
    //         .make_raw_tx(raw_evm_tx)
    //         .map_err(|e| to_jsonrpsee_error_object(e, ETH_RPC_ERROR))?;

    //     ethereum.add_messages(vec![raw_message]);

    //     Ok::<_, ErrorObjectOwned>(tx_hash)
    // })?;

    rpc.register_async_method(
        "debug_traceBlockByHash",
        |parmaeters, ethereum| async move {
            info!("eth module: debug_traceBlockByHash");

            let mut params = parmaeters.sequence();

            let block_hash: B256 = params.next().unwrap();
            let evm = Evm::<C>::default();
            let mut working_set = WorkingSet::<C>::new(ethereum.storage.clone());
            let opts: Option<GethDebugTracingOptions> = params.optional_next().unwrap();

            let block_number =
                match evm.get_block_number_by_block_hash(block_hash, &mut working_set) {
                    Some(block_number) => block_number,
                    None => {
                        return Err(to_jsonrpsee_error_object(
                            EthApiError::UnknownBlockNumber,
                            ETH_RPC_ERROR,
                        ));
                    }
                };

            // If opts is None or if opts.tracer is None, then do not check cache or insert cache, just perform the operation
            if opts.as_ref().map_or(true, |o| o.tracer.is_none()) {
                return evm
                    .trace_block_transactions_by_number(
                        block_number,
                        opts.clone(),
                        None,
                        &mut working_set,
                    )
                    .map_err(|e| to_jsonrpsee_error_object(e, ETH_RPC_ERROR));
            }

            if let Some(traces) = ethereum.trace_cache.lock().unwrap().get(&block_number) {
                // If traces are found in cache convert them to specified opts and then return
                let requested_opts = opts.clone().unwrap();
                let traces = get_traces_with_reuqested_tracer_and_config(
                    traces.clone(),
                    requested_opts.tracer.unwrap(),
                    requested_opts.tracer_config,
                )?;
                return Ok::<Vec<GethTrace>, ErrorObjectOwned>(traces);
            }
            let cache_options = create_trace_cache_opts();
            let traces = evm
                .trace_block_transactions_by_number(
                    block_number,
                    Some(cache_options),
                    None,
                    &mut working_set,
                )
                .map_err(|e| to_jsonrpsee_error_object(e, ETH_RPC_ERROR))?;
            ethereum
                .trace_cache
                .lock()
                .unwrap()
                .insert(block_number, traces.clone());
            // Convert the traces to the requested tracer and config
            let requested_opts = opts.clone().unwrap();
            let tracer_config = requested_opts.tracer_config;
            let traces = get_traces_with_reuqested_tracer_and_config(
                traces.clone(),
                requested_opts.tracer.unwrap(),
                tracer_config,
            )?;

            Ok::<Vec<GethTrace>, ErrorObjectOwned>(traces)
        },
    )?;

    rpc.register_async_method(
        "debug_traceBlockByNumber",
        |parameters, ethereum| async move {
            info!("eth module: debug_traceBlockByNumber");

            let mut params = parameters.sequence();

            let block_number: BlockNumberOrTag = params.next().unwrap();
            let opts: Option<GethDebugTracingOptions> = params.optional_next().unwrap();

            let mut working_set = WorkingSet::<C>::new(ethereum.storage.clone());
            let evm = Evm::<C>::default();
            let block_number = match block_number {
                BlockNumberOrTag::Number(block_number) => block_number,
                BlockNumberOrTag::Latest => convert_u256_to_u64(evm.block_number(&mut working_set)?),
                _ => {
                    return Err(to_jsonrpsee_error_object(
                        EthApiError::Unsupported("Earliest, pending, safe and finalized are not supported for debug_traceBlockByNumber"),
                        ETH_RPC_ERROR,
                    ));
                }
            };

            // If opts is None or if opts.tracer is None, then do not check cache or insert cache, just perform the operation
            if opts.as_ref().map_or(true, |o| o.tracer.is_none()) {
                return evm.trace_block_transactions_by_number(block_number, opts.clone(), None, &mut working_set)
                        .map_err(|e| to_jsonrpsee_error_object(e, ETH_RPC_ERROR));
            }

            if let Some(traces) = ethereum.trace_cache.lock().unwrap().get(&block_number) {
                // If traces are found in cache convert them to specified opts and then return
                let requested_opts = opts.clone().unwrap();
                let tracer_config = requested_opts.tracer_config;
                let traces = get_traces_with_reuqested_tracer_and_config(traces.clone(), requested_opts.tracer.unwrap(), tracer_config)?;
                return Ok::<Vec<GethTrace>, ErrorObjectOwned>(traces);
            }

            let cache_options = create_trace_cache_opts();
            let traces = evm
                .trace_block_transactions_by_number(
                    block_number,
                    Some(cache_options),
                    None,
                    &mut working_set,
                )
                .map_err(|e| to_jsonrpsee_error_object(e, ETH_RPC_ERROR))?;
                ethereum
                    .trace_cache
                    .lock()
                    .unwrap()
                    .insert(block_number, traces.clone());

            // Convert the traces to the requested tracer and config
            let requested_opts = opts.clone().unwrap();
            let tracer_config = requested_opts.tracer_config;
            let traces = get_traces_with_reuqested_tracer_and_config(traces.clone(), requested_opts.tracer.unwrap(), tracer_config)?;

            Ok::<Vec<GethTrace>, ErrorObjectOwned>(traces)
        },
    )?;

    rpc.register_async_method(
        "debug_traceTransaction",
        |parameters, ethereum| async move {
            // the main rpc handler for debug_traceTransaction
            // Checks the cache in ethereum struct if the trace exists
            // if found; returns the trace
            // else; calls the debug_trace_transaction_block function in evm
            // that function traces the entire block, returns all the traces to here
            // then we put them into cache and return the trace of the requested transaction
            info!("eth module: debug_traceTransaction");

            let mut params = parameters.sequence();

            let tx_hash: B256 = params.next()?;

            let evm = Evm::<C>::default();
            let mut working_set = WorkingSet::<C>::new(ethereum.storage.clone());

            let tx = evm
                .get_transaction_by_hash(tx_hash, &mut working_set)
                .unwrap()
                .ok_or_else(|| EthApiError::UnknownBlockOrTxIndex)?;
            let trace_index = convert_u256_to_u64(
                tx.transaction_index
                    .expect("Tx index must be set for tx inside block"),
            );

            let block_number = convert_u256_to_u64(
                tx.block_number
                    .expect("Block number must be set for tx inside block"),
            );

            let opts: Option<GethDebugTracingOptions> = params.optional_next().unwrap();

            // If opts is None or if opts.tracer is None, then do not check cache or insert cache, just perform the operation
            // also since this is not cached we need to stop at somewhere, so we add param stop_at
            if opts.as_ref().map_or(true, |o| o.tracer.is_none()) {
                return Ok::<GethTrace, ErrorObjectOwned>(
                    evm.trace_block_transactions_by_number(
                        block_number,
                        opts.clone(),
                        Some(trace_index as usize),
                        &mut working_set,
                    )
                    .map_err(|e| to_jsonrpsee_error_object(e, ETH_RPC_ERROR))?
                        [trace_index as usize]
                        .clone(),
                );
            }

            // check cache if found convert to requested tracer and config and return
            if let Some(traces) = ethereum.trace_cache.lock().unwrap().get(&block_number) {
                let requested_opts = opts.clone().unwrap();
                let tracer_config = requested_opts.tracer_config;
                let traces = get_traces_with_reuqested_tracer_and_config(
                    vec![traces[trace_index as usize].clone()],
                    requested_opts.tracer.unwrap(),
                    tracer_config,
                )?;
                return Ok::<GethTrace, ErrorObjectOwned>(traces.into_iter().next().unwrap());
            }

            let cache_options = create_trace_cache_opts();
            let traces = evm
                .trace_block_transactions_by_number(
                    block_number,
                    Some(cache_options),
                    None,
                    &mut working_set,
                )
                .map_err(|e| to_jsonrpsee_error_object(e, ETH_RPC_ERROR))?;
            ethereum
                .trace_cache
                .lock()
                .unwrap()
                .insert(block_number, traces.clone());
            // Convert the traces to the requested tracer and config
            let requested_opts = opts.clone().unwrap();
            let tracer_config = requested_opts.tracer_config;
            let traces = get_traces_with_reuqested_tracer_and_config(
                vec![traces[trace_index as usize].clone()],
                requested_opts.tracer.unwrap(),
                tracer_config,
            )?;

            Ok::<GethTrace, ErrorObjectOwned>(traces.into_iter().next().unwrap())
        },
    )?;

    rpc.register_async_method("txpool_content", |_, _| async move {
        info!("eth module: txpool_content");

        // This is a simple mock for serde.
        let json = json!({
            "pending": {},
            "queued": {}
        });

        Ok::<_, ErrorObjectOwned>(json)
    })?;

    rpc.register_async_method(
        "eth_getUncleByBlockHashAndIndex",
        |parameters, _| async move {
            info!("eth module: eth_getUncleByBlockHashAndIndex");

            let mut params = parameters.sequence();

            let _block_hash: String = params.next().unwrap();
            let _uncle_index_position: String = params.next().unwrap();

            let res = json!(null);

            Ok::<_, ErrorObjectOwned>(res)
        },
    )?;

    if !is_sequencer {
        rpc.register_async_method(
            "eth_sendRawTransaction",
            |parameters, ethereum| async move {
                info!("Full Node: eth_sendRawTransaction");
                // send this directly to the sequencer
                let data: Bytes = parameters.one().unwrap();
                // sequencer client should send it
                let tx_hash = ethereum
                    .sequencer_client
                    .as_ref()
                    .unwrap()
                    .send_raw_tx(data)
                    .await;

                tx_hash.map_err(|e| to_jsonrpsee_error_object(e, ETH_RPC_ERROR))
            },
        )?;

        rpc.register_async_method(
            "eth_getTransactionByHash",
            |parameters, ethereum| async move {
                let mut params = parameters.sequence();
                let hash: B256 = params.next().unwrap();
                let mempool_only: Result<Option<bool>, ErrorObjectOwned> = params.next();
                info!(
                    "Full Node: eth_getTransactionByHash({}, {:?})",
                    hash, mempool_only
                );

                // check if mempool_only parameter was given what was its value
                match mempool_only {
                    // only ask sequencer
                    Ok(Some(true)) => {
                        let tx = ethereum
                            .sequencer_client
                            .as_ref()
                            .unwrap()
                            .get_tx_by_hash(hash, Some(true))
                            .await
                            .map_err(|e| to_jsonrpsee_error_object(e, ETH_RPC_ERROR))?;

                        Ok::<Option<reth_rpc_types::Transaction>, ErrorObjectOwned>(tx)
                    }
                    _ => {
                        // if mempool_only is not true ask evm first then sequencer
                        let evm = Evm::<C>::default();
                        let mut working_set = WorkingSet::<C>::new(ethereum.storage.clone());
                        match evm.get_transaction_by_hash(hash, &mut working_set) {
                            Ok(Some(tx)) => Ok::<
                                Option<reth_rpc_types::Transaction>,
                                ErrorObjectOwned,
                            >(Some(tx)),
                            Ok(None) => {
                                // if not found in evm then ask to sequencer mempool
                                let tx = ethereum
                                    .sequencer_client
                                    .as_ref()
                                    .unwrap()
                                    .get_tx_by_hash(hash, Some(true))
                                    .await
                                    .map_err(|e| to_jsonrpsee_error_object(e, ETH_RPC_ERROR))?;

                                Ok::<Option<reth_rpc_types::Transaction>, ErrorObjectOwned>(tx)
                            }
                            Err(e) => {
                                // return error
                                Err(to_jsonrpsee_error_object(e, ETH_RPC_ERROR))
                            }
                        }
                    }
                }
            },
        )?;
    }

    Ok(())
}

// fn get_call_request_and_params(
//     from: Address,
//     chain_id: u64,
//     request: &TransactionRequest,
// ) -> (TransactionRequest, U256, U256) {
//     // TODO: we need an oracle to fetch the gas price of the current chain
//     // https://github.com/Sovereign-Labs/sovereign-sdk/issues/883
//     let gas_price = request.gas_price.unwrap_or_default();
//     let max_fee_per_gas = request.max_fee_per_gas.unwrap_or_default();

//     // TODO: Generate call request better according to the transaction type
//     // https://github.com/Sovereign-Labs/sovereign-sdk/issues/946
//     let call_request = TransactionRequest {
//         from: Some(from),
//         to: request.to,
//         gas: request.gas,
//         gas_price: Some(U256::from(gas_price)),
//         max_fee_per_gas: Some(U256::from(max_fee_per_gas)),
//         value: request.value,
//         input: request.input.clone(),
//         nonce: request.nonce,
//         chain_id: Some(U64::from(chain_id)),
//         access_list: request.access_list.clone(),
//         max_priority_fee_per_gas: Some(U256::from(max_fee_per_gas)),
//         transaction_type: None,
//         blob_versioned_hashes: None,
//         max_fee_per_blob_gas: None,
//         sidecar: None,
//         other: OtherFields::default(),
//     };

//     (call_request, gas_price, max_fee_per_gas)
// }

pub fn get_latest_git_tag() -> Result<String, ErrorObjectOwned> {
    let latest_tag_commit = Command::new("git")
        .args(["rev-list", "--tags", "--max-count=1"])
        .output()
        .map_err(|e| to_jsonrpsee_error_object(e, "Failed to get version"))?;

    if !latest_tag_commit.status.success() {
        return Err(to_jsonrpsee_error_object(
            "Failure",
            "Failed to get version",
        ));
    }

    let latest_tag_commit = String::from_utf8_lossy(&latest_tag_commit.stdout)
        .trim()
        .to_string();

    let latest_tag = Command::new("git")
        .args(["describe", "--tags", &latest_tag_commit])
        .output()
        .map_err(|e| to_jsonrpsee_error_object(e, "Failed to get version"))?;

    if !latest_tag.status.success() {
        return Err(to_jsonrpsee_error_object(
            "Failure",
            "Failed to get version",
        ));
    }

    Ok(String::from_utf8_lossy(&latest_tag.stdout)
        .trim()
        .to_string())
}

fn apply_call_config(call_frame: CallFrame, call_config: CallConfig) -> CallFrame {
    // let only_top_call = call_config.only_top_call.unwrap_or();
    let mut new_call_frame = call_frame.clone();
    if let Some(true) = call_config.only_top_call {
        new_call_frame.calls = vec![];
    }
    if !call_config.with_log.unwrap_or(false) {
        remove_logs_from_call_frame(&mut vec![new_call_frame.clone()]);
    }
    new_call_frame
}

fn remove_logs_from_call_frame(call_frame: &mut Vec<CallFrame>) {
    for frame in call_frame {
        frame.logs = vec![];
        remove_logs_from_call_frame(&mut frame.calls);
    }
}

fn get_traces_with_reuqested_tracer_and_config(
    traces: Vec<GethTrace>,
    tracer: GethDebugTracerType,
    tracer_config: GethDebugTracerConfig,
) -> Result<Vec<GethTrace>, EthApiError> {
    // This can be only CallConfig or PreStateConfig if it is not CallConfig return Error for now

    let mut new_traces = vec![];
    match tracer {
        GethDebugTracerType::BuiltInTracer(builtin_tracer) => {
            match builtin_tracer {
                GethDebugBuiltInTracerType::CallTracer => {
                    // Apply the call config to the traces
                    let call_config =
                        GethDebugTracerConfig::into_call_config(tracer_config).unwrap_or_default();
                    // if call config is the same in the cache then do not process again and return early
                    match call_config {
                        CallConfig {
                            only_top_call: None,
                            with_log: Some(true),
                        }
                        | CallConfig {
                            only_top_call: Some(false),
                            with_log: Some(true),
                        } => {
                            return Ok(traces);
                        }
                        _ => {
                            traces.into_iter().for_each(|trace| {
                                if let GethTrace::CallTracer(call_frame) = trace {
                                    let new_call_frame =
                                        apply_call_config(call_frame.clone(), call_config);
                                    new_traces.push(GethTrace::CallTracer(new_call_frame));
                                }
                            });
                        }
                    }
                    Ok(new_traces)
                }
                GethDebugBuiltInTracerType::FourByteTracer => {
                    traces.into_iter().for_each(|trace| {
                        if let GethTrace::CallTracer(call_frame) = trace {
                            let four_byte_frame =
                                convert_call_trace_into_4byte_frame(vec![call_frame]);
                            new_traces.push(GethTrace::FourByteTracer(four_byte_frame));
                        }
                    });
                    Ok(new_traces)
                }
                GethDebugBuiltInTracerType::NoopTracer => {
                    Ok(vec![GethTrace::NoopTracer(NoopFrame::default())])
                }
                _ => Err(EthApiError::Unsupported("This tracer is not supported")),
            }
        }
        GethDebugTracerType::JsTracer(_code) => {
            // This also requires DatabaseRef trait
            // Implement after readonly state is implemented
            Err(EthApiError::Unsupported("JsTracer"))
        }
    }
}

pub fn convert_call_trace_into_4byte_frame(call_frames: Vec<CallFrame>) -> FourByteFrame {
    FourByteFrame(convert_call_trace_into_4byte_map(
        call_frames,
        BTreeMap::new(),
    ))
}

fn convert_call_trace_into_4byte_map(
    call_frames: Vec<CallFrame>,
    mut four_byte_map: BTreeMap<String, u64>,
) -> BTreeMap<String, u64> {
    // For each input in each call
    // get the first 4 bytes, get the size of the input
    // the key is : "<first 4 bytes>-<size of the input>"
    // value is the occurence of the key
    for call_frame in call_frames {
        let input = call_frame.input;
        // If this is a function call (function selector is 4 bytes long)
        if input.len() >= 4 {
            let input_size = input.0.len() - 4;
            let four_byte = &input.to_string()[2..10]; // Ignore the 0x
            let key = format!("{}-{}", four_byte, input_size);
            let count = four_byte_map.entry(key).or_insert(0);
            *count += 1;
        }
        four_byte_map = convert_call_trace_into_4byte_map(call_frame.calls, four_byte_map);
    }
    four_byte_map
}

fn create_trace_cache_opts() -> GethDebugTracingOptions {
    // Get the traces with call tracer onlytopcall false and withlog true and always cache this way
    let mut call_config_map = serde_json::Map::new();
    call_config_map.insert("only_top_call".to_string(), serde_json::Value::Bool(false));
    call_config_map.insert("with_log".to_string(), serde_json::Value::Bool(true));
    let call_config = serde_json::Value::Object(call_config_map);
    GethDebugTracingOptions {
        tracer: Some(GethDebugTracerType::BuiltInTracer(
            GethDebugBuiltInTracerType::CallTracer,
        )),
        tracer_config: GethDebugTracerConfig(call_config),
        ..Default::default()
    }
}
