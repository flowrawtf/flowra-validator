/// Dependencies that are needed for the BAM (Jito Scheduler Service) to function.
/// All-in-one for convenience.
use std::sync::{Arc, atomic::AtomicU8};
use {
    crate::{
        banking_stage::{committer::CommitTransactionDetails, consumer::Consumer},
        bundle_stage::bundle_account_locker::BundleAccountLocker,
        proxy::block_engine_stage::BlockBuilderFeeInfo,
        tip_manager::{CrankChainResult, CrankStep, TipManagerError, TipManagers},
    },
    arc_swap::ArcSwap,
    jito_protos::proto::{
        bam_api::{SchedulerMessage, SchedulerMessageV0, scheduler_message::VersionedMsg},
        bam_types,
    },
    solana_clock::Slot,
    solana_gossip::cluster_info::ClusterInfo,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_transaction::sanitized::SanitizedTransaction,
    solana_transaction_error::TransactionError,
    solana_turbine::ShredReceiverAddresses,
    std::sync::RwLock,
    thiserror::Error,
    tokio::sync::mpsc,
};

pub enum BamOutboundMessage {
    AtomicTxnBatchResult(bam_types::AtomicTxnBatchResult),
    LeaderState(bam_types::LeaderState),
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BamConnectionState {
    Disconnected = 0,
    Connecting = 1,
    DrainingBlockEngine = 2,
    BlockEngineDrained = 3,
    Connected = 4,
}

impl BamConnectionState {
    pub fn from_u8(state: u8) -> Self {
        match state {
            1 => Self::Connecting,
            2 => Self::DrainingBlockEngine,
            3 => Self::BlockEngineDrained,
            4 => Self::Connected,
            _ => Self::Disconnected,
        }
    }
}

#[derive(Clone)]
pub struct BamDependencies {
    /// Also carries the internal Block Engine drain state during BAM cutover.
    pub bam_enabled: Arc<AtomicU8>,

    pub batch_sender: crossbeam_channel::Sender<bam_types::MultipleAtomicTxnBatch>,
    pub batch_receiver: crossbeam_channel::Receiver<bam_types::MultipleAtomicTxnBatch>,

    pub outbound_sender: mpsc::Sender<BamOutboundMessage>,

    pub cluster_info: Arc<ClusterInfo>,
    pub block_builder_fee_info: Arc<ArcSwap<BlockBuilderFeeInfo>>,
    pub bank_forks: Arc<RwLock<BankForks>>,
    pub bam_node_pubkey: Arc<ArcSwap<Pubkey>>,
    pub bam_tpu_info: Arc<ArcSwap<Option<(std::net::SocketAddr, std::net::SocketAddr)>>>,
    pub bam_shred_receiver_addresses: Arc<ArcSwap<ShredReceiverAddresses>>,
}

/// Why tip programs could not be prepared on a Bank.
#[derive(Clone, Debug, Error, PartialEq)]
pub(crate) enum TipProgramsError {
    #[error("tip bundle construction failed: {0}")]
    Construction(#[from] TipManagerError),
    #[error("tip bundle commit failed: {0}")]
    Commit(#[from] TransactionError),
}

#[derive(Clone)]
pub struct TipProcessingDependencies {
    pub tip_managers: TipManagers,
    pub block_builder_fee_info: Arc<ArcSwap<BlockBuilderFeeInfo>>,
    pub cluster_info: Arc<ClusterInfo>,
    pub bundle_account_locker: BundleAccountLocker,
}

impl TipProcessingDependencies {
    /// Prepare this Bank on the scheduler thread using the metadata snapshot it caches.
    /// Reports each distinct failure once per slot, remembering the last in `last_error`.
    pub(crate) fn process_tip_programs(
        &self,
        consumer: &Consumer,
        bank: &Arc<Bank>,
        builder: &BlockBuilderFeeInfo,
        last_error: &mut Option<(Slot, TipProgramsError)>,
    ) -> bool {
        let bank_key = (bank.slot(), bank.bank_id());
        // Match BundleStage's local-cluster policy: free-rent PDAs are discarded by AccountsDb.
        if bank.rent_collector().rent.minimum_balance(0) == 0 {
            return true;
        }
        if builder.block_builder == Pubkey::default() {
            return false;
        }
        let keypair = self.cluster_info.keypair();
        let mut fail = |error: TipProgramsError| {
            if last_error
                .as_ref()
                .is_some_and(|(slot, last)| *slot == bank.slot() && *last == error)
            {
                return false;
            }
            let balance = bank.get_balance(&self.cluster_info.id());
            datapoint_error!(
                "bam_scheduler-tip_programs_error",
                ("slot", bank.slot(), i64),
                ("error", error.to_string(), String),
                ("identity_balance", balance, i64),
            );
            *last_error = Some((bank.slot(), error));
            false
        };
        // Execute one crank stage. `Err(None)` is a retry without a cause to report.
        let process = |bundle: &[RuntimeTransaction<SanitizedTransaction>]| -> std::result::Result<
            (),
            Option<TipProgramsError>,
        > {
            if bundle.is_empty() {
                return Ok(());
            }
            let results = consumer
                .process_and_record_transactions_with_policy(
                    bank,
                    bundle,
                    Some(&self.bundle_account_locker),
                    true,
                )
                .execute_and_commit_transactions_output
                .commit_transactions_result;
            debug!("tip bundle result for bank {bank_key:?}: {results:?}");
            // Recording opens after this Bank is published and closes before it ends; retry.
            let Ok(results) = results else {
                return Err(None);
            };
            // A reverted bundle marks its other transactions CommitCancelled; report the cause.
            let Some(error) = results
                .into_iter()
                .filter_map(|result| match result {
                    CommitTransactionDetails::Committed { result, .. } => result.err(),
                    CommitTransactionDetails::NotCommitted(error) => Some(error),
                })
                .min_by_key(|error| *error == TransactionError::CommitCancelled)
            else {
                return Ok(());
            };
            Err(Some(error.into()))
        };
        // Each program is cranked on its own. Only our own (primary) program gates BAM
        // admission; an upstream program we cannot crank is logged and must not cost us
        // the slot.
        let mut primary_ok = true;
        for (i, tip_manager) in self.tip_managers.iter().enumerate() {
            let program = tip_manager.tip_payment_program_id();
            // Crank construction reads the accounts created by initialization; keep it lazy.
            let ok = match tip_manager.get_initialize_tip_programs_bundle(bank, &keypair) {
                Ok(init) => match process(&init) {
                    Ok(()) => true,
                    Err(error) => error.is_some_and(&mut fail),
                },
                Err(error) => fail(error.into()),
            } && match tip_manager.get_crank_steps(bank, &keypair, builder) {
                Ok(steps) => {
                    CrankStep::run_chain(
                        steps,
                        &mut |step| process(&step.txs),
                        &mut |step, error| {
                            warn!(
                                "tip program {program} crank step {} did not commit: {error:?}",
                                step.label
                            );
                            if let Some(error) = error {
                                fail(error.clone());
                            }
                        },
                    ) == CrankChainResult::Committed
                }
                Err(error) => {
                    error!("tip program {program} crank could not be built: {error:?}");
                    fail(error.into())
                }
            };
            if !ok && i == 0 {
                primary_ok = false;
            }
        }
        primary_ok
    }
}

pub fn v0_to_versioned_proto(v0: SchedulerMessageV0) -> SchedulerMessage {
    SchedulerMessage {
        versioned_msg: Some(VersionedMsg::V0(v0)),
    }
}
