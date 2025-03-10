use reth_primitives::revm::env::{fill_tx_env, fill_tx_env_with_recovered};
use reth_primitives::revm_primitives::TxEnv;
use reth_primitives::{TransactionSigned, TransactionSignedEcRecovered, TxHash, U256};
use reth_revm::tracing::{FourByteInspector, TracingInspector, TracingInspectorConfig};
use reth_rpc_types::trace::geth::{
    FourByteFrame, GethDebugBuiltInTracerType, GethDebugTracerType, GethDebugTracingOptions,
    GethTrace, NoopFrame,
};
use revm::precompile::{PrecompileSpecId, Precompiles};
use revm::primitives::db::Database;
use revm::primitives::{Address, BlockEnv, CfgEnvWithHandlerCfg, EVMError, ResultAndState, SpecId};
use revm::{inspector_handle_register, Inspector};

use crate::error::rpc::{EthApiError, EthResult};
use crate::evm::db::EvmDb;
use crate::RpcInvalidTransactionError;

pub(crate) fn trace_transaction<C: sov_modules_api::Context>(
    opts: GethDebugTracingOptions,
    config_env: CfgEnvWithHandlerCfg,
    block_env: BlockEnv,
    tx_env: TxEnv,
    db: &mut EvmDb<'_, C>,
) -> EthResult<(GethTrace, revm::primitives::State)> {
    let GethDebugTracingOptions {
        config,
        tracer,
        tracer_config,
        ..
    } = opts;

    if let Some(tracer) = tracer {
        return match tracer {
            GethDebugTracerType::BuiltInTracer(tracer) => match tracer {
                GethDebugBuiltInTracerType::FourByteTracer => {
                    let mut inspector = FourByteInspector::default();
                    let res = inspect(db, config_env, block_env, tx_env, &mut inspector)?;
                    return Ok((FourByteFrame::from(inspector).into(), res.state));
                }
                GethDebugBuiltInTracerType::CallTracer => {
                    let call_config = tracer_config
                        .into_call_config()
                        .map_err(|_| EthApiError::InvalidTracerConfig)?;
                    let mut inspector = TracingInspector::new(
                        TracingInspectorConfig::from_geth_config(&config)
                            .set_record_logs(call_config.with_log.unwrap_or_default()),
                    );
                    let res = inspect(db, config_env, block_env, tx_env, &mut inspector)?;
                    let frame = inspector
                        .into_geth_builder()
                        .geth_call_traces(call_config, res.result.gas_used());
                    return Ok((frame.into(), res.state));
                }
                GethDebugBuiltInTracerType::PreStateTracer => {
                    // Requires DatabaseRef trait
                    // meaning we need a readonly state to implement this
                    return Err(EthApiError::Unsupported("PreStateTracer"));
                }
                GethDebugBuiltInTracerType::NoopTracer => {
                    Ok((NoopFrame::default().into(), Default::default()))
                }
                // TODO: either implement or return unsupported
                GethDebugBuiltInTracerType::MuxTracer => todo!("MuxTracer"),
            },
            GethDebugTracerType::JsTracer(_code) => {
                // This also requires DatabaseRef trait
                // Implement after readonly state is implemented
                return Err(EthApiError::Unsupported("JsTracer"));
            }
        };
    }

    // default structlog tracer
    let inspector_config = TracingInspectorConfig::from_geth_config(&config);

    let mut inspector = TracingInspector::new(inspector_config);

    let res = inspect(db, config_env, block_env, tx_env, &mut inspector)?;
    let gas_used = res.result.gas_used();
    let return_value = res.result.into_output().unwrap_or_default();
    let frame = inspector
        .into_geth_builder()
        .geth_traces(gas_used, return_value, config);

    Ok((frame.into(), res.state))
}

