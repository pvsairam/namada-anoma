//! The ledger's protocol
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fmt::Debug;

use borsh_ext::BorshSerializeExt;
use eyre::{eyre, WrapErr};
use masp_primitives::transaction::Transaction;
use namada_core::booleans::BoolResultUnitExt;
use namada_core::hash::Hash;
use namada_core::storage::Key;
use namada_gas::TxGasMeter;
use namada_sdk::tx::TX_TRANSFER_WASM;
use namada_state::StorageWrite;
use namada_tx::data::protocol::ProtocolTxType;
use namada_tx::data::{
    GasLimit, TxResult, TxType, VpStatusFlags, VpsResult, WrapperTx,
};
use namada_tx::{Section, Tx};
use namada_vote_ext::EthereumTxData;
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use thiserror::Error;

use crate::address::{Address, InternalAddress};
use crate::ledger::gas::{GasMetering, VpGasMeter};
use crate::ledger::governance::GovernanceVp;
use crate::ledger::native_vp::ethereum_bridge::bridge_pool_vp::BridgePoolVp;
use crate::ledger::native_vp::ethereum_bridge::nut::NonUsableTokens;
use crate::ledger::native_vp::ethereum_bridge::vp::EthBridge;
use crate::ledger::native_vp::ibc::Ibc;
use crate::ledger::native_vp::masp::MaspVp;
use crate::ledger::native_vp::multitoken::MultitokenVp;
use crate::ledger::native_vp::parameters::{self, ParametersVp};
use crate::ledger::native_vp::{self, NativeVp};
use crate::ledger::pgf::PgfVp;
use crate::ledger::pos::{self, PosVP};
use crate::state::{DBIter, State, StorageHasher, StorageRead, WlState, DB};
use crate::storage;
use crate::storage::TxIndex;
use crate::token::Amount;
use crate::vm::wasm::{TxCache, VpCache};
use crate::vm::{self, wasm, WasmCacheAccess};

#[allow(missing_docs)]
#[derive(Error, Debug)]
pub enum Error {
    #[error("Missing tx section: {0}")]
    MissingSection(String),
    #[error("State error: {0}")]
    StateError(namada_state::Error),
    #[error("Storage error: {0}")]
    StorageError(namada_state::StorageError),
    #[error("Wrapper tx runner error: {0}")]
    WrapperRunnerError(String),
    #[error("Transaction runner error: {0}")]
    TxRunnerError(vm::wasm::run::Error),
    #[error("{0:?}")]
    ProtocolTxError(#[from] eyre::Error),
    #[error("Txs must either be encrypted or a decryption of an encrypted tx")]
    TxTypeError,
    #[error("Fee ushielding error: {0}")]
    FeeUnshieldingError(namada_tx::data::WrapperTxErr),
    #[error("Gas error: {0}")]
    GasError(String),
    #[error("Error while processing transaction's fees: {0}")]
    FeeError(String),
    #[error("Invalid transaction section signature: {0}")]
    InvalidSectionSignature(String),
    #[error(
        "The decrypted transaction {0} has already been applied in this block"
    )]
    ReplayAttempt(Hash),
    #[error("Error executing VP for addresses: {0:?}")]
    VpRunnerError(vm::wasm::run::Error),
    #[error("The address {0} doesn't exist")]
    MissingAddress(Address),
    #[error("IBC native VP: {0}")]
    IbcNativeVpError(crate::ledger::native_vp::ibc::Error),
    #[error("PoS native VP: {0}")]
    PosNativeVpError(pos::vp::Error),
    #[error("PoS native VP panicked")]
    PosNativeVpRuntime,
    #[error("Parameters native VP: {0}")]
    ParametersNativeVpError(parameters::Error),
    #[error("IBC Token native VP: {0}")]
    MultitokenNativeVpError(crate::ledger::native_vp::multitoken::Error),
    #[error("Governance native VP error: {0}")]
    GovernanceNativeVpError(crate::ledger::governance::Error),
    #[error("Pgf native VP error: {0}")]
    PgfNativeVpError(crate::ledger::pgf::Error),
    #[error("Ethereum bridge native VP error: {0:?}")]
    EthBridgeNativeVpError(native_vp::ethereum_bridge::vp::Error),
    #[error("Ethereum bridge pool native VP error: {0:?}")]
    BridgePoolNativeVpError(native_vp::ethereum_bridge::bridge_pool_vp::Error),
    #[error("Non usable tokens native VP error: {0:?}")]
    NutNativeVpError(native_vp::ethereum_bridge::nut::Error),
    #[error("MASP native VP error: {0}")]
    MaspNativeVpError(native_vp::masp::Error),
    #[error("Access to an internal address {0:?} is forbidden")]
    AccessForbidden(InternalAddress),
}

impl Error {
    /// Determine if the error originates from an invalid transaction
    /// section signature. This is required for replay protection.
    const fn invalid_section_signature_flag(&self) -> VpStatusFlags {
        if matches!(self, Self::InvalidSectionSignature(_)) {
            VpStatusFlags::INVALID_SIGNATURE
        } else {
            VpStatusFlags::empty()
        }
    }
}

/// Shell parameters for running wasm transactions.
#[allow(missing_docs)]
#[derive(Debug)]
pub struct ShellParams<'a, S, D, H, CA>
where
    S: State<D = D, H = H> + Sync,
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
    CA: 'static + WasmCacheAccess + Sync,
{
    pub tx_gas_meter: &'a RefCell<TxGasMeter>,
    pub state: &'a mut S,
    pub vp_wasm_cache: &'a mut VpCache<CA>,
    pub tx_wasm_cache: &'a mut TxCache<CA>,
}

