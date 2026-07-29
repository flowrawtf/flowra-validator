//! FLOWRA PoC: conflict-aware transaction scheduler.
//!
//! Starts as a faithful structural copy of [`super::greedy_scheduler`] so it
//! compiles and behaves identically; `// FLOWRA PoC:` comments mark the points
//! where the conflict-aware logic will later diverge.

use {
    super::{
        greedy_scheduler::BLOCK_LIMIT_IN_FLIGHT_DIVISOR,
        scheduler::{Scheduler, SchedulingSummary},
        scheduler_common::{
            SchedulingCommon, TransactionSchedulingError, TransactionSchedulingInfo, select_thread,
        },
        scheduler_error::SchedulerError,
        transaction_priority_id::TransactionPriorityId,
        transaction_state::TransactionState,
        transaction_state_container::StateContainer,
    },
    crate::{
        banking_stage::{
            consumer::TARGET_NUM_TRANSACTIONS_PER_BATCH,
            decision_maker::BufferedPacketsDecision,
            scheduler_messages::{ConsumeWork, FinishedConsumeWork},
        },
        bundle_stage::bundle_account_locker::BundleAccountLocker,
    },
    agave_scheduling_utils::thread_aware_account_locks::{
        ThreadAwareAccountLocks, ThreadId, ThreadSet, TryLockError,
    },
    ahash::{HashMap, HashMapExt},
    crossbeam_channel::{Receiver, Sender},
    log::info,
    solana_clock::Slot,
    solana_cost_model::block_cost_limits::MAX_BLOCK_UNITS,
    solana_ledger::shred::get_data_shred_bytes_per_batch_typical,
    solana_pubkey::Pubkey,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    std::{env, num::Saturating},
};

// FLOWRA PoC: same knobs as `GreedySchedulerConfig` for now; conflict-aware
// specific knobs (e.g. conflict look-ahead depth) will be added here.
pub(crate) struct ConflictAwareSchedulerConfig {
    /// See [`super::greedy_scheduler::GreedySchedulerConfig::target_scheduled_cus`].
    pub target_scheduled_cus: Option<u64>,
    pub max_scanned_transactions_per_scheduling_pass: usize,
    pub target_transactions_per_batch: usize,
    pub target_entry_bytes_per_batch: u64,
}

impl Default for ConflictAwareSchedulerConfig {
    fn default() -> Self {
        Self {
            target_scheduled_cus: None,
            max_scanned_transactions_per_scheduling_pass: 100_000,
            target_transactions_per_batch: TARGET_NUM_TRANSACTIONS_PER_BATCH,
            // Same budget the greedy scheduler uses; see its
            // DEFAULT_TARGET_ENTRY_BYTES_PER_BATCH for the 15% derivation.
            target_entry_bytes_per_batch: get_data_shred_bytes_per_batch_typical() * 15 / 100,
        }
    }
}

/// FLOWRA PoC: per-pass scheduling tallies, accumulated across scheduling
/// passes and reported once per leader-slot change (rate-limited logging).
#[derive(Default)]
struct ConflictAwarePassStats {
    /// Slot the current tallies were accumulated for, if any observed yet.
    current_slot: Option<Slot>,
    /// Number of scheduling passes accumulated.
    passes: usize,
    /// Number of transactions popped from the container.
    popped: usize,
    /// Number of transactions scheduled.
    scheduled: usize,
    /// Number of transactions unschedulable due to account-lock conflicts.
    unschedulable_conflict: usize,
    /// Number of transactions unschedulable due to thread capacity.
    unschedulable_thread: usize,
    /// FLOWRA PoC: CUs scheduled onto each worker thread this slot. Grown to
    /// `num_threads` on first pass. The spread across this vec is the greedy
    /// scheduler's makespan imbalance — the quantity a graph/bin-packing
    /// scheduler would improve, and the gate for whether it's worth building.
    scheduled_cus_per_thread: Vec<u64>,
}