/// Executes the [Env] against the given [Database] without committing state changes.
pub(crate) fn inspect<DB, I>(
    db: DB,
    config_env: CfgEnvWithHandlerCfg,
    block_env: BlockEnv,
    tx_env: TxEnv,
    inspector: I,
) -> Result<ResultAndState, EVMError<DB::Error>>
where
    DB: Database,
    <DB as Database>::Error: Into<EthApiError>,
    I: Inspector<DB>,
{
    let mut evm = revm::Evm::builder()
        .with_db(db)
        .with_external_context(inspector)
        .with_cfg_env_with_handler_cfg(config_env)
        .with_block_env(block_env)
        .with_tx_env(tx_env)
        .append_handler_register(inspector_handle_register)
        .build();

    evm.transact()
}

/// Taken from reth
/// https://github.com/paradigmxyz/reth/blob/606640285e763b64519213bad34c76fe4d24652f/crates/rpc/rpc/src/eth/revm_utils.rs#L69
/// Helper type to work with different transaction types when configuring the EVM env.
///
/// This makes it easier to handle errors.
pub(crate) trait FillableTransaction {
    /// Returns the hash of the transaction.
    fn hash(&self) -> TxHash;

    /// Fill the transaction environment with the given transaction.
    fn try_fill_tx_env(&self, tx_env: &mut TxEnv) -> EthResult<()>;
}

impl FillableTransaction for TransactionSignedEcRecovered {
    fn hash(&self) -> TxHash {
        self.hash
    }

    fn try_fill_tx_env(&self, tx_env: &mut TxEnv) -> EthResult<()> {
        fill_tx_env_with_recovered(tx_env, self);
        Ok(())
    }
}
impl FillableTransaction for TransactionSigned {
    fn hash(&self) -> TxHash {
        self.hash
    }

    fn try_fill_tx_env(&self, tx_env: &mut TxEnv) -> EthResult<()> {
        let signer = self
            .recover_signer()
            .ok_or_else(|| EthApiError::InvalidTransactionSignature)?;
        fill_tx_env(tx_env, self, signer);
        Ok(())
    }
}

/// https://github.com/paradigmxyz/reth/blob/332e412a0f8d34ff2bbb7e07921f8cacdcf69d64/crates/rpc/rpc/src/eth/revm_utils.rs#L385
/// Caps the configured [TxEnv] `gas_limit` with the allowance of the caller.
pub(crate) fn cap_tx_gas_limit_with_caller_allowance<DB>(db: DB, env: &mut TxEnv) -> EthResult<()>
where
    DB: Database,
    EthApiError: From<<DB as Database>::Error>,
{
    if let Ok(gas_limit) = caller_gas_allowance(db, env)?.try_into() {
        env.gas_limit = gas_limit;
    }

    Ok(())
}

/// https://github.com/paradigmxyz/reth/blob/332e412a0f8d34ff2bbb7e07921f8cacdcf69d64/crates/rpc/rpc/src/eth/revm_utils.rs#L403
/// Calculates the caller gas allowance.
///
/// `allowance = (account.balance - tx.value) / tx.gas_price`
///
/// Returns an error if the caller has insufficient funds.
/// Caution: This assumes non-zero `env.gas_price`. Otherwise, zero allowance will be returned.
pub(crate) fn caller_gas_allowance<DB>(mut db: DB, env: &TxEnv) -> EthResult<U256>
where
    DB: Database,
    EthApiError: From<<DB as Database>::Error>,
{
    Ok(db
        // Get the caller account.
        .basic(env.caller)?
        // Get the caller balance.
        .map(|acc| acc.balance)
        .unwrap_or_default()
        // Subtract transferred value from the caller balance.
        .checked_sub(env.value)
        // Return error if the caller has insufficient funds.
        .ok_or_else(|| RpcInvalidTransactionError::InsufficientFunds)?
        // Calculate the amount of gas the caller can afford with the specified gas price.
        .checked_div(env.gas_price)
        // This will be 0 if gas price is 0. It is fine, because we check it before.
        .unwrap_or_default())
}

/// Returns the addresses of the precompiles corresponding to the SpecId.
#[inline]
pub(crate) fn get_precompiles(spec_id: SpecId) -> impl IntoIterator<Item = Address> {
    let spec = PrecompileSpecId::from_spec_id(spec_id);
    Precompiles::new(spec)
        .addresses()
        .copied()
        .map(Address::from)
}