impl<'a, S, D, H, CA> ShellParams<'a, S, D, H, CA>
where
    S: State<D = D, H = H> + Sync,
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
    CA: 'static + WasmCacheAccess + Sync,
{
    /// Create a new instance of `ShellParams`
    pub fn new(
        tx_gas_meter: &'a RefCell<TxGasMeter>,
        state: &'a mut S,
        vp_wasm_cache: &'a mut VpCache<CA>,
        tx_wasm_cache: &'a mut TxCache<CA>,
    ) -> Self {
        Self {
            tx_gas_meter,
            state,
            vp_wasm_cache,
            tx_wasm_cache,
        }
    }
}

/// Result of applying a transaction
pub type Result<T> = std::result::Result<T, Error>;

/// Arguments needed to execute a Wrapper transaction
pub struct WrapperArgs<'a> {
    /// The block proposer for the current block
    pub block_proposer: &'a Address,
    /// Flag if the wrapper transaction committed the fee unshielding operation
    pub is_committed_fee_unshield: bool,
}

/// Dispatch a given transaction to be applied based on its type. Some storage
/// updates may be derived and applied natively rather than via the wasm
/// environment, in which case validity predicates will be bypassed.
///
/// If the given tx is a successfully decrypted payload apply the necessary
/// vps. Otherwise, we include the tx on chain with the gas charge added
/// but no further validations.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_tx<'a, D, H, CA>(
    tx: Tx,
    tx_bytes: &'a [u8],
    tx_index: TxIndex,
    tx_gas_meter: &'a RefCell<TxGasMeter>,
    state: &'a mut WlState<D, H>,
    vp_wasm_cache: &'a mut VpCache<CA>,
    tx_wasm_cache: &'a mut TxCache<CA>,
    wrapper_args: Option<&mut WrapperArgs>,
) -> Result<TxResult>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
    CA: 'static + WasmCacheAccess + Sync,
{
    match tx.header().tx_type {
        // Raw trasaction type is allowed only for governance proposals
        TxType::Raw => apply_wasm_tx(
            tx,
            &tx_index,
            ShellParams {
                tx_gas_meter,
                state,
                vp_wasm_cache,
                tx_wasm_cache,
            },
        ),
        TxType::Protocol(protocol_tx) => {
            apply_protocol_tx(protocol_tx.tx, tx.data(), state)
        }
        TxType::Wrapper(ref wrapper) => {
            let fee_unshielding_transaction =
                get_fee_unshielding_transaction(&tx, wrapper);
            let changed_keys = apply_wrapper_tx(
                tx.clone(),
                wrapper,
                fee_unshielding_transaction,
                tx_bytes,
                ShellParams {
                    tx_gas_meter,
                    state,
                    vp_wasm_cache,
                    tx_wasm_cache,
                },
                wrapper_args,
            )
            .map_err(|e| Error::WrapperRunnerError(e.to_string()))?;
            let mut inner_res = apply_wasm_tx(
                tx,
                &tx_index,
                ShellParams {
                    tx_gas_meter,
                    state,
                    vp_wasm_cache,
                    tx_wasm_cache,
                },
            )?;

            inner_res.wrapper_changed_keys = changed_keys;
            Ok(inner_res)
        }
    }
}

/// Load the wasm hash for a transfer from storage.
///
/// # Panics
/// If the transaction hash is not found in storage
pub fn get_transfer_hash_from_storage<S>(storage: &S) -> Hash
where
    S: StorageRead,
{
    let transfer_code_name_key =
        Key::wasm_code_name(TX_TRANSFER_WASM.to_string());
    storage
        .read(&transfer_code_name_key)
        .expect("Could not read the storage")
        .expect("Expected tx transfer hash in storage")
}

/// Performs the required operation on a wrapper transaction:
///  - replay protection
///  - fee payment
///  - gas accounting
///
/// Returns the set of changed storage keys.
pub(crate) fn apply_wrapper_tx<S, D, H, CA>(
    tx: Tx,
    wrapper: &WrapperTx,
    fee_unshield_transaction: Option<Transaction>,
    tx_bytes: &[u8],
    mut shell_params: ShellParams<'_, S, D, H, CA>,
    wrapper_args: Option<&mut WrapperArgs>,
) -> Result<BTreeSet<Key>>
where
    S: State<D = D, H = H> + Sync,
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
    CA: 'static + WasmCacheAccess + Sync,
{
    let mut changed_keys = BTreeSet::default();

    // Write wrapper tx hash to storage
    shell_params
        .state
        .write_log_mut()
        .write_tx_hash(tx.header_hash())
        .expect("Error while writing tx hash to storage");

    // Charge fee before performing any fallible operations
    charge_fee(
        wrapper,
        fee_unshield_transaction,
        &mut shell_params,
        &mut changed_keys,
        wrapper_args,
    )?;

    // Account for gas
    shell_params
        .tx_gas_meter
        .borrow_mut()
        .add_wrapper_gas(tx_bytes)
        .map_err(|err| Error::GasError(err.to_string()))?;

    Ok(changed_keys)
}