impl ConflictAwarePassStats {
    fn record_pass(
        &mut self,
        popped: usize,
        scheduled: usize,
        unschedulable_conflict: usize,
        unschedulable_thread: usize,
        pass_cus_per_thread: &[u64],
    ) {
        self.passes += 1;
        self.popped += popped;
        self.scheduled += scheduled;
        self.unschedulable_conflict += unschedulable_conflict;
        self.unschedulable_thread += unschedulable_thread;
        if self.scheduled_cus_per_thread.len() < pass_cus_per_thread.len() {
            self.scheduled_cus_per_thread
                .resize(pass_cus_per_thread.len(), 0);
        }
        for (acc, add) in self
            .scheduled_cus_per_thread
            .iter_mut()
            .zip(pass_cus_per_thread)
        {
            *acc += *add;
        }
    }

    /// Report and reset tallies when the observed leader slot changes.
    /// Logs at most one line per slot change to stay rate-limited.
    fn observe_slot(&mut self, slot: Option<Slot>) {
        if slot == self.current_slot {
            return;
        }
        if let Some(prev_slot) = self.current_slot {
            // FLOWRA PoC: per-thread CU balance + makespan imbalance for this slot.
            // imbalance = max_thread_cu / mean_thread_cu (1.0 == perfectly balanced).
            // A greedy scheduler that already lands near 1.0 under contention load
            // leaves no makespan headroom for a graph scheduler — this is the gate.
            let cus = &self.scheduled_cus_per_thread;
            let nthreads = cus.len().max(1);
            let total_cu: u64 = cus.iter().sum();
            let max_cu = cus.iter().copied().max().unwrap_or(0);
            let min_cu = cus.iter().copied().min().unwrap_or(0);
            let mean_cu = total_cu as f64 / nthreads as f64;
            let imbalance = if mean_cu > 0.0 {
                max_cu as f64 / mean_cu
            } else {
                0.0
            };
            info!(
                "conflict_aware_scheduler_stats: slot={prev_slot} passes={} popped={} \
                 scheduled={} unschedulable_conflict={} unschedulable_thread={} \
                 total_cu={total_cu} max_cu={max_cu} min_cu={min_cu} \
                 imbalance={imbalance:.3} per_thread_cu={cus:?}",
                self.passes,
                self.popped,
                self.scheduled,
                self.unschedulable_conflict,
                self.unschedulable_thread,
            );
        }
        *self = Self {
            current_slot: slot,
            ..Self::default()
        };
    }
}

/// FLOWRA Stage 1 probe: per-leader-slot account co-occurrence structure via
/// union-find. Every scheduled tx unions all of its writable accounts; at the
/// slot boundary the connected components + their aggregate CU are logged.
///
/// This answers, cheaply and *without* changing scheduling, whether the load is
/// graph-improvable: many independent components each holding a modest CU share
/// => a cluster->thread bin-packer (Stage 1 / Rakurai's graph) can rebalance
/// makespan; one giant component holding ~all CU => nothing can split it and
/// Stage 1 is futile on this load. Env-gated by `FLOWRA_COOC_PROBE`.
#[derive(Default)]
struct CoocProbe {
    current_slot: Option<Slot>,
    idx: HashMap<Pubkey, u32>,
    parent: Vec<u32>,
    node_cu: Vec<u64>,
    num_txs: usize,
}

impl CoocProbe {
    fn node(&mut self, key: &Pubkey) -> u32 {
        if let Some(&id) = self.idx.get(key) {
            return id;
        }
        let id = self.parent.len() as u32;
        self.idx.insert(*key, id);
        self.parent.push(id);
        self.node_cu.push(0);
        id
    }

    fn find(&mut self, mut x: u32) -> u32 {
        while self.parent[x as usize] != x {
            // path halving
            let grandparent = self.parent[self.parent[x as usize] as usize];
            self.parent[x as usize] = grandparent;
            x = grandparent;
        }
        x
    }

