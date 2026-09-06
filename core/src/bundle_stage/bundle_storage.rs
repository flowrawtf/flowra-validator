use {
    crate::{
        banking_stage::{
            scheduler_messages::MaxAge,
            transaction_scheduler::{
                receive_and_buffer::{PacketHandlingError, TransactionViewReceiveAndBuffer},
                transaction_state_container::{
                    RuntimeTransactionView, StateContainer, TransactionViewStateContainer,
                },
            },
        },
        packet_bundle::VerifiedPacketBundle,
        transaction_priority::transaction_tip_lamports,
    },
    ahash::HashSet,
    arrayvec::ArrayVec,
    smallvec::SmallVec,
    solana_clock::{BankId, Slot},
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_runtime_transaction::{
        sanitize_config::sanitize_config, transaction_meta::TransactionMeta,
    },
    solana_svm::transaction_error_metrics::TransactionErrorMetrics,
    solana_transaction::TransactionError,
    std::{
        cmp::Ordering,
        collections::{BinaryHeap, VecDeque},
    },
};

/// The order buffered bundles are handed to execution in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleOrdering {
    /// Arrival order. What Jito ships.
    Fifo,
    /// Highest tip first; equal tips in arrival order.
    ///
    /// The leader window, not the cost model, is what runs out on mainnet: bundles still in
    /// the buffer when the window closes are cleared, and the tip on a cleared bundle was
    /// measured at five times the tip on a landed one. Under FIFO the order they are tried
    /// in is the order the engine happened to deliver them.
    TipPriority,
}

impl BundleOrdering {
    /// `FLOWRA_BUNDLE_TIP_PRIORITY=1` selects [`Self::TipPriority`]; anything else is FIFO.
    pub fn from_env() -> Self {
        match std::env::var("FLOWRA_BUNDLE_TIP_PRIORITY").as_deref() {
            Ok("1") => Self::TipPriority,
            _ => Self::Fifo,
        }
    }
}

/// Bundles removed by [`BundleStorage::prune_stale`], by the reason the working bank would
/// have rejected them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PruneStats {
    pub stale_blockhash: u64,
    pub already_processed: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BundleStorageError {
    EmptyBatch,
    ContainerFull,
    PacketMarkedDiscard(usize),
    PacketFilterError((PacketHandlingError, usize /* packet index */)),
    BundleTooLarge,
    DuplicateTransaction,
}

struct BundleTransactionId {
    container_ids: SmallVec<[usize; 5]>,
    sanitized_bank_id: BankId,
    sanitized_bank_slot: Slot,
    /// Lamports the bundle transfers to tip accounts, summed over its transactions. Computed
    /// once at insert; always zero under [`BundleOrdering::Fifo`].
    tip_lamports: u64,
    /// Arrival sequence number; the tie-break under tip ordering.
    seq: u64,
}

/// Heap key for tip ordering: highest tip first, earliest arrival among equals.
struct ByTip(BundleTransactionId);

impl PartialEq for ByTip {
    fn eq(&self, other: &Self) -> bool {
        self.0.tip_lamports == other.0.tip_lamports && self.0.seq == other.0.seq
    }
}
impl Eq for ByTip {}
impl PartialOrd for ByTip {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for ByTip {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .tip_lamports
            .cmp(&other.0.tip_lamports)
            .then_with(|| other.0.seq.cmp(&self.0.seq))
    }
}

/// The unprocessed queue, in whichever order the storage was built with.
enum BundleQueue {
    Fifo(VecDeque<BundleTransactionId>),
    Tip(BinaryHeap<ByTip>),
}

impl BundleQueue {
    fn with_capacity(ordering: BundleOrdering, capacity: usize) -> Self {
        match ordering {
            BundleOrdering::Fifo => Self::Fifo(VecDeque::with_capacity(capacity)),
            BundleOrdering::TipPriority => Self::Tip(BinaryHeap::with_capacity(capacity)),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Fifo(queue) => queue.len(),
            Self::Tip(heap) => heap.len(),
        }
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn push(&mut self, bundle: BundleTransactionId) {
        match self {
            Self::Fifo(queue) => queue.push_back(bundle),
            Self::Tip(heap) => heap.push(ByTip(bundle)),
        }
    }

    fn pop(&mut self) -> Option<BundleTransactionId> {
        match self {
            Self::Fifo(queue) => queue.pop_front(),
            Self::Tip(heap) => heap.pop().map(|entry| entry.0),
        }
    }

    fn drain(&mut self) -> Vec<BundleTransactionId> {
        match self {
            Self::Fifo(queue) => queue.drain(..).collect(),
            Self::Tip(heap) => heap.drain().map(|entry| entry.0).collect(),
        }
    }

    /// Fold the cost-model retry queue back in at a slot boundary. Under FIFO the retries
    /// go ahead of everything that arrived since, oldest first, as before; under tip
    /// ordering they simply take their place by tip, with their original arrival as the
    /// tie-break.
    fn requeue_retries(&mut self, retries: &mut VecDeque<BundleTransactionId>) {
        match self {
            Self::Fifo(queue) => {
                // the retry queue has the oldest bundles at the front; pop from the back and
                // push to the front so the oldest ends up at the front of the queue
                while let Some(bundle) = retries.pop_back() {
                    queue.push_front(bundle);
                }
            }
            Self::Tip(heap) => heap.extend(retries.drain(..).map(ByTip)),
        }
    }
}

pub struct BundleStorageEntry {
    pub container_ids: SmallVec<[usize; 5]>,
    pub transactions: SmallVec<[RuntimeTransactionView; 5]>,
    pub max_ages: SmallVec<[MaxAge; 5]>,
    sanitized_bank_id: BankId,
    sanitized_bank_slot: Slot,
    tip_lamports: u64,
    seq: u64,
}

/// Bundle storage has two queues: one for unprocessed bundles and another for ones that exceeded
/// the cost model and need to get retried next slot.
pub struct BundleStorage {
    last_slot: Slot,
    transaction_capacity: usize,
    transaction_view_state_container: TransactionViewStateContainer,
    ordering: BundleOrdering,
    /// Tip-account PDAs of every managed tip program; what a transfer must target to count as a
    /// tip. Empty under FIFO, where tips are not computed.
    tip_accounts: std::collections::HashSet<Pubkey>,
    next_seq: u64,
    unprocessed_bundles: BundleQueue,
    // Storage for bundles that exceeded the cost model for the slot they were last attempted
    // execution on
    cost_model_buffered_bundles: VecDeque<BundleTransactionId>,
    /// The bank the buffer was last pruned against; pruning runs once per leader bank.
    last_pruned_bank: Option<(Slot, BankId)>,
}

impl BundleStorage {
    const MAX_PACKETS_PER_BUNDLE: usize = 5;