/// Retrieve the Masp `Transaction` for fee unshielding from the provided
/// transaction, if present
pub fn get_fee_unshielding_transaction(
    tx: &Tx,
    wrapper: &WrapperTx,
) -> Option<Transaction> {
    wrapper
        .unshield_section_hash
        .and_then(|ref hash| tx.get_section(hash))
        .and_then(|section| {
            if let Section::MaspTx(transaction) = section.as_ref() {
                Some(transaction.to_owned())
            } else {
                None
            }
        })
}

/// Charge fee for the provided wrapper transaction. Returns error if:
/// - The unshielding fails because of gas (other errors are ignored cause we
///   still try to get the fees amount from the transparent balance and, if it
///   works, execution can continue)
/// - Fee amount overflows
/// - Not enough funds are available to pay the entire amount of the fee
/// - The accumulated fee amount to be credited to the block proposer overflows
fn charge_fee<S, D, H, CA>(
    wrapper: &WrapperTx,
    masp_transaction: Option<Transaction>,
    shell_params: &mut ShellParams<'_, S, D, H, CA>,
    changed_keys: &mut BTreeSet<Key>,
    wrapper_args: Option<&mut WrapperArgs>,
) -> Result<()>
where
    S: State<D = D, H = H> + Sync,
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
    CA: 'static + WasmCacheAccess + Sync,
{
    // Unshield funds if requested
    let valid_fee_unshielding = if let Some(transaction) = masp_transaction {
        run_fee_unshielding(wrapper, shell_params, transaction)
    } else {
        Ok(false)
    };

    // Charge or check fees before propagating any possible error coming from
    // the fee unshielding. If fee unshielding failed for non-gas reasons but
    // the fees can still be paid we'll continue with the execution (this is a
    // different logic from the one we apply in process_proposal)
    match wrapper_args {
        Some(WrapperArgs {
            block_proposer,
            is_committed_fee_unshield: _,
        }) => transfer_fee(shell_params.state, block_proposer, wrapper)?,
        None => check_fees(shell_params.state, wrapper)?,
    }

    changed_keys
        .extend(shell_params.state.write_log_mut().get_keys_with_precommit());

    // Commit tx write log even in case of subsequent errors
    shell_params.state.write_log_mut().commit_tx();

    // Update the flag only after the valid fee payment has been committed. If
    // fee unshielding went out of gas propagate the error
    if let Some(args) = wrapper_args {
        args.is_committed_fee_unshield = valid_fee_unshielding?;
    }

    Ok(())
}

/// Executes the masp fee unshielding transaction. Returns `true if the unshield
/// was successful, `false` otherwise and error in case of out-of-gas
pub fn run_fee_unshielding<S, D, H, CA>(
    wrapper: &WrapperTx,
    shell_params: &mut ShellParams<'_, S, D, H, CA>,
    transaction: Transaction,
) -> Result<bool>
where
    S: State<D = D, H = H> + Sync,
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
    CA: 'static + WasmCacheAccess + Sync,
{
    let ShellParams {
        tx_gas_meter,
        state,
        vp_wasm_cache,
        tx_wasm_cache,
    } = shell_params;

    // The unshielding is subject to a gas limit imposed by a protocol
    // parameter, instantiate a custom gas meter for this step and
    // initialize it with the already consumed gas. The gas limit should
    // actually be the lowest between the protocol parameter and the actual gas
    // limit of the transaction
    let min_gas_limit = state
        .read::<u64>(
            &namada_parameters::storage::get_fee_unshielding_gas_limit_key(),
        )
        .expect("Error reading the storage")
        .expect("Missing fee unshielding gas limit in storage")
        .min(tx_gas_meter.borrow().tx_gas_limit.into());
    let mut unshield_gas_meter = TxGasMeter::new(GasLimit::from(min_gas_limit));
    unshield_gas_meter
        .copy_consumed_gas_from(&tx_gas_meter.borrow())
        .map_err(|e| Error::GasError(e.to_string()))?;
    let ref_unshield_gas_meter = RefCell::new(unshield_gas_meter);

    let result = match wrapper.generate_fee_unshielding(
        get_transfer_hash_from_storage(*state),
        Some(TX_TRANSFER_WASM.to_string()),
        transaction,
    ) {
        Ok(fee_unshielding_tx) => {
            // NOTE: A clean tx write log must be provided to this call
            // for a correct vp validation. Block write log, instead,
            // should contain any prior changes (if any). This is to simulate
            // the unshielding tx (to prevent the already written
            // keys from being passed/triggering VPs) but we cannot
            // commit the tx write log yet cause the tx could still
            // be invalid.
            state.write_log_mut().precommit_tx();
            match apply_wasm_tx(
                fee_unshielding_tx,
                &TxIndex::default(),
                ShellParams {
                    tx_gas_meter: &ref_unshield_gas_meter,
                    state: *state,
                    vp_wasm_cache,
                    tx_wasm_cache,
                },
            ) {
                Ok(result) => {
                    // NOTE: do not commit yet cause this could be
                    // exploited to get free unshieldings and shielded
                    // operations
                    if !result.is_accepted() {
                        state.write_log_mut().drop_tx_keep_precommit();
                        tracing::error!(
                            "The unshielding tx is invalid, some VPs rejected \
                             it: {:#?}",
                            result.vps_result.rejected_vps
                        );
                    }

                    result.is_accepted()
                }
                Err(e) => {
                    state.write_log_mut().drop_tx_keep_precommit();
                    tracing::error!(
                        "The unshielding tx is invalid, wasm run failed: {}",
                        e
                    );
                    if let Error::GasError(_) = e {
                        // Popagate only if it is a gas error
                        return Err(e);
                    }

                    false
                }
            }
        }
        Err(e) => {
            tracing::error!("{}", e);
            false
        }
    };

    tx_gas_meter
        .borrow_mut()
        .copy_consumed_gas_from(&ref_unshield_gas_meter.borrow())
        .map_err(|e| Error::GasError(e.to_string()))?;

    Ok(result)
}