    fn union(&mut self, a: u32, b: u32) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent[ra as usize] = rb;
        }
    }

    /// Union all writable accounts of one scheduled tx; credit its CU to the
    /// component (attributed to the first write account's node).
    fn record_tx(&mut self, write_keys: &[Pubkey], cost: u64) {
        let Some((first, rest)) = write_keys.split_first() else {
            return;
        };
        self.num_txs += 1;
        let n0 = self.node(first);
        self.node_cu[n0 as usize] += cost;
        for key in rest {
            let nk = self.node(key);
            self.union(n0, nk);
        }
    }

    fn observe_slot(&mut self, slot: Option<Slot>) {
        if slot == self.current_slot {
            return;
        }
        if let (Some(prev_slot), false) = (self.current_slot, self.parent.is_empty()) {
            let n = self.parent.len();
            let mut comp_cu: HashMap<u32, u64> = HashMap::new();
            let mut comp_acc: HashMap<u32, u32> = HashMap::new();
            for i in 0..n as u32 {
                let root = self.find(i);
                *comp_cu.entry(root).or_insert(0) += self.node_cu[i as usize];
                *comp_acc.entry(root).or_insert(0) += 1;
            }
            let total_cu: u64 = comp_cu.values().sum();
            let mut comps: Vec<(u64, u32)> =
                comp_cu.iter().map(|(r, &cu)| (cu, comp_acc[r])).collect();
            comps.sort_unstable_by(|a, b| b.0.cmp(&a.0));
            let (largest_cu, largest_accts) = comps.first().copied().unwrap_or((0, 0));
            let largest_pct = if total_cu > 0 {
                100.0 * largest_cu as f64 / total_cu as f64
            } else {
                0.0
            };
            let top5_cu: Vec<u64> = comps.iter().take(5).map(|c| c.0).collect();
            info!(
                "cooc_probe: slot={prev_slot} txs={} accounts={n} components={} \
                 total_cu={total_cu} largest_cu={largest_cu} ({largest_pct:.1}% of total) \
                 largest_accts={largest_accts} top5_cu={top5_cu:?}",
                self.num_txs,
                comps.len(),
            );
        }
        self.current_slot = slot;
        self.idx.clear();
        self.parent.clear();
        self.node_cu.clear();
        self.num_txs = 0;
    }
}

/// FLOWRA PoC: conflict-aware scheduler. Currently a structural copy of
/// `GreedyScheduler`: schedules in priority order, scheduling anything that
/// can be immediately scheduled, up to the limits.
pub struct ConflictAwareScheduler<Tx: TransactionWithMeta> {
    common: SchedulingCommon<Tx>,
    unschedulables: Vec<TransactionPriorityId>,
    config: ConflictAwareSchedulerConfig,
    bundle_account_locker: BundleAccountLocker,
    // FLOWRA PoC: per-pass tallies, reported per leader-slot change.
    pass_stats: ConflictAwarePassStats,

    // FLOWRA Stage 0: hot-account CU-balanced thread affinity (env-gated;
    // `stage0_enabled == false` keeps behaviour byte-identical to greedy).
    //
    // The greedy scheduler pins each account to the least-loaded thread at
    // first-touch with zero look-ahead to a cluster's eventual total CU, so two
    // hot clusters can land on the same thread and inflate makespan. Stage 0
    // learns each hot account's aggregate CU from the *previous* leader slot,
    // LPT-assigns whole accounts to threads (largest CU first -> least-loaded
    // thread), and biases `thread_selector` toward that preferred thread before
    // first-touch pins it. Prediction is only a *hint*: it is honoured solely
    // when the preferred thread is already lock-legal (in `thread_set`),
    // otherwise it falls back to the greedy least-loaded pick — so correctness
    // is independent of prediction quality.
    stage0_enabled: bool,
    /// Cap on how many hot accounts carry an affinity (largest CU first).
    stage0_top_k: usize,
    /// Hot account -> (preferred thread, projected CU) learned from the prior
    /// leader slot, consulted this slot.
    affinity_map: HashMap<Pubkey, (ThreadId, u64)>,
    /// Per-write-account CU accumulated during the current leader slot; drained
    /// into `affinity_map` at the next leader-slot boundary.
    slot_account_cus: HashMap<Pubkey, u64>,
    /// Leader slot the current `affinity_map` was built for (Some slots only).
    current_affinity_slot: Option<Slot>,