    /// FIFO storage.
    #[allow(unused)]
    pub fn with_capacity(transaction_capacity: usize) -> Self {
        Self::with_ordering(
            transaction_capacity,
            BundleOrdering::Fifo,
            std::collections::HashSet::new(),
        )
    }

    /// Storage handing bundles out in `ordering`. `tip_accounts` is only consulted under
    /// [`BundleOrdering::TipPriority`].
    pub fn with_ordering(
        transaction_capacity: usize,
        ordering: BundleOrdering,
        tip_accounts: std::collections::HashSet<Pubkey>,
    ) -> Self {
        Self {
            last_slot: Slot::default(),
            transaction_capacity,
            transaction_view_state_container: TransactionViewStateContainer::with_capacity(
                transaction_capacity,
            ),
            ordering,
            tip_accounts,
            next_seq: 0,
            unprocessed_bundles: BundleQueue::with_capacity(ordering, transaction_capacity),
            cost_model_buffered_bundles: VecDeque::with_capacity(transaction_capacity),
            last_pruned_bank: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn ordering(&self) -> BundleOrdering {
        self.ordering
    }

    /// Drop buffered bundles that `bank` would reject before execution: an expired (or
    /// unknown) blockhash, or a signature already in the status cache.
    ///
    /// Bundles received before a leader window sit in the buffer until the first slot opens,
    /// and on mainnet a large share of them are dead by then — the same transaction landed in
    /// an earlier leader's block, or its blockhash aged out while it waited. Executing each of
    /// those costs a lock round-trip and a load before it fails, and every one of them runs
    /// ahead of a live bundle in FIFO order. This applies the same age and status-cache checks
    /// the bank runs at execution, once per leader bank, over the whole buffer, so the window
    /// opens on bundles that can still land.
    ///
    /// Only these two verdicts prune. Anything else the checks report is left for execution
    /// to classify, since it may depend on state that changes within the slot. Runs once per
    /// `(slot, bank_id)`; a second call for the same bank returns zeros.
    pub fn prune_stale(&mut self, bank: &Bank) -> PruneStats {
        let bank_key = (bank.slot(), bank.bank_id());
        if self.last_pruned_bank == Some(bank_key) {
            return PruneStats::default();
        }
        self.last_pruned_bank = Some(bank_key);

        let mut stats = PruneStats::default();
        let mut error_counters = TransactionErrorMetrics::default();
        let max_age = bank.max_processing_age();
        let container = &mut self.transaction_view_state_container;

        // Returns true when the bundle should stay, after counting it if it goes.
        let mut keep = |bundle: &BundleTransactionId, stats: &mut PruneStats| -> bool {
            let verdict = {
                let transactions: SmallVec<[&RuntimeTransactionView; 5]> = bundle
                    .container_ids
                    .iter()
                    .map(|id| {
                        container
                            .get_transaction(*id)
                            .expect("transaction must exist")
                    })
                    .collect();
                let lock_results: SmallVec<[Result<(), TransactionError>; 5]> =
                    SmallVec::from_elem(Ok(()), transactions.len());
                bank.check_transactions::<RuntimeTransactionView>(
                    &transactions,
                    &lock_results,
                    max_age,
                    true,
                    &mut error_counters,
                )
                .into_iter()
                .find_map(|result| result.err())
            };
            match verdict {
                Some(TransactionError::BlockhashNotFound) => stats.stale_blockhash += 1,
                Some(TransactionError::AlreadyProcessed) => stats.already_processed += 1,
                _ => return true,
            }
            for container_id in bundle.container_ids.iter() {
                container.remove_by_id(*container_id);
            }
            false
        };

        for bundle in self.unprocessed_bundles.drain() {
            if keep(&bundle, &mut stats) {
                self.unprocessed_bundles.push(bundle);
            }
        }
        let retries = std::mem::take(&mut self.cost_model_buffered_bundles);
        for bundle in retries {
            if keep(&bundle, &mut stats) {
                self.cost_model_buffered_bundles.push_back(bundle);
            }
        }

        stats
    }

    pub fn unprocessed_bundles_len(&self) -> usize {
        self.unprocessed_bundles.len()
    }

    pub fn cost_model_buffered_bundles_len(&self) -> usize {
        self.cost_model_buffered_bundles.len()
    }

    pub fn num_packets_buffered(&self) -> usize {
        self.transaction_view_state_container.buffer_size()
    }

    /// Retries a bundle by inserting the transactions back into the transaction_view_state_container.
    /// The bundle is then pushed back to the cost_model_buffered_bundles queue.
    pub fn retry_bundle(&mut self, bundle: BundleStorageEntry) {
        for (container_id, transaction) in bundle.container_ids.iter().zip(bundle.transactions) {
            self.transaction_view_state_container
                .get_mut_transaction_state(*container_id)
                .unwrap()
                .retry_transaction(transaction);
        }
        self.cost_model_buffered_bundles
            .push_back(BundleTransactionId {
                container_ids: bundle.container_ids,
                sanitized_bank_id: bundle.sanitized_bank_id,
                sanitized_bank_slot: bundle.sanitized_bank_slot,
                tip_lamports: bundle.tip_lamports,
                seq: bundle.seq,
            });
    }

    /// Destroys a bundle by removing the transactions from the transaction_view_state_container.
    /// It's important that transactions in the BundleStorageEntry are not used after this call
    /// as it will lead to panic inside the TransactionViewStateContainer.
    pub fn destroy_bundle(&mut self, bundle: BundleStorageEntry) {
        for container_id in bundle.container_ids.into_iter() {
            self.transaction_view_state_container
                .remove_by_id(container_id);
        }
    }

    /// Pops a bundle from the unprocessed_bundles queue and returns it as a BundleStorageEntry.
    /// Returns None if there are no bundles to pop.
    pub fn pop_bundle(&mut self, slot: Slot, bank_id: BankId) -> Option<BundleStorageEntry> {
        if slot != self.last_slot {
            self.unprocessed_bundles
                .requeue_retries(&mut self.cost_model_buffered_bundles);
            self.last_slot = slot;
        }

        // only want to pop from the unprocessed bundles queue and wait for slot boundary to refresh from cost_model_buffered_bundles
        while let Some(bundle) = self.unprocessed_bundles.pop() {
            if bundle.sanitized_bank_slot == slot && bundle.sanitized_bank_id != bank_id {
                for container_id in bundle.container_ids {
                    self.transaction_view_state_container
                        .remove_by_id(container_id);
                }
                continue;
            }

            let (bundle_transactions, bundle_max_ages): (
                SmallVec<[RuntimeTransactionView; 5]>,
                SmallVec<[MaxAge; 5]>,
            ) = bundle
                .container_ids
                .iter()
                .map(|id| {
                    self.transaction_view_state_container
                        .get_mut_transaction_state(*id)
                        .unwrap()
                        .take_transaction_for_scheduling()
                })
                .unzip();

            return Some(BundleStorageEntry {
                container_ids: bundle.container_ids,
                transactions: bundle_transactions,
                max_ages: bundle_max_ages,
                sanitized_bank_id: bundle.sanitized_bank_id,
                sanitized_bank_slot: bundle.sanitized_bank_slot,
                tip_lamports: bundle.tip_lamports,
                seq: bundle.seq,
            });
        }

        None
    }

    pub fn insert_bundle(
        &mut self,
        bundle: VerifiedPacketBundle,
        root_bank: &Bank,
        working_bank: &Bank,
        blacklisted_accounts: &HashSet<Pubkey>,
    ) -> Result<(), BundleStorageError> {
        let batch = bundle.take();

        // Packet checks
        if batch.is_empty() {
            return Err(BundleStorageError::EmptyBatch);
        }
        if batch.len() > Self::MAX_PACKETS_PER_BUNDLE {
            return Err(BundleStorageError::BundleTooLarge);
        }
        if let Some(idx) = batch
            .iter()
            .enumerate()
            .find_map(|(idx, packet)| packet.meta().discard().then_some(idx))
        {
            return Err(BundleStorageError::PacketMarkedDiscard(idx));
        }

        // Container checks
        if self
            .transaction_view_state_container
            .buffer_size()
            .saturating_add(batch.len())
            > self.transaction_capacity
        {
            return Err(BundleStorageError::ContainerFull);
        }

        let mut container_ids = SmallVec::<[usize; 5]>::new();
        let mut maybe_error = Ok(());
        let sanitize_config = sanitize_config(
            working_bank
                .feature_set
                .snapshot()
                .limit_instruction_accounts,
        );
        let transaction_account_lock_limit = working_bank
            .get_transaction_account_lock_limit()
            .min(root_bank.get_transaction_account_lock_limit());

        for (idx, packet) in batch.iter().enumerate() {
            // bundles shall contain all valid packets; checked above
            let packet_data = packet.data(..).unwrap();

            // try to insert the packet into the container
            if let Some(container_id) = self
                .transaction_view_state_container
                .try_insert_map_only_with_data(packet_data, |bytes| {
                    match TransactionViewReceiveAndBuffer::try_handle_packet(
                        bytes,
                        root_bank,
                        working_bank,
                        transaction_account_lock_limit,
                        &sanitize_config,
                        blacklisted_accounts,
                    ) {
                        Ok(state) => Ok(state),
                        Err(e) => {
                            maybe_error = Err(e);
                            Err(())
                        }
                    }
                })
            {
                container_ids.push(container_id);
            } else {
                // any error shall rollback any transactions added to the container
                for container_id in container_ids.iter() {
                    self.transaction_view_state_container
                        .remove_by_id(*container_id);
                }
                return Err(BundleStorageError::PacketFilterError((
                    maybe_error.unwrap_err(),
                    idx,
                )));
            }
        }

        let is_duplicate_hashes = self.does_contain_duplicate_hashes(&container_ids);
        if is_duplicate_hashes {
            for container_id in container_ids.iter() {
                self.transaction_view_state_container
                    .remove_by_id(*container_id);
            }
            return Err(BundleStorageError::DuplicateTransaction);
        }

        let tip_lamports = match self.ordering {
            BundleOrdering::Fifo => 0,
            BundleOrdering::TipPriority => container_ids
                .iter()
                .map(|id| {
                    let transaction = self
                        .transaction_view_state_container
                        .get_transaction(*id)
                        .expect("transaction must exist");
                    transaction_tip_lamports(transaction, &self.tip_accounts)
                })
                .fold(0u64, u64::saturating_add),
        };
        let seq = self.next_seq;
        self.next_seq += 1;

        self.unprocessed_bundles.push(BundleTransactionId {
            container_ids,
            sanitized_bank_id: working_bank.bank_id(),
            sanitized_bank_slot: working_bank.slot(),
            tip_lamports,
            seq,
        });

        Ok(())
    }

    fn does_contain_duplicate_hashes(&self, container_ids: &[usize]) -> bool {
        let mut transaction_hashes = ArrayVec::<_, { Self::MAX_PACKETS_PER_BUNDLE }>::new();
        for container_id in container_ids.iter() {
            let transaction_hash = self
                .transaction_view_state_container
                .get_transaction(*container_id)
                .unwrap()
                .message_hash();
            if transaction_hashes.contains(&transaction_hash) {
                return true;
            }
            transaction_hashes.push(transaction_hash);
        }
        false
    }

    pub fn clear(&mut self) {
        for bundle in self.unprocessed_bundles.drain() {
            for id in bundle.container_ids.iter() {
                self.transaction_view_state_container.remove_by_id(*id);
            }
        }
        for bundle in self.cost_model_buffered_bundles.drain(..) {
            for id in bundle.container_ids.iter() {
                self.transaction_view_state_container.remove_by_id(*id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            banking_stage::transaction_scheduler::{
                receive_and_buffer::PacketHandlingError,
                transaction_state_container::StateContainer,
            },
            bundle_stage::bundle_storage::{
                BundleOrdering, BundleStorage, BundleStorageEntry, BundleStorageError, PruneStats,
            },
            packet_bundle::VerifiedPacketBundle,
        },
        ahash::{HashSet, HashSetExt},
        solana_account::AccountSharedData,
        solana_address_lookup_table_interface::{
            self as address_lookup_table,
            state::{AddressLookupTable, LookupTableMeta},
        },
        solana_genesis_config::{GenesisConfig, create_genesis_config},
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_leader_schedule::SlotLeader,
        solana_message::{AddressLoader, AddressLookupTableAccount, VersionedMessage, v0},
        solana_perf::packet::{BytesPacket, PacketBatch},
        solana_pubkey::Pubkey,
        solana_runtime::bank::{Bank, NewBankOptions},
        solana_signature::Signature,
        solana_signer::Signer,
        solana_system_interface::instruction as system_instruction,
        solana_transaction::{Transaction, versioned::VersionedTransaction},
        std::borrow::Cow,
    };

    fn bundle_of(transaction: Transaction) -> VerifiedPacketBundle {
        VerifiedPacketBundle::new(PacketBatch::from(vec![
            BytesPacket::from_data(transaction).unwrap(),
        ]))
    }

    #[test]
    fn test_prune_stale_drops_expired_blockhash_and_keeps_live() {
        let (genesis_config, mint_keypair) = create_genesis_config(10_000_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let recipient = Pubkey::new_unique();
        let live = solana_system_transaction::transfer(
            &mint_keypair,
            &recipient,
            1,
            bank.last_blockhash(),
        );
        // A blockhash the bank has never seen: dead on arrival.
        let dead =
            solana_system_transaction::transfer(&mint_keypair, &recipient, 2, Hash::new_unique());

        let mut bundle_storage = BundleStorage::with_capacity(4);
        let blacklist = HashSet::new();
        assert!(
            bundle_storage
                .insert_bundle(bundle_of(dead), &bank, &bank, &blacklist)
                .is_ok()
        );
        assert!(
            bundle_storage
                .insert_bundle(bundle_of(live), &bank, &bank, &blacklist)
                .is_ok()
        );
        assert_eq!(bundle_storage.unprocessed_bundles_len(), 2);
        assert_eq!(bundle_storage.num_packets_buffered(), 2);

        assert_eq!(
            bundle_storage.prune_stale(&bank),
            PruneStats {
                stale_blockhash: 1,
                already_processed: 0
            }
        );
        assert_eq!(bundle_storage.unprocessed_bundles_len(), 1);
        assert_eq!(bundle_storage.num_packets_buffered(), 1);

        // The survivor is the live one, and it is still poppable.
        let popped = bundle_storage
            .pop_bundle(bank.slot(), bank.bank_id())
            .unwrap();
        assert_eq!(popped.transactions.len(), 1);
        assert_eq!(
            *popped.transactions[0].recent_blockhash(),
            bank.last_blockhash()
        );
        bundle_storage.destroy_bundle(popped);

        // Same bank again: nothing to do.
        assert_eq!(bundle_storage.prune_stale(&bank), PruneStats::default());
    }

    #[test]
    fn test_prune_stale_drops_already_processed() {
        let (genesis_config, mint_keypair) = create_genesis_config(10_000_000);
        // Executing a transaction needs the program cache's fork graph, which only the
        // bank-forks constructor wires up.
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let recipient = Pubkey::new_unique();
        let transaction = solana_system_transaction::transfer(
            &mint_keypair,
            &recipient,
            bank.get_minimum_balance_for_rent_exemption(0),
            bank.last_blockhash(),
        );
        // Land it first, so its signature is in the status cache.
        bank.process_transaction(&transaction).unwrap();

        let mut bundle_storage = BundleStorage::with_capacity(4);
        let blacklist = HashSet::new();
        assert!(
            bundle_storage
                .insert_bundle(bundle_of(transaction), &bank, &bank, &blacklist)
                .is_ok()
        );

        assert_eq!(
            bundle_storage.prune_stale(&bank),
            PruneStats {
                stale_blockhash: 0,
                already_processed: 1
            }
        );
        assert_eq!(bundle_storage.unprocessed_bundles_len(), 0);
        assert_eq!(bundle_storage.num_packets_buffered(), 0);
    }

    #[test]
    fn test_prune_stale_covers_cost_model_retry_queue() {
        let (genesis_config, mint_keypair) = create_genesis_config(10_000_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let recipient = Pubkey::new_unique();
        let dead =
            solana_system_transaction::transfer(&mint_keypair, &recipient, 2, Hash::new_unique());

        let mut bundle_storage = BundleStorage::with_capacity(4);
        let blacklist = HashSet::new();
        assert!(
            bundle_storage
                .insert_bundle(bundle_of(dead), &bank, &bank, &blacklist)
                .is_ok()
        );
        // Pop it and push it back through the cost-model retry path.
        let popped = bundle_storage
            .pop_bundle(bank.slot(), bank.bank_id())
            .unwrap();
        bundle_storage.retry_bundle(popped);
        assert_eq!(bundle_storage.cost_model_buffered_bundles_len(), 1);

        assert_eq!(
            bundle_storage.prune_stale(&bank),
            PruneStats {
                stale_blockhash: 1,
                already_processed: 0
            }
        );
        assert_eq!(bundle_storage.cost_model_buffered_bundles_len(), 0);
        assert_eq!(bundle_storage.num_packets_buffered(), 0);
    }

    fn tip_tx(payer: &Keypair, tip_account: &Pubkey, lamports: u64) -> Transaction {
        solana_system_transaction::transfer(payer, tip_account, lamports, Hash::default())
    }

    fn tip_storage(tip_account: Pubkey) -> BundleStorage {
        BundleStorage::with_ordering(
            100,
            BundleOrdering::TipPriority,
            std::collections::HashSet::from([tip_account]),
        )
    }

    fn first_signature(entry: &BundleStorageEntry) -> Signature {
        entry.transactions[0].signatures()[0]
    }

    /// Pop every bundle in order, returning each one's leading signature.
    fn pop_all(storage: &mut BundleStorage, slot: u64, bank_id: u64) -> Vec<Signature> {
        let mut order = Vec::new();
        while let Some(entry) = storage.pop_bundle(slot, bank_id) {
            order.push(first_signature(&entry));
            storage.destroy_bundle(entry);
        }
        order
    }

    #[test]
    fn test_tip_priority_pops_highest_tip_first() {
        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let tip_account = Pubkey::new_unique();
        let payer = Keypair::new();
        let mut storage = tip_storage(tip_account);
        assert_eq!(storage.ordering(), BundleOrdering::TipPriority);

        let txs: Vec<Transaction> = [100, 300, 200]
            .iter()
            .map(|lamports| tip_tx(&payer, &tip_account, *lamports))
            .collect();
        for tx in &txs {
            storage
                .insert_bundle(bundle_of(tx.clone()), &bank, &bank, &HashSet::new())
                .unwrap();
        }

        assert_eq!(
            pop_all(&mut storage, bank.slot(), bank.bank_id()),
            vec![
                txs[1].signatures[0],
                txs[2].signatures[0],
                txs[0].signatures[0]
            ]
        );
        assert_eq!(storage.num_packets_buffered(), 0);
    }

    #[test]
    fn test_tip_priority_equal_tips_keep_arrival_order() {
        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let tip_account = Pubkey::new_unique();
        let mut storage = tip_storage(tip_account);

        // Distinct payers so the transactions differ while the tips do not.
        let txs: Vec<Transaction> = (0..3)
            .map(|_| tip_tx(&Keypair::new(), &tip_account, 100))
            .collect();
        for tx in &txs {
            storage
                .insert_bundle(bundle_of(tx.clone()), &bank, &bank, &HashSet::new())
                .unwrap();
        }

        assert_eq!(
            pop_all(&mut storage, bank.slot(), bank.bank_id()),
            txs.iter().map(|tx| tx.signatures[0]).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_tip_priority_untipped_bundle_sorts_last() {
        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let tip_account = Pubkey::new_unique();
        let mut storage = tip_storage(tip_account);

        // A transfer to something other than a tip account is not a tip.
        let untipped = tip_tx(&Keypair::new(), &Pubkey::new_unique(), 1_000_000);
        let tipped = tip_tx(&Keypair::new(), &tip_account, 50);
        storage
            .insert_bundle(bundle_of(untipped.clone()), &bank, &bank, &HashSet::new())
            .unwrap();
        storage
            .insert_bundle(bundle_of(tipped.clone()), &bank, &bank, &HashSet::new())
            .unwrap();

        assert_eq!(
            pop_all(&mut storage, bank.slot(), bank.bank_id()),
            vec![tipped.signatures[0], untipped.signatures[0]]
        );
    }

    #[test]
    fn test_tip_priority_retries_merge_by_tip_next_slot() {
        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let bank_id = bank.bank_id();
        let tip_account = Pubkey::new_unique();
        let payer = Keypair::new();
        let mut storage = tip_storage(tip_account);

        let retried = tip_tx(&payer, &tip_account, 300);
        storage
            .insert_bundle(bundle_of(retried.clone()), &bank, &bank, &HashSet::new())
            .unwrap();
        let entry = storage.pop_bundle(bank.slot(), bank_id).unwrap();
        storage.retry_bundle(entry);
        assert_eq!(storage.cost_model_buffered_bundles_len(), 1);

        // Two arrive after the retry: one richer, one poorer.
        let richer = tip_tx(&payer, &tip_account, 500);
        let poorer = tip_tx(&payer, &tip_account, 100);
        for tx in [&richer, &poorer] {
            storage
                .insert_bundle(bundle_of(tx.clone()), &bank, &bank, &HashSet::new())
                .unwrap();
        }
        // Still the same slot: the retry stays parked.
        let entry = storage.pop_bundle(bank.slot(), bank_id).unwrap();
        assert_eq!(first_signature(&entry), richer.signatures[0]);
        storage.destroy_bundle(entry);

        // Next slot: the retry is folded back in by tip, ahead of the poorer newcomer.
        assert_eq!(
            pop_all(&mut storage, bank.slot() + 1, bank_id),
            vec![retried.signatures[0], poorer.signatures[0]]
        );
        assert_eq!(storage.cost_model_buffered_bundles_len(), 0);
    }

    #[test]
    fn test_fifo_ignores_tips() {
        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let tip_account = Pubkey::new_unique();
        let payer = Keypair::new();
        let mut storage = BundleStorage::with_capacity(100);
        assert_eq!(storage.ordering(), BundleOrdering::Fifo);

        let small = tip_tx(&payer, &tip_account, 100);
        let large = tip_tx(&payer, &tip_account, 300);
        for tx in [&small, &large] {
            storage
                .insert_bundle(bundle_of(tx.clone()), &bank, &bank, &HashSet::new())
                .unwrap();
        }
        assert_eq!(
            pop_all(&mut storage, bank.slot(), bank.bank_id()),
            vec![small.signatures[0], large.signatures[0]]
        );
    }

    #[test]
    fn test_prune_stale_keeps_tip_order() {
        let (genesis_config, mint_keypair) = create_genesis_config(10_000_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let tip_account = Pubkey::new_unique();
        let mut storage = tip_storage(tip_account);

        let dead = solana_system_transaction::transfer(
            &mint_keypair,
            &tip_account,
            900,
            Hash::new_unique(),
        );
        let live_small = solana_system_transaction::transfer(
            &mint_keypair,
            &tip_account,
            100,
            bank.last_blockhash(),
        );
        let live_large = solana_system_transaction::transfer(
            &mint_keypair,
            &tip_account,
            300,
            bank.last_blockhash(),
        );
        for tx in [&dead, &live_small, &live_large] {
            storage
                .insert_bundle(bundle_of(tx.clone()), &bank, &bank, &HashSet::new())
                .unwrap();
        }

        assert_eq!(
            storage.prune_stale(&bank),
            PruneStats {
                stale_blockhash: 1,
                already_processed: 0
            }
        );
        assert_eq!(
            pop_all(&mut storage, bank.slot(), bank.bank_id()),
            vec![live_large.signatures[0], live_small.signatures[0]]
        );
    }

    pub fn test_tx() -> Transaction {
        let keypair1 = Keypair::new();
        let pubkey1 = keypair1.pubkey();
        solana_system_transaction::transfer(&keypair1, &pubkey1, 42, Hash::default())
    }

    #[test]
    fn test_bundle_alt_resolution_uses_root_bank() {
        let (root_bank, _bank_forks) =
            Bank::new_with_bank_forks_for_tests(&GenesisConfig::default());
        let working_bank = Bank::new_from_parent(
            root_bank.clone(),
            SlotLeader::new_unique(),
            root_bank.slot() + 1,
        );
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let address_lookup_table_key = Pubkey::new_unique();
        let address_lookup_table = AddressLookupTable {
            meta: LookupTableMeta::default(),
            addresses: Cow::Borrowed(&[recipient]),
        };
        let data = address_lookup_table.serialize_for_tests().unwrap();
        let mut account =
            AccountSharedData::new(1, data.len(), &address_lookup_table::program::id());
        account.set_data(data);
        working_bank.store_account(&address_lookup_table_key, &account);

        let message = v0::Message::try_compile(
            &payer.pubkey(),
            &[system_instruction::transfer(&payer.pubkey(), &recipient, 1)],
            &[AddressLookupTableAccount {
                key: address_lookup_table_key,
                addresses: vec![recipient],
            }],
            working_bank.last_blockhash(),
        )
        .unwrap();

        assert!(
            AddressLoader::load_addresses(&working_bank, &message.address_table_lookups).is_ok()
        );

        let transaction =
            VersionedTransaction::try_new(VersionedMessage::V0(message), &[&payer]).unwrap();
        let packet = BytesPacket::from_data(transaction).unwrap();
        let bundle = VerifiedPacketBundle::new(PacketBatch::from(vec![packet]));
        let mut bundle_storage = BundleStorage::with_capacity(1);

        assert_eq!(
            bundle_storage.insert_bundle(
                bundle,
                root_bank.as_ref(),
                &working_bank,
                &HashSet::new(),
            ),
            Err(BundleStorageError::PacketFilterError((
                PacketHandlingError::ALTResolution,
                0,
            )))
        );
    }

    #[test]
    fn test_bundle_vote_only_check_uses_working_bank() {
        let (root_bank, _bank_forks) =
            Bank::new_with_bank_forks_for_tests(&GenesisConfig::default());
        let working_bank = Bank::new_from_parent_with_options(
            root_bank.clone(),
            SlotLeader::new_unique(),
            root_bank.slot() + 1,
            NewBankOptions {
                vote_only_bank: true,
            },
        );
        let packet = BytesPacket::from_data(test_tx()).unwrap();
        let bundle = VerifiedPacketBundle::new(PacketBatch::from(vec![packet]));
        let mut bundle_storage = BundleStorage::with_capacity(1);

        assert_eq!(
            bundle_storage.insert_bundle(
                bundle,
                root_bank.as_ref(),
                &working_bank,
                &HashSet::new(),
            ),
            Err(BundleStorageError::PacketFilterError((
                PacketHandlingError::Sanitization,
                0,
            )))
        );
    }

    #[test]
    fn test_bundle_too_large() {
        let mut bundle_storage = BundleStorage::with_capacity(10);

        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let packets: Vec<BytesPacket> = (0..BundleStorage::MAX_PACKETS_PER_BUNDLE + 1)
            .map(|_| BytesPacket::from_data(test_tx()).unwrap())
            .collect();
        let bundle = VerifiedPacketBundle::new(PacketBatch::from(packets));
        let result = bundle_storage.insert_bundle(bundle, &bank, &bank, &HashSet::new());

        assert_matches!(result, Err(BundleStorageError::BundleTooLarge));
        assert_eq!(bundle_storage.unprocessed_bundles.len(), 0);
        assert_eq!(bundle_storage.cost_model_buffered_bundles.len(), 0);
        assert!(bundle_storage.transaction_view_state_container.is_empty());
    }

    #[test]
    fn test_bundle_marked_discard() {
        let mut bundle_storage = BundleStorage::with_capacity(10);
        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let packet_1 = BytesPacket::from_data(test_tx()).unwrap();
        let mut packet_2 = BytesPacket::from_data(test_tx()).unwrap();
        packet_2.meta_mut().set_discard(true);
        let bundle = VerifiedPacketBundle::new(PacketBatch::from(vec![packet_1, packet_2]));
        let result = bundle_storage.insert_bundle(bundle, &bank, &bank, &HashSet::new());
        assert_matches!(result, Err(BundleStorageError::PacketMarkedDiscard(1)));
    }

    #[test]
    fn test_bundle_storage_exceeds_capacity() {
        let mut bundle_storage = BundleStorage::with_capacity(10);
        let bank = Bank::new_for_tests(&GenesisConfig::default());

        for i in 0..10 {
            let packet = BytesPacket::from_data(test_tx()).unwrap();
            let bundle = VerifiedPacketBundle::new(PacketBatch::from(vec![packet]));
            bundle_storage
                .insert_bundle(bundle, &bank, &bank, &HashSet::new())
                .unwrap();
            assert_eq!(bundle_storage.unprocessed_bundles.len(), i + 1);
            assert_eq!(
                bundle_storage
                    .transaction_view_state_container
                    .buffer_size(),
                i + 1
            );
        }

        let packet = BytesPacket::from_data(test_tx()).unwrap();

        let bundle = VerifiedPacketBundle::new(PacketBatch::from(vec![packet]));
        let result = bundle_storage.insert_bundle(bundle, &bank, &bank, &HashSet::new());
        assert_eq!(result, Err(BundleStorageError::ContainerFull));
        assert_eq!(bundle_storage.unprocessed_bundles.len(), 10);
        assert_eq!(
            bundle_storage
                .transaction_view_state_container
                .buffer_size(),
            10
        );
    }

    #[test]
    fn test_bundle_empty() {
        let mut bundle_storage = BundleStorage::with_capacity(10);
        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let bundle = VerifiedPacketBundle::new(PacketBatch::from(vec![]));
        let result = bundle_storage.insert_bundle(bundle, &bank, &bank, &HashSet::new());
        assert_matches!(result, Err(BundleStorageError::EmptyBatch));
    }

    #[test]
    fn test_bundle_duplicate_hashes() {
        let mut bundle_storage = BundleStorage::with_capacity(10);
        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let packet_1 = BytesPacket::from_data(test_tx()).unwrap();
        let packet_2 = packet_1.clone();
        let bundle = VerifiedPacketBundle::new(PacketBatch::from(vec![packet_1, packet_2]));
        let result = bundle_storage.insert_bundle(bundle, &bank, &bank, &HashSet::new());
        assert_matches!(result, Err(BundleStorageError::DuplicateTransaction));
        assert!(
            bundle_storage
                .transaction_view_state_container
                .buffer_size()
                == 0
        );
        assert!(bundle_storage.unprocessed_bundles.is_empty());
        assert!(bundle_storage.cost_model_buffered_bundles.is_empty());
    }

    #[test]
    fn test_retry_bundle() {
        let mut bundle_storage = BundleStorage::with_capacity(10);

        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let bank_id = bank.bank_id();
        let packet_1 = BytesPacket::from_data(test_tx()).unwrap();
        let packet_2 = BytesPacket::from_data(test_tx()).unwrap();
        let bundle = VerifiedPacketBundle::new(PacketBatch::from(vec![packet_1, packet_2]));
        let result = bundle_storage.insert_bundle(bundle, &bank, &bank, &HashSet::new());
        assert!(result.is_ok());

        let bundle_storage_entry = bundle_storage.pop_bundle(bank.slot(), bank_id).unwrap();
        bundle_storage.retry_bundle(bundle_storage_entry);

        assert!(bundle_storage.pop_bundle(bank.slot(), bank_id).is_none());
        assert!(bundle_storage.unprocessed_bundles.is_empty());
        assert_eq!(bundle_storage.cost_model_buffered_bundles.len(), 1);
        assert_eq!(
            bundle_storage
                .transaction_view_state_container
                .buffer_size(),
            2
        );

        let bundle = bundle_storage.pop_bundle(bank.slot() + 1, bank_id).unwrap();
        bundle_storage.destroy_bundle(bundle);

        let packet = BytesPacket::from_data(test_tx()).unwrap();
        bundle_storage
            .insert_bundle(
                VerifiedPacketBundle::new(PacketBatch::from(vec![packet])),
                &bank,
                &bank,
                &HashSet::new(),
            )
            .unwrap();

        assert!(
            bundle_storage
                .pop_bundle(bank.slot(), bank.bank_id() + 1)
                .is_none()
        );
        assert!(bundle_storage.transaction_view_state_container.is_empty());
    }

    #[test]
    fn test_bundle_blacklisted_account() {
        let mut bundle_storage = BundleStorage::with_capacity(10);
        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let tx = test_tx();
        let pubkey = tx.message().account_keys[0];
        let blacklisted_accounts = HashSet::from_iter([pubkey]);
        let packet = BytesPacket::from_data(tx).unwrap();
        let bundle = VerifiedPacketBundle::new(PacketBatch::from(vec![packet]));
        let result = bundle_storage.insert_bundle(bundle, &bank, &bank, &blacklisted_accounts);
        assert_matches!(
            result,
            Err(BundleStorageError::PacketFilterError((
                PacketHandlingError::FilterKey,
                0
            )))
        );
    }

    #[test]
    fn test_retry_bundle_ordering_preserved() {
        let mut bundle_storage = BundleStorage::with_capacity(100);
        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let bank_id = bank.bank_id();

        let tx_1 = test_tx();
        let tx_2 = test_tx();
        let tx_3 = test_tx();
        let tx_4 = test_tx();

        let packet_batch_1 = VerifiedPacketBundle::new(PacketBatch::from(vec![
            BytesPacket::from_data(&tx_1).unwrap(),
        ]));
        let packet_batch_2 = VerifiedPacketBundle::new(PacketBatch::from(vec![
            BytesPacket::from_data(&tx_2).unwrap(),
        ]));
        let packet_batch_3 = VerifiedPacketBundle::new(PacketBatch::from(vec![
            BytesPacket::from_data(&tx_3).unwrap(),
        ]));
        let packet_batch_4 = VerifiedPacketBundle::new(PacketBatch::from(vec![
            BytesPacket::from_data(&tx_4).unwrap(),
        ]));

        bundle_storage
            .insert_bundle(packet_batch_1, &bank, &bank, &HashSet::new())
            .unwrap();
        bundle_storage
            .insert_bundle(packet_batch_2, &bank, &bank, &HashSet::new())
            .unwrap();
        bundle_storage
            .insert_bundle(packet_batch_3, &bank, &bank, &HashSet::new())
            .unwrap();
        bundle_storage
            .insert_bundle(packet_batch_4, &bank, &bank, &HashSet::new())
            .unwrap();

        let bundle_storage_entry_1 = bundle_storage.pop_bundle(bank.slot(), bank_id).unwrap();
        assert_eq!(
            bundle_storage_entry_1.transactions[0].signatures()[0],
            tx_1.signatures[0]
        );
        let bundle_storage_entry_2 = bundle_storage.pop_bundle(bank.slot(), bank_id).unwrap();
        assert_eq!(
            bundle_storage_entry_2.transactions[0].signatures()[0],
            tx_2.signatures[0]
        );

        bundle_storage.retry_bundle(bundle_storage_entry_1);
        bundle_storage.destroy_bundle(bundle_storage_entry_2);

        let bundle_storage_entry_1 = bundle_storage.pop_bundle(bank.slot() + 1, bank_id).unwrap();
        assert_eq!(
            bundle_storage_entry_1.transactions[0].signatures()[0],
            tx_1.signatures[0]
        );
        let bundle_storage_entry_3 = bundle_storage.pop_bundle(bank.slot() + 1, bank_id).unwrap();
        assert_eq!(
            bundle_storage_entry_3.transactions[0].signatures()[0],
            tx_3.signatures[0]
        );
        let bundle_storage_entry_4 = bundle_storage.pop_bundle(bank.slot() + 1, bank_id).unwrap();
        assert_eq!(
            bundle_storage_entry_4.transactions[0].signatures()[0],
            tx_4.signatures[0]
        );
    }

    #[test]
    fn test_destroy_bundle() {
        let mut bundle_storage = BundleStorage::with_capacity(100);
        let bank = Bank::new_for_tests(&GenesisConfig::default());
        let bank_id = bank.bank_id();

        let tx_1 = test_tx();
        let tx_2 = test_tx();

        let packet_batch_1 = VerifiedPacketBundle::new(PacketBatch::from(vec![
            BytesPacket::from_data(&tx_1).unwrap(),
        ]));
        let packet_batch_2 = VerifiedPacketBundle::new(PacketBatch::from(vec![
            BytesPacket::from_data(&tx_2).unwrap(),
        ]));

        bundle_storage
            .insert_bundle(packet_batch_1, &bank, &bank, &HashSet::new())
            .unwrap();
        bundle_storage
            .insert_bundle(packet_batch_2, &bank, &bank, &HashSet::new())
            .unwrap();

        let bundle_storage_entry_1 = bundle_storage.pop_bundle(bank.slot(), bank_id).unwrap();
        bundle_storage.destroy_bundle(bundle_storage_entry_1);
        assert!(
            bundle_storage
                .transaction_view_state_container
                .buffer_size()
                == 1
        );
        let bundle_storage_entry_2 = bundle_storage.pop_bundle(bank.slot(), bank_id).unwrap();
        bundle_storage.destroy_bundle(bundle_storage_entry_2);
        assert!(
            bundle_storage
                .transaction_view_state_container
                .buffer_size()
                == 0
        );
    }

    #[test]
    fn test_clear() {
        let mut bundle_storage = BundleStorage::with_capacity(100);
        let bank = Bank::new_for_tests(&GenesisConfig::default());

        let tx_1 = test_tx();
        let tx_2 = test_tx();

        let packet_batch_1 = VerifiedPacketBundle::new(PacketBatch::from(vec![
            BytesPacket::from_data(&tx_1).unwrap(),
        ]));
        let packet_batch_2 = VerifiedPacketBundle::new(PacketBatch::from(vec![
            BytesPacket::from_data(&tx_2).unwrap(),
        ]));

        bundle_storage
            .insert_bundle(packet_batch_1, &bank, &bank, &HashSet::new())
            .unwrap();
        bundle_storage
            .insert_bundle(packet_batch_2, &bank, &bank, &HashSet::new())
            .unwrap();

        bundle_storage.clear();
        assert!(bundle_storage.unprocessed_bundles.is_empty());
        assert!(bundle_storage.cost_model_buffered_bundles.is_empty());
        assert!(
            bundle_storage
                .transaction_view_state_container
                .buffer_size()
                == 0
        );
    }
}