/// Perform the actual transfer of fess from the fee payer to the block
/// proposer.
pub fn transfer_fee<S>(
    state: &mut S,
    block_proposer: &Address,
    wrapper: &WrapperTx,
) -> Result<()>
where
    S: State + StorageRead + StorageWrite,
{
    let balance = crate::token::read_balance(
        state,
        &wrapper.fee.token,
        &wrapper.fee_payer(),
    )
    .unwrap();

    match wrapper.get_tx_fee() {
        Ok(fees) => {
            let fees =
                crate::token::denom_to_amount(fees, &wrapper.fee.token, state)
                    .map_err(|e| Error::FeeError(e.to_string()))?;
            if balance.checked_sub(fees).is_some() {
                token_transfer(
                    state,
                    &wrapper.fee.token,
                    &wrapper.fee_payer(),
                    block_proposer,
                    fees,
                )
                .map_err(|e| Error::FeeError(e.to_string()))
            } else {
                // Balance was insufficient for fee payment, move all the
                // available funds in the transparent balance of
                // the fee payer. This shouldn't happen as it should be
                // prevented from mempool/process_proposal.
                tracing::error!(
                    "Transfer of tx fee cannot be applied to due to \
                     insufficient funds. Falling back to transferring the \
                     available balance which is less than the fee. This \
                     shouldn't happen."
                );
                token_transfer(
                    state,
                    &wrapper.fee.token,
                    &wrapper.fee_payer(),
                    block_proposer,
                    balance,
                )
                .map_err(|e| Error::FeeError(e.to_string()))?;

                Err(Error::FeeError(
                    "Transparent balance of wrapper's signer was insufficient \
                     to pay fee. All the available transparent funds have \
                     been moved to the block proposer"
                        .to_string(),
                ))
            }
        }
        Err(e) => {
            // Fee overflow. This shouldn't happen as it should be prevented
            // from mempool/process_proposal.
            tracing::error!(
                "Transfer of tx fee cannot be applied to due to fee overflow. \
                 This shouldn't happen."
            );

            Err(Error::FeeError(format!("{}", e)))
        }
    }
}

/// Transfer `token` from `src` to `dest`. Returns an `Err` if `src` has
/// insufficient balance or if the transfer the `dest` would overflow (This can
/// only happen if the total supply doesn't fit in `token::Amount`). Contrary to
/// `crate::token::transfer` this function updates the tx write log and
/// not the block write log.
fn token_transfer<WLS>(
    state: &mut WLS,
    token: &Address,
    src: &Address,
    dest: &Address,
    amount: Amount,
) -> Result<()>
where
    WLS: State + StorageRead,
{
    let src_key = crate::token::storage_key::balance_key(token, src);
    let src_balance = crate::token::read_balance(state, token, src)
        .expect("Token balance read in protocol must not fail");
    match src_balance.checked_sub(amount) {
        Some(new_src_balance) => {
            if src == dest {
                return Ok(());
            }
            let dest_key = crate::token::storage_key::balance_key(token, dest);
            let dest_balance = crate::token::read_balance(state, token, dest)
                .expect("Token balance read in protocol must not fail");
            match dest_balance.checked_add(amount) {
                Some(new_dest_balance) => {
                    state
                        .write_log_mut()
                        .write(&src_key, new_src_balance.serialize_to_vec())
                        .map_err(|e| Error::FeeError(e.to_string()))?;
                    match state
                        .write_log_mut()
                        .write(&dest_key, new_dest_balance.serialize_to_vec())
                    {
                        Ok(_) => Ok(()),
                        Err(e) => Err(Error::FeeError(e.to_string())),
                    }
                }
                None => Err(Error::FeeError(
                    "The transfer would overflow destination balance"
                        .to_string(),
                )),
            }
        }
        None => Err(Error::FeeError("Insufficient source balance".to_string())),
    }
}

/// Check if the fee payer has enough transparent balance to pay fees
pub fn check_fees<S>(state: &S, wrapper: &WrapperTx) -> Result<()>
where
    S: State + StorageRead,
{
    let balance = crate::token::read_balance(
        state,
        &wrapper.fee.token,
        &wrapper.fee_payer(),
    )
    .unwrap();

    let fees = wrapper
        .get_tx_fee()
        .map_err(|e| Error::FeeError(e.to_string()))?;

    let fees = crate::token::denom_to_amount(fees, &wrapper.fee.token, state)
        .map_err(|e| Error::FeeError(e.to_string()))?;
    if balance.checked_sub(fees).is_some() {
        Ok(())
    } else {
        Err(Error::FeeError(
            "Insufficient transparent balance to pay fees".to_string(),
        ))
    }
}