    // FLOWRA Stage 1 probe: env-gated account co-occurrence structure logging.
    cooc_probe_enabled: bool,
    cooc: CoocProbe,

    /// Block cost limit of the current leader bank. Only consulted when
    /// `config.target_scheduled_cus` is `None`.
    block_limit: u64,
}

impl<Tx: TransactionWithMeta> ConflictAwareScheduler<Tx> {
    pub(crate) fn new(
        consume_work_senders: Vec<Sender<ConsumeWork<Tx>>>,
        finished_consume_work_receiver: Receiver<FinishedConsumeWork<Tx>>,
        config: ConflictAwareSchedulerConfig,
        bundle_account_locker: BundleAccountLocker,
    ) -> Self {
        let stage0_enabled = env::var("FLOWRA_STAGE0")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on"))
            .unwrap_or(false);
        let stage0_top_k = env::var("FLOWRA_STAGE0_TOP_K")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|k| *k > 0)
            .unwrap_or(64);
        if stage0_enabled {
            info!(
                "conflict_aware_stage0: ENABLED top_k={stage0_top_k} \
                 (hot-account CU-balanced thread affinity)"
            );
        }
        let cooc_probe_enabled = env::var("FLOWRA_COOC_PROBE")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on"))
            .unwrap_or(false);
        if cooc_probe_enabled {
            info!("cooc_probe: ENABLED (per-slot account co-occurrence component structure)");
        }
        Self {
            unschedulables: Vec::with_capacity(config.max_scanned_transactions_per_scheduling_pass),
            common: SchedulingCommon::new(
                consume_work_senders,
                finished_consume_work_receiver,
                config.target_transactions_per_batch,
            ),
            config,
            bundle_account_locker,
            pass_stats: ConflictAwarePassStats::default(),
            stage0_enabled,
            stage0_top_k,
            affinity_map: HashMap::new(),
            slot_account_cus: HashMap::new(),
            current_affinity_slot: None,
            cooc_probe_enabled,
            cooc: CoocProbe::default(),
            block_limit: MAX_BLOCK_UNITS,
        }
    }

    /// Total in-flight CU the scheduler may hold across all worker threads.
    fn target_scheduled_cus(&self) -> u64 {
        self.config
            .target_scheduled_cus
            .unwrap_or(self.block_limit / BLOCK_LIMIT_IN_FLIGHT_DIVISOR)
    }

    /// FLOWRA Stage 0: rebuild `affinity_map` from the just-finished leader
    /// slot's per-account CU via LPT (longest-processing-time-first): sort hot
    /// accounts by CU desc, keep the top-K, and greedily place each on the
    /// currently least-loaded thread. Drains `slot_account_cus` so the next
    /// slot accumulates fresh.
    fn rebuild_affinity(&mut self, num_threads: usize) {
        let mut accounts: Vec<(Pubkey, u64)> = self.slot_account_cus.drain().collect();
        if accounts.is_empty() {
            self.affinity_map.clear();
            return;
        }
        accounts.sort_unstable_by(|a, b| b.1.cmp(&a.1));
        accounts.truncate(self.stage0_top_k);

        let mut thread_load = vec![0u64; num_threads.max(1)];
        let mut new_map: HashMap<Pubkey, (ThreadId, u64)> = HashMap::with_capacity(accounts.len());
        for (acct, cu) in accounts {
            // LPT: assign this (next-largest) account to the least-loaded thread.
            let thread_id = (0..thread_load.len())
                .min_by_key(|&t| thread_load[t])
                .unwrap_or(0);
            thread_load[thread_id] += cu;
            new_map.insert(acct, (thread_id, cu));
        }
        info!(
            "conflict_aware_stage0: rebuilt affinity hot_accounts={} projected_thread_load={:?}",
            new_map.len(),
            thread_load,
        );
        self.affinity_map = new_map;
    }

    /// FLOWRA Stage 0: at each new leader-slot boundary, promote the previous
    /// slot's accumulated per-account CU into `affinity_map`. Only fires on
    /// `Some(slot)` transitions (non-leader `None` gaps are ignored so the prior
    /// slot's accumulation survives until the next leader slot consumes it).
    fn maybe_rebuild_affinity(&mut self, slot: Option<Slot>) {
        if !self.stage0_enabled {
            return;
        }
        let Some(slot) = slot else {
            return;
        };
        if Some(slot) == self.current_affinity_slot {
            return;
        }
        let num_threads = self.common.consume_work_senders.len();
        self.rebuild_affinity(num_threads);
        self.current_affinity_slot = Some(slot);
    }
}

