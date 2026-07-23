//! FLOWRA PoC: conflict-aware transaction scheduler.
//!
//! Starts as a faithful structural copy of [`super::greedy_scheduler`] so it
//! compiles and behaves identically; `// FLOWRA PoC:` comments mark the points
//! where the conflict-aware logic will later diverge.

use {
    super::{
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
    crossbeam_channel::{Receiver, Sender},
    log::info,
    solana_clock::Slot,
    solana_cost_model::block_cost_limits::MAX_BLOCK_UNITS,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    std::num::Saturating,
};

// FLOWRA PoC: same knobs as `GreedySchedulerConfig` for now; conflict-aware
// specific knobs (e.g. conflict look-ahead depth) will be added here.
pub(crate) struct ConflictAwareSchedulerConfig {
    pub target_scheduled_cus: u64,
    pub max_scanned_transactions_per_scheduling_pass: usize,
    pub target_transactions_per_batch: usize,
}

impl Default for ConflictAwareSchedulerConfig {
    fn default() -> Self {
        Self {
            target_scheduled_cus: MAX_BLOCK_UNITS / 4,
            max_scanned_transactions_per_scheduling_pass: 100_000,
            target_transactions_per_batch: TARGET_NUM_TRANSACTIONS_PER_BATCH,
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
}

impl ConflictAwarePassStats {
    fn record_pass(
        &mut self,
        popped: usize,
        scheduled: usize,
        unschedulable_conflict: usize,
        unschedulable_thread: usize,
    ) {
        self.passes += 1;
        self.popped += popped;
        self.scheduled += scheduled;
        self.unschedulable_conflict += unschedulable_conflict;
        self.unschedulable_thread += unschedulable_thread;
    }

    /// Report and reset tallies when the observed leader slot changes.
    /// Logs at most one line per slot change to stay rate-limited.
    fn observe_slot(&mut self, slot: Option<Slot>) {
        if slot == self.current_slot {
            return;
        }
        if let Some(prev_slot) = self.current_slot {
            info!(
                "conflict_aware_scheduler_stats: slot={prev_slot} passes={} popped={} \
                 scheduled={} unschedulable_conflict={} unschedulable_thread={}",
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
}

impl<Tx: TransactionWithMeta> ConflictAwareScheduler<Tx> {
    pub(crate) fn new(
        consume_work_senders: Vec<Sender<ConsumeWork<Tx>>>,
        finished_consume_work_receiver: Receiver<FinishedConsumeWork<Tx>>,
        config: ConflictAwareSchedulerConfig,
        bundle_account_locker: BundleAccountLocker,
    ) -> Self {
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
        }
    }
}

impl<Tx: TransactionWithMeta> Scheduler<Tx> for ConflictAwareScheduler<Tx> {
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
        let target_cu_per_thread = self.config.target_scheduled_cus / num_threads as u64;

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

            // FLOWRA PoC: this is where conflict-aware ordering will diverge
            // from the greedy scheduler — instead of retrying strictly in
            // priority order, conflicting transactions will be steered/deferred
            // based on observed account contention.
            match try_schedule_transaction(
                transaction_state,
                &mut self.common.account_locks,
                schedulable_threads,
                |thread_set| {
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
                    num_scheduled += 1;
                    self.common.batches.add_transaction_to_batch(
                        thread_id,
                        id.id,
                        transaction,
                        max_age,
                        cost,
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
        self.pass_stats
            .observe_slot(decision.bank().map(|bank| bank.slot()));

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