/// Apply a transaction going via the wasm environment. Gas will be metered and
/// validity predicates will be triggered in the normal way.
pub fn apply_wasm_tx<'a, S, D, H, CA>(
    tx: Tx,
    tx_index: &TxIndex,
    shell_params: ShellParams<'a, S, D, H, CA>,
) -> Result<TxResult>
where
    S: State<D = D, H = H> + Sync,
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
    CA: 'static + WasmCacheAccess + Sync,
{
    let ShellParams {
        tx_gas_meter,
        state,
        vp_wasm_cache,
        tx_wasm_cache,
    } = shell_params;

    let tx_hash = tx.raw_header_hash();
    if state.write_log().has_replay_protection_entry(&tx_hash) {
        // If the same transaction has already been applied in this block, skip
        // execution and return
        return Err(Error::ReplayAttempt(tx_hash));
    }

    let verifiers = execute_tx(
        &tx,
        tx_index,
        state,
        tx_gas_meter,
        vp_wasm_cache,
        tx_wasm_cache,
    )?;

    let vps_result = check_vps(CheckVps {
        tx: &tx,
        tx_index,
        state,
        tx_gas_meter: &mut tx_gas_meter.borrow_mut(),
        verifiers_from_tx: &verifiers,
        vp_wasm_cache,
    })?;

    let gas_used = tx_gas_meter.borrow().get_tx_consumed_gas();
    let initialized_accounts = state.write_log().get_initialized_accounts();
    let changed_keys = state.write_log().get_keys();
    let ibc_events = state.write_log_mut().take_ibc_events();

    Ok(TxResult {
        gas_used,
        wrapper_changed_keys: Default::default(),
        changed_keys,
        vps_result,
        initialized_accounts,
        ibc_events,
        eth_bridge_events: BTreeSet::default(),
    })
}

/// Apply a derived transaction to storage based on some protocol transaction.
/// The logic here must be completely deterministic and will be executed by all
/// full nodes every time a protocol transaction is included in a block. Storage
/// is updated natively rather than via the wasm environment, so gas does not
/// need to be metered and validity predicates are bypassed. A [`TxResult`]
/// containing changed keys and the like should be returned in the normal way.
pub(crate) fn apply_protocol_tx<D, H>(
    tx: ProtocolTxType,
    data: Option<Vec<u8>>,
    state: &mut WlState<D, H>,
) -> Result<TxResult>
where
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
{
    use namada_ethereum_bridge::protocol::transactions;
    use namada_vote_ext::{ethereum_events, validator_set_update};

    let Some(data) = data else {
        return Err(Error::ProtocolTxError(eyre!(
            "Protocol tx data must be present"
        )));
    };
    let ethereum_tx_data = EthereumTxData::deserialize(&tx, &data)
        .wrap_err_with(|| {
            format!(
                "Attempt made to apply an unsupported protocol transaction! - \
                 {tx:?}",
            )
        })
        .map_err(Error::ProtocolTxError)?;

    match ethereum_tx_data {
        EthereumTxData::EthEventsVext(
            namada_vote_ext::ethereum_events::SignedVext(ext),
        ) => {
            let ethereum_events::VextDigest { events, .. } =
                ethereum_events::VextDigest::singleton(ext);
            transactions::ethereum_events::apply_derived_tx(state, events)
                .map_err(Error::ProtocolTxError)
        }
        EthereumTxData::BridgePoolVext(ext) => {
            transactions::bridge_pool_roots::apply_derived_tx(state, ext.into())
                .map_err(Error::ProtocolTxError)
        }
        EthereumTxData::ValSetUpdateVext(ext) => {
            // NOTE(feature = "abcipp"): with ABCI++, we can write the
            // complete proof to storage in one go. the decided vote extension
            // digest must already have >2/3 of the voting power behind it.
            // with ABCI+, multiple vote extension protocol txs may be needed
            // to reach a complete proof.
            let signing_epoch = ext.data.signing_epoch;
            transactions::validator_set_update::aggregate_votes(
                state,
                validator_set_update::VextDigest::singleton(ext),
                signing_epoch,
            )
            .map_err(Error::ProtocolTxError)
        }
        EthereumTxData::EthereumEvents(_)
        | EthereumTxData::BridgePool(_)
        | EthereumTxData::ValidatorSetUpdate(_) => {
            // TODO(namada#198): implement this
            tracing::warn!(
                "Attempt made to apply an unimplemented protocol transaction, \
                 no actions will be taken"
            );
            Ok(TxResult::default())
        }
    }
}

/// Execute a transaction code. Returns verifiers requested by the transaction.
#[allow(clippy::too_many_arguments)]
fn execute_tx<S, D, H, CA>(
    tx: &Tx,
    tx_index: &TxIndex,
    state: &mut S,
    tx_gas_meter: &RefCell<TxGasMeter>,
    vp_wasm_cache: &mut VpCache<CA>,
    tx_wasm_cache: &mut TxCache<CA>,
) -> Result<BTreeSet<Address>>
where
    S: State<D = D, H = H>,
    D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
    H: 'static + StorageHasher + Sync,
    CA: 'static + WasmCacheAccess + Sync,
{
    wasm::run::tx(
        state,
        tx_gas_meter,
        tx_index,
        tx,
        vp_wasm_cache,
        tx_wasm_cache,
    )
    .map_err(|err| match err {
        wasm::run::Error::GasError(msg) => Error::GasError(msg),
        wasm::run::Error::MissingSection(msg) => Error::MissingSection(msg),
        _ => Error::TxRunnerError(err),
    })
}

/// Arguments to [`check_vps`].
struct CheckVps<'a, S, CA>
where
    S: State,
    CA: 'static + WasmCacheAccess + Sync,
{
    tx: &'a Tx,
    tx_index: &'a TxIndex,
    state: &'a S,
    tx_gas_meter: &'a mut TxGasMeter,
    verifiers_from_tx: &'a BTreeSet<Address>,
    vp_wasm_cache: &'a mut VpCache<CA>,
}