impl<Tx: TransactionWithMeta> Scheduler<Tx> for ConflictAwareScheduler<Tx> {
    fn set_block_limit(&mut self, block_limit: u64) {
        self.block_limit = block_limit;
    }

    fn schedule<S: StateContainer<Tx>>(
        &mut self,
        container: &mut S,
        budget: u64,
    ) -> Result<SchedulingSummary, SchedulerError> {
        // Subtract any in-flight compute units from the budget.
        let mut budget = budget.saturating_sub(
            self.common
                .in_flight_tracker
                .cus_in_flight_per_thread()
                .iter()
                .sum(),
        );

        let starting_queue_size = container.queue_size();
        let starting_buffer_size = container.buffer_size();

        let num_threads = self.common.consume_work_senders.len();
        let target_cu_per_thread = self.target_scheduled_cus() / num_threads as u64;

        let mut schedulable_threads = ThreadSet::any(num_threads);
        for thread_id in 0..num_threads {
            if self.common.in_flight_tracker.cus_in_flight_per_thread()[thread_id]
                >= target_cu_per_thread
            {
                schedulable_threads.remove(thread_id);
            }
        }
        if schedulable_threads.is_empty() {
            return Ok(SchedulingSummary {
                starting_queue_size,
                starting_buffer_size,
                ..SchedulingSummary::default()
            });
        }

        #[cfg(debug_assertions)]
        debug_assert!(
            self.common.batches.is_empty(),
            "batches must start empty for scheduling"
        );

        // Track metrics on filter.
        let mut num_scanned: usize = 0;
        let mut num_scheduled = Saturating::<usize>(0);
        let mut num_sent: usize = 0;
        let mut num_unschedulable_conflicts: usize = 0;
        let mut num_unschedulable_threads: usize = 0;
        // FLOWRA PoC: CUs this pass placed the greedy way, per worker thread.
        let mut pass_cus_per_thread = vec![0u64; num_threads];

        while budget > 0
            && num_scanned < self.config.max_scanned_transactions_per_scheduling_pass
            && !schedulable_threads.is_empty()
            && !container.is_empty()
        {
            let Some(id) = container.pop() else {
                unreachable!("container is not empty")
            };

            num_scanned += 1;

            // Should always be in the container, during initial testing phase panic.
            // Later, we can replace with a continue in case this does happen.
            let Some(transaction_state) = container.get_mut_transaction_state(id.id) else {
                panic!("transaction state must exist")
            };

            // FLOWRA Stage 0: peek the transaction's writable accounts to (a)
            // find its preferred thread (the affinity of its hottest known
            // account) and (b) capture the write keys for this-slot CU
            // accumulation. Cheap no-op when Stage 0 is disabled.
            let mut write_keys_buf: Vec<Pubkey> = Vec::new();
            let preferred_thread: Option<ThreadId> = if self.stage0_enabled
                || self.cooc_probe_enabled
            {
                let transaction = transaction_state.transaction();
                let account_keys = transaction.account_keys();
                let mut best: Option<(ThreadId, u64)> = None;
                for (index, key) in account_keys.iter().enumerate() {
                    if transaction.is_writable(index) {
                        write_keys_buf.push(*key);
                        if self.stage0_enabled {
                            if let Some(&(thread_id, cu)) = self.affinity_map.get(key) {
                                if best.is_none_or(|(_, best_cu)| cu > best_cu) {
                                    best = Some((thread_id, cu));
                                }
                            }
                        }
                    }
                }
                best.map(|(thread_id, _)| thread_id)
            } else {
                None
            };

            // FLOWRA PoC: this is where conflict-aware ordering diverges from the
            // greedy scheduler. Stage 0 steers the thread choice toward a hot
            // account's CU-balanced preferred thread, but only when that thread
            // is already lock-legal for this transaction; otherwise it falls
            // back to the greedy least-loaded pick.
            match try_schedule_transaction(
                transaction_state,
                &mut self.common.account_locks,
                schedulable_threads,
                |thread_set| {
                    if let Some(preferred) = preferred_thread {
                        if thread_set.contains(preferred) {
                            return preferred;
                        }
                    }
                    select_thread(
                        thread_set,
                        self.common.batches.total_cus(),
                        self.common.in_flight_tracker.cus_in_flight_per_thread(),
                        self.common.batches.transactions(),
                        self.common.in_flight_tracker.num_in_flight_per_thread(),
                    )
                },
                &self.bundle_account_locker,
            ) {
                Err(TransactionSchedulingError::UnschedulableConflicts) => {
                    num_unschedulable_conflicts += 1;
                    self.unschedulables.push(id);
                }
                Err(TransactionSchedulingError::UnschedulableThread) => {
                    num_unschedulable_threads += 1;
                    self.unschedulables.push(id);
                }
                Ok(TransactionSchedulingInfo {
                    thread_id,
                    transaction,
                    max_age,
                    cost,
                }) => {
                    // Mirrors `greedy_scheduler`: flush the batch before it exceeds the
                    // per-batch entry byte budget, which SIMD-0296's larger transactions
                    // can reach well before the transaction count target does.
                    let transaction_bytes = transaction.serialized_size() as u64;
                    if self.common.batches.entry_bytes()[thread_id] + transaction_bytes
                        > self.config.target_entry_bytes_per_batch
                    {
                        num_sent += self.common.send_batches()?;
                    }

                    num_scheduled += 1;
                    pass_cus_per_thread[thread_id] += cost;
                    // FLOWRA Stage 1 probe: union this tx's writable accounts.
                    if self.cooc_probe_enabled {
                        self.cooc.record_tx(&write_keys_buf, cost);
                    }
                    // FLOWRA Stage 0: attribute this tx's CU to each of its
                    // writable accounts for next-slot affinity learning.
                    if self.stage0_enabled {
                        for key in write_keys_buf.drain(..) {
                            *self.slot_account_cus.entry(key).or_insert(0) += cost;
                        }
                    }
                    self.common.batches.add_transaction_to_batch(
                        thread_id,
                        id.id,
                        transaction,
                        max_age,
                        cost,
                        transaction_bytes,
                    );
                    budget = budget.saturating_sub(cost);

                    // If target batch size is reached, send all the batches
                    if self.common.batches.transactions()[thread_id].len()
                        >= self.config.target_transactions_per_batch
                    {
                        num_sent += self.common.send_batches()?;
                    }

                    // if the thread is at target_cu_per_thread, remove it from the schedulable threads
                    // if there are no more schedulable threads, stop scheduling.
                    if self.common.in_flight_tracker.cus_in_flight_per_thread()[thread_id]
                        + self.common.batches.total_cus()[thread_id]
                        >= target_cu_per_thread
                    {
                        schedulable_threads.remove(thread_id);
                        if schedulable_threads.is_empty() {
                            break;
                        }
                    }
                }
            }
        }

        num_sent += self.common.send_batches()?;
        let Saturating(num_scheduled) = num_scheduled;
        assert_eq!(
            num_scheduled, num_sent,
            "number of scheduled and sent transactions must match"
        );

        // Push unschedulables back into the queue
        container.push_ids_into_queue(self.unschedulables.drain(..));

        // FLOWRA PoC: accumulate per-pass tallies; reported on slot change in
        // `receive_completed` where the leader slot is observable.
        self.pass_stats.record_pass(
            num_scanned,
            num_scheduled,
            num_unschedulable_conflicts,
            num_unschedulable_threads,
            &pass_cus_per_thread,
        );

        Ok(SchedulingSummary {
            starting_queue_size,
            starting_buffer_size,
            num_scheduled,
            num_unschedulable_conflicts,
            num_unschedulable_threads,
        })
    }