/// Check the acceptance of a transaction by validity predicates
fn check_vps<S, CA>(
    CheckVps {
        tx,
        tx_index,
        state,
        tx_gas_meter,
        verifiers_from_tx,
        vp_wasm_cache,
    }: CheckVps<'_, S, CA>,
) -> Result<VpsResult>
where
    S: State + Sync,
    CA: 'static + WasmCacheAccess + Sync,
{
    let (verifiers, keys_changed) = state
        .write_log()
        .verifiers_and_changed_keys(verifiers_from_tx);

    let vps_result = execute_vps(
        verifiers,
        keys_changed,
        tx,
        tx_index,
        state,
        tx_gas_meter,
        vp_wasm_cache,
    )?;
    tracing::debug!("Total VPs gas cost {:?}", vps_result.gas_used);

    tx_gas_meter
        .add_vps_gas(&vps_result.gas_used)
        .map_err(|err| Error::GasError(err.to_string()))?;

    Ok(vps_result)
}

/// Execute verifiers' validity predicates
#[allow(clippy::too_many_arguments)]
fn execute_vps<S, CA>(
    verifiers: BTreeSet<Address>,
    keys_changed: BTreeSet<storage::Key>,
    tx: &Tx,
    tx_index: &TxIndex,
    state: &S,
    tx_gas_meter: &TxGasMeter,
    vp_wasm_cache: &mut VpCache<CA>,
) -> Result<VpsResult>
where
    S: State + Sync,
    CA: 'static + WasmCacheAccess + Sync,
{
    let vps_result = verifiers
        .par_iter()
        .try_fold(VpsResult::default, |mut result, addr| {
            let gas_meter =
                RefCell::new(VpGasMeter::new_from_tx_meter(tx_gas_meter));
            let tx_accepted = match &addr {
                Address::Implicit(_) | Address::Established(_) => {
                    let (vp_hash, gas) = state
                        .validity_predicate(addr)
                        .map_err(Error::StateError)?;
                    gas_meter
                        .borrow_mut()
                        .consume(gas)
                        .map_err(|err| Error::GasError(err.to_string()))?;
                    let Some(vp_code_hash) = vp_hash else {
                        return Err(Error::MissingAddress(addr.clone()));
                    };

                    wasm::run::vp(
                        vp_code_hash,
                        tx,
                        tx_index,
                        addr,
                        state,
                        &gas_meter,
                        &keys_changed,
                        &verifiers,
                        vp_wasm_cache.clone(),
                    )
                    .map_err(|err| match err {
                        wasm::run::Error::GasError(msg) => Error::GasError(msg),
                        wasm::run::Error::InvalidSectionSignature(msg) => {
                            Error::InvalidSectionSignature(msg)
                        }
                        _ => Error::VpRunnerError(err),
                    })
                }
                Address::Internal(internal_addr) => {
                    let ctx = native_vp::Ctx::new(
                        addr,
                        state,
                        tx,
                        tx_index,
                        &gas_meter,
                        &keys_changed,
                        &verifiers,
                        vp_wasm_cache.clone(),
                    );

                    match internal_addr {
                        InternalAddress::PoS => {
                            let pos = PosVP { ctx };
                            pos.validate_tx(tx, &keys_changed, &verifiers)
                                .map_err(Error::PosNativeVpError)
                        }
                        InternalAddress::Ibc => {
                            let ibc = Ibc { ctx };
                            ibc.validate_tx(tx, &keys_changed, &verifiers)
                                .map_err(Error::IbcNativeVpError)
                        }
                        InternalAddress::Parameters => {
                            let parameters = ParametersVp { ctx };
                            parameters
                                .validate_tx(tx, &keys_changed, &verifiers)
                                .map_err(Error::ParametersNativeVpError)
                        }
                        InternalAddress::PosSlashPool => Err(
                            Error::AccessForbidden((*internal_addr).clone()),
                        ),
                        InternalAddress::Governance => {
                            let governance = GovernanceVp { ctx };
                            governance
                                .validate_tx(tx, &keys_changed, &verifiers)
                                .map_err(Error::GovernanceNativeVpError)
                        }
                        InternalAddress::Multitoken => {
                            let multitoken = MultitokenVp { ctx };
                            multitoken
                                .validate_tx(tx, &keys_changed, &verifiers)
                                .map_err(Error::MultitokenNativeVpError)
                        }
                        InternalAddress::EthBridge => {
                            let bridge = EthBridge { ctx };
                            bridge
                                .validate_tx(tx, &keys_changed, &verifiers)
                                .map_err(Error::EthBridgeNativeVpError)
                        }
                        InternalAddress::EthBridgePool => {
                            let bridge_pool = BridgePoolVp { ctx };
                            bridge_pool
                                .validate_tx(tx, &keys_changed, &verifiers)
                                .map_err(Error::BridgePoolNativeVpError)
                        }
                        InternalAddress::Pgf => {
                            let pgf_vp = PgfVp { ctx };
                            pgf_vp
                                .validate_tx(tx, &keys_changed, &verifiers)
                                .map_err(Error::PgfNativeVpError)
                        }
                        InternalAddress::Nut(_) => {
                            let non_usable_tokens = NonUsableTokens { ctx };
                            non_usable_tokens
                                .validate_tx(tx, &keys_changed, &verifiers)
                                .map_err(Error::NutNativeVpError)
                        }
                        internal_addr @ (InternalAddress::IbcToken(_)
                        | InternalAddress::Erc20(_)) => {
                            // The address should be a part of a multitoken
                            // key
                            verifiers
                                .contains(&Address::Internal(
                                    InternalAddress::Multitoken,
                                ))
                                .ok_or_else(|| {
                                    Error::AccessForbidden(
                                        internal_addr.clone(),
                                    )
                                })
                        }
                        InternalAddress::Masp => {
                            let masp = MaspVp { ctx };
                            masp.validate_tx(tx, &keys_changed, &verifiers)
                                .map_err(Error::MaspNativeVpError)
                        }
                        InternalAddress::TempStorage => Err(
                            // Temp storage changes must never be committed
                            Error::AccessForbidden((*internal_addr).clone()),
                        ),
                    }
                }
            };

            tx_accepted.map_or_else(
                |err| {
                    result
                        .status_flags
                        .insert(err.invalid_section_signature_flag());
                    result.rejected_vps.insert(addr.clone());
                    result.errors.push((addr.clone(), err.to_string()));
                },
                |()| {
                    result.accepted_vps.insert(addr.clone());
                },
            );

            // Execution of VPs can (and must) be short-circuited
            // only in case of a gas overflow to prevent the
            // transaction from consuming resources that have not
            // been acquired in the corresponding wrapper tx. For
            // all the other errors we keep evaluating the vps. This
            // allows to display a consistent VpsResult across all
            // nodes and find any invalid signatures
            result
                .gas_used
                .set(gas_meter.into_inner())
                .map_err(|err| Error::GasError(err.to_string()))?;

            Ok(result)
        })
        .try_reduce(VpsResult::default, |a, b| {
            merge_vp_results(a, b, tx_gas_meter)
        })?;

    Ok(vps_result)
}

/// Merge VP results from parallel runs
fn merge_vp_results(
    a: VpsResult,
    mut b: VpsResult,
    tx_gas_meter: &TxGasMeter,
) -> Result<VpsResult> {
    let mut accepted_vps = a.accepted_vps;
    let mut rejected_vps = a.rejected_vps;
    accepted_vps.extend(b.accepted_vps);
    rejected_vps.extend(b.rejected_vps);
    let mut errors = a.errors;
    errors.append(&mut b.errors);
    let status_flags = a.status_flags | b.status_flags;
    let mut gas_used = a.gas_used;

    gas_used
        .merge(b.gas_used, tx_gas_meter)
        .map_err(|err| Error::GasError(err.to_string()))?;

    Ok(VpsResult {
        accepted_vps,
        rejected_vps,
        gas_used,
        errors,
        status_flags,
    })
}

#[cfg(test)]
mod tests {
    use eyre::Result;
    use namada_core::collections::HashMap;
    use namada_core::ethereum_events::testing::DAI_ERC20_ETH_ADDRESS;
    use namada_core::ethereum_events::{EthereumEvent, TransferToNamada};
    use namada_core::keccak::keccak_hash;
    use namada_core::storage::BlockHeight;
    use namada_core::voting_power::FractionalVotingPower;
    use namada_core::{address, key};
    use namada_ethereum_bridge::protocol::transactions::votes::{
        EpochedVotingPower, Votes,
    };
    use namada_ethereum_bridge::storage::eth_bridge_queries::EthBridgeQueries;
    use namada_ethereum_bridge::storage::proof::EthereumProof;
    use namada_ethereum_bridge::storage::{vote_tallies, vp};
    use namada_ethereum_bridge::test_utils;
    use namada_tx::{SignableEthMessage, Signed};
    use namada_vote_ext::bridge_pool_roots::BridgePoolRootVext;
    use namada_vote_ext::ethereum_events::EthereumEventsVext;

    use super::*;

    fn apply_eth_tx<D, H>(
        tx: EthereumTxData,
        state: &mut WlState<D, H>,
    ) -> Result<TxResult>
    where
        D: 'static + DB + for<'iter> DBIter<'iter> + Sync,
        H: 'static + StorageHasher + Sync,
    {
        let (data, tx) = tx.serialize();
        let tx_result = apply_protocol_tx(tx, Some(data), state)?;
        Ok(tx_result)
    }

    #[test]
    /// Tests that if the same [`ProtocolTxType::EthEventsVext`] is applied
    /// twice within the same block, it doesn't result in voting power being
    /// double counted.
    fn test_apply_protocol_tx_duplicate_eth_events_vext() -> Result<()> {
        let validator_a = address::testing::established_address_2();
        let validator_b = address::testing::established_address_3();
        let validator_a_stake = Amount::native_whole(100);
        let validator_b_stake = Amount::native_whole(100);
        let total_stake = validator_a_stake + validator_b_stake;
        let (mut state, _) = test_utils::setup_storage_with_validators(
            HashMap::from_iter(vec![
                (validator_a.clone(), validator_a_stake),
                (validator_b, validator_b_stake),
            ]),
        );
        let event = EthereumEvent::TransfersToNamada {
            nonce: 0.into(),
            transfers: vec![TransferToNamada {
                amount: Amount::from(100),
                asset: DAI_ERC20_ETH_ADDRESS,
                receiver: address::testing::established_address_4(),
            }],
        };
        let vext = EthereumEventsVext {
            block_height: BlockHeight(100),
            validator_addr: address::testing::established_address_2(),
            ethereum_events: vec![event.clone()],
        };
        let signing_key = key::testing::keypair_1();
        let signed = vext.sign(&signing_key);
        let tx = EthereumTxData::EthEventsVext(
            namada_vote_ext::ethereum_events::SignedVext(signed),
        );

        apply_eth_tx(tx.clone(), &mut state)?;
        apply_eth_tx(tx, &mut state)?;

        let eth_msg_keys = vote_tallies::Keys::from(&event);
        let seen_by: Votes = state.read(&eth_msg_keys.seen_by())?.unwrap();
        assert_eq!(seen_by, Votes::from([(validator_a, BlockHeight(100))]));

        // the vote should have only be applied once
        let voting_power: EpochedVotingPower =
            state.read(&eth_msg_keys.voting_power())?.unwrap();
        let expected = EpochedVotingPower::from([(
            0.into(),
            FractionalVotingPower::HALF * total_stake,
        )]);
        assert_eq!(voting_power, expected);

        Ok(())
    }