    // FLOWRA PoC: same as the trait's default implementation, plus leader-slot
    // observation to rate-limit the per-pass stats log to one line per slot
    // change (the decision is the only place the scheduler sees the slot).
    fn receive_completed(
        &mut self,
        container: &mut impl StateContainer<Tx>,
        decision: &BufferedPacketsDecision,
    ) -> Result<(usize, usize), SchedulerError> {
        let observed_slot = decision.bank().map(|bank| bank.slot());
        self.pass_stats.observe_slot(observed_slot);
        // FLOWRA Stage 0: promote the previous slot's per-account CU into the
        // affinity map at each new leader-slot boundary.
        self.maybe_rebuild_affinity(observed_slot);
        // FLOWRA Stage 1 probe: log the previous slot's co-occurrence structure.
        if self.cooc_probe_enabled {
            self.cooc.observe_slot(observed_slot);
        }

        let mut total_num_transactions = Saturating::<usize>(0);
        let mut total_num_retryable = Saturating::<usize>(0);
        loop {
            let (num_transactions, num_retryable) = self.common.try_receive_completed(container)?;
            if num_transactions == 0 {
                break;
            }
            total_num_transactions += num_transactions;
            total_num_retryable += num_retryable;
        }
        let Saturating(total_num_transactions) = total_num_transactions;
        let Saturating(total_num_retryable) = total_num_retryable;
        Ok((total_num_transactions, total_num_retryable))
    }