    #[test]
    /// Tests that if the same [`ProtocolTxType::BridgePoolVext`] is applied
    /// twice within the same block, it doesn't result in voting power being
    /// double counted.
    fn test_apply_protocol_tx_duplicate_bp_roots_vext() -> Result<()> {
        let validator_a = address::testing::established_address_2();
        let validator_b = address::testing::established_address_3();
        let validator_a_stake = Amount::native_whole(100);
        let validator_b_stake = Amount::native_whole(100);
        let total_stake = validator_a_stake + validator_b_stake;
        let (mut state, keys) = test_utils::setup_storage_with_validators(
            HashMap::from_iter(vec![
                (validator_a.clone(), validator_a_stake),
                (validator_b, validator_b_stake),
            ]),
        );
        vp::bridge_pool::init_storage(&mut state);

        let root = state.ethbridge_queries().get_bridge_pool_root();
        let nonce = state.ethbridge_queries().get_bridge_pool_nonce();
        test_utils::commit_bridge_pool_root_at_height(
            &mut state,
            &root,
            100.into(),
        );
        let to_sign = keccak_hash([root.0, nonce.to_bytes()].concat());
        let signing_key = key::testing::keypair_1();
        let hot_key =
            &keys[&address::testing::established_address_2()].eth_bridge;
        let sig = Signed::<_, SignableEthMessage>::new(hot_key, to_sign).sig;
        let vext = BridgePoolRootVext {
            block_height: BlockHeight(100),
            validator_addr: address::testing::established_address_2(),
            sig,
        }
        .sign(&signing_key);
        let tx = EthereumTxData::BridgePoolVext(vext);
        apply_eth_tx(tx.clone(), &mut state)?;
        apply_eth_tx(tx, &mut state)?;

        let bp_root_keys = vote_tallies::Keys::from((
            &vote_tallies::BridgePoolRoot(EthereumProof::new((root, nonce))),
            100.into(),
        ));
        let root_seen_by: Votes = state.read(&bp_root_keys.seen_by())?.unwrap();
        assert_eq!(
            root_seen_by,
            Votes::from([(validator_a, BlockHeight(100))])
        );
        // the vote should have only be applied once
        let voting_power: EpochedVotingPower =
            state.read(&bp_root_keys.voting_power())?.unwrap();
        let expected = EpochedVotingPower::from([(
            0.into(),
            FractionalVotingPower::HALF * total_stake,
        )]);
        assert_eq!(voting_power, expected);

        Ok(())
    }

    #[test]
    fn test_native_vp_out_of_gas() {
        let (mut state, _validators) = test_utils::setup_default_storage();

        // some random token address
        let token_address = Address::Established([0xff; 20].into());

        let src_address = Address::Established([0xab; 20].into());
        let dst_address = Address::Established([0xba; 20].into());

        // supply an address with 1000 of said token
        namada_token::credit_tokens(
            &mut state,
            &token_address,
            &src_address,
            1000.into(),
        )
        .unwrap();

        // commit storage changes. this will act as the
        // initial state of the chain
        state.commit_tx();
        state.commit_block().unwrap();

        // "execute" a dummy tx, by manually performing its state changes
        let (dummy_tx, changed_keys, verifiers) = {
            let mut tx = Tx::from_type(TxType::Raw);
            tx.set_code(namada_tx::Code::new(vec![], None));
            tx.set_data(namada_tx::Data::new(vec![]));

            // transfer half of the supply of src to dst
            namada_token::transfer(
                &mut state,
                &token_address,
                &src_address,
                &dst_address,
                500.into(),
            )
            .unwrap();

            let changed_keys = {
                let mut set = BTreeSet::new();
                set.insert(namada_token::storage_key::balance_key(
                    &token_address,
                    &src_address,
                ));
                set.insert(namada_token::storage_key::balance_key(
                    &token_address,
                    &dst_address,
                ));
                set
            };

            let verifiers = {
                let mut set = BTreeSet::new();
                set.insert(Address::Internal(InternalAddress::Multitoken));
                set
            };

            (tx, changed_keys, verifiers)
        };

        // temp vp cache
        let (mut vp_cache, _) =
            wasm::compilation_cache::common::testing::cache();

        // gas meter with no gas left
        let gas_meter = TxGasMeter::new(0);

        let result = execute_vps(
            verifiers,
            changed_keys,
            &dummy_tx,
            &TxIndex::default(),
            &state,
            &gas_meter,
            &mut vp_cache,
        );
        assert!(matches!(result.unwrap_err(), Error::GasError(_)));
    }
}