    fn scheduling_common_mut(&mut self) -> &mut SchedulingCommon<Tx> {
        &mut self.common
    }
}

// FLOWRA PoC: structural copy of `greedy_scheduler::try_schedule_transaction`;
// kept local because the conflict-aware divergence will live here (e.g.
// contention-aware thread selection and lock acquisition ordering).
fn try_schedule_transaction<Tx: TransactionWithMeta>(
    transaction_state: &mut TransactionState<Tx>,
    account_locks: &mut ThreadAwareAccountLocks,
    schedulable_threads: ThreadSet,
    thread_selector: impl Fn(ThreadSet) -> ThreadId,
    bundle_account_locker: &BundleAccountLocker,
) -> Result<TransactionSchedulingInfo<Tx>, TransactionSchedulingError> {
    // Schedule the transaction if it can be.
    let transaction = transaction_state.transaction();
    let account_keys = transaction.account_keys();
    let write_account_locks = account_keys
        .iter()
        .enumerate()
        .filter_map(|(index, key)| transaction.is_writable(index).then_some(key));
    let read_account_locks = account_keys
        .iter()
        .enumerate()
        .filter_map(|(index, key)| (!transaction.is_writable(index)).then_some(key));

    // Check bundle account locks doesn't have it yet
    let l_account_locks = bundle_account_locker.account_locks();
    for lock in read_account_locks.clone() {
        if l_account_locks.write_locks().contains_key(lock) {
            return Err(TransactionSchedulingError::UnschedulableConflicts);
        }
    }
    for lock in write_account_locks.clone() {
        if l_account_locks.write_locks().contains_key(lock)
            || l_account_locks.read_locks().contains_key(lock)
        {
            return Err(TransactionSchedulingError::UnschedulableConflicts);
        }
    }

    let thread_id = match account_locks.try_lock_accounts(
        write_account_locks,
        read_account_locks,
        schedulable_threads,
        thread_selector,
    ) {
        Ok(thread_id) => thread_id,
        Err(TryLockError::MultipleConflicts) => {
            return Err(TransactionSchedulingError::UnschedulableConflicts);
        }
        Err(TryLockError::ThreadNotAllowed) => {
            return Err(TransactionSchedulingError::UnschedulableThread);
        }
    };

    // Avoid time of check time of use race condition between bundle account locker and account locks
    drop(l_account_locks);

    let (transaction, max_age) = transaction_state.take_transaction_for_scheduling();
    let cost = transaction_state.cost();

    Ok(TransactionSchedulingInfo {
        thread_id,
        transaction,
        max_age,
        cost,
    })
}
