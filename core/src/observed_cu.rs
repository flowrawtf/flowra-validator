//! Scheduler-local estimate of the compute a transaction will actually use.
//!
//! Blocks on mainnet carry 90-120M *requested* CU (the cost model charges the explicit
//! `SetComputeUnitLimit`, or 200k per instruction) for 9-15M *consumed*: an eight-fold
//! over-request, uniform across schedulers. Everything the scheduler reasons about in CU is
//! in requested units: the priority a transaction is sorted by (reward per cost, so an honest
//! limit is deflated and a tight one inflated), the in-flight quota each worker may hold, and
//! the pacing budget, which subtracts the cost tracker's post-commit (consumed) total from a
//! requested-unit target.
//!
//! This learns, per program, the ratio of consumed to requested from the commit results the
//! workers report, and when `FLOWRA_OBSERVED_CU=1` is set the scheduler's cost estimate uses
//! it. The cost tracker and the block limit stay stock: they are a consensus rule, and only
//! the scheduler's own estimate changes. Off (the default), nothing here is consulted and
//! scheduling is byte-identical to before.
use {
    dashmap::DashMap,
    solana_pubkey::Pubkey,
    solana_sdk_ids::compute_budget,
    solana_svm_transaction::svm_message::SVMStaticMessage,
    std::sync::{
        OnceLock,
        atomic::{AtomicU32, Ordering},
    },
};

/// Fixed-point one: a ratio of `SCALE` means consumed equals requested.
pub const SCALE: u64 = 1 << 16;
/// Weight of each new observation in the moving average, as a shift: 1/16. A program that
/// changes its behaviour is relearned within a few dozen landings.
const EMA_SHIFT: u32 = 4;
/// Programs the table learns at most. Memory is bounded by this; a program first seen once
/// the table is full is estimated at its requested limit, like an unknown one.
const MAX_PROGRAMS: usize = 8_192;
/// The estimate never drops below this share of the requested limit...
const FLOOR_PCT: u64 = 5;
/// ...nor below this many CU, unless the limit itself is lower.
const FLOOR_CU: u64 = 20_000;

/// Whether the scheduler uses the learned estimate: `FLOWRA_OBSERVED_CU=1`. Read once.
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        let enabled = std::env::var("FLOWRA_OBSERVED_CU").is_ok_and(|value| value == "1");
        if enabled {
            log::info!("FLOWRA observed-CU cost estimate: enabled");
        }
        enabled
    })
}

/// Per-program moving average of consumed/requested, in `SCALE` fixed point.
#[derive(Debug, Default)]
pub struct ObservedCu {
    ratios: DashMap<Pubkey, AtomicU32>,
}

impl ObservedCu {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one landed transaction of `program` into its ratio.
    pub fn record(&self, program: &Pubkey, requested: u64, consumed: u64) {
        if requested == 0 {
            return;
        }
        let observed = (consumed.saturating_mul(SCALE) / requested).min(SCALE) as u32;
        if let Some(ratio) = self.ratios.get(program) {
            let _ = ratio.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                // current + (observed - current) / 2^EMA_SHIFT, without going negative.
                let delta = (i64::from(observed) - i64::from(current)) >> EMA_SHIFT;
                Some((i64::from(current) + delta) as u32)
            });
        } else if self.ratios.len() < MAX_PROGRAMS {
            // A new program starts at its first observation rather than at one, so the
            // first few landings are not averaged against a guess.
            self.ratios
                .entry(*program)
                .or_insert_with(|| AtomicU32::new(observed));
        }
    }

    /// The learned ratio of `program`, in `SCALE` fixed point, if it has been seen.
    pub fn ratio(&self, program: &Pubkey) -> Option<u64> {
        self.ratios
            .get(program)
            .map(|ratio| u64::from(ratio.load(Ordering::Relaxed)))
    }

    /// Execution CU to schedule a transaction of `program` at, given its requested limit:
    /// the limit scaled by the learned ratio, never below the floor and never above the
    /// limit. An unknown program is estimated at its limit.
    pub fn estimate(&self, program: &Pubkey, requested: u64) -> u64 {
        let Some(ratio) = self.ratio(program) else {
            return requested;
        };
        let floor = (requested * FLOOR_PCT / 100).max(FLOOR_CU).min(requested);
        let scaled = u64::try_from(u128::from(requested) * u128::from(ratio) / u128::from(SCALE))
            .unwrap_or(requested);
        scaled.clamp(floor, requested)
    }

    pub fn len(&self) -> usize {
        self.ratios.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ratios.is_empty()
    }
}

fn table() -> &'static ObservedCu {
    static TABLE: OnceLock<ObservedCu> = OnceLock::new();
    TABLE.get_or_init(ObservedCu::new)
}

/// The program a transaction is attributed to: its first instruction's program that is not
/// the compute-budget program, whose instructions only set limits.
pub fn program_key(transaction: &impl SVMStaticMessage) -> Option<&Pubkey> {
    transaction
        .program_instructions_iter()
        .map(|(program_id, _)| program_id)
        .find(|program_id| **program_id != compute_budget::id())
}

/// Learn from a transaction that committed with `consumed` CU against a requested limit of
/// `requested`. A no-op unless [`enabled`].
pub fn record_transaction(transaction: &impl SVMStaticMessage, requested: u64, consumed: u64) {
    if !enabled() {
        return;
    }
    if let Some(program) = program_key(transaction) {
        table().record(program, requested, consumed);
    }
}

/// The execution CU the scheduler should charge `transaction` at: its requested limit unless
/// [`enabled`] and its program has been seen land.
pub fn scheduling_execution_cost(transaction: &impl SVMStaticMessage, requested: u64) -> u64 {
    if !enabled() {
        return requested;
    }
    match program_key(transaction) {
        Some(program) => table().estimate(program, requested),
        None => requested,
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, solana_compute_budget_interface::ComputeBudgetInstruction,
        solana_keypair::Keypair, solana_message::Message,
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction, solana_signer::Signer,
        solana_system_interface::instruction as system_instruction,
        solana_transaction::Transaction,
    };

    #[test]
    fn test_ratio_is_learned_as_a_moving_average() {
        let table = ObservedCu::new();
        let program = Pubkey::new_unique();
        assert_eq!(table.ratio(&program), None);
        assert_eq!(table.estimate(&program, 1_400_000), 1_400_000);

        // The first landing sets the ratio outright: 175k of 1.4M.
        table.record(&program, 1_400_000, 175_000);
        assert_eq!(table.ratio(&program), Some(SCALE / 8));
        assert_eq!(table.estimate(&program, 1_400_000), 175_000);
        assert_eq!(table.estimate(&program, 400_000), 50_000);

        // Later landings move it a sixteenth of the way each.
        for _ in 0..64 {
            table.record(&program, 1_400_000, 700_000);
        }
        let ratio = table.ratio(&program).unwrap();
        assert!(ratio > SCALE * 49 / 100 && ratio <= SCALE / 2, "{ratio}");

        // Over-consumption (not possible on chain, but defensively) is clamped to one.
        let greedy = Pubkey::new_unique();
        table.record(&greedy, 100, 1_000);
        assert_eq!(table.ratio(&greedy), Some(SCALE));
        assert_eq!(table.estimate(&greedy, 100), 100);
    }

    #[test]
    fn test_estimate_floors() {
        let table = ObservedCu::new();
        let program = Pubkey::new_unique();
        table.record(&program, 1_000_000, 0);
        assert_eq!(table.ratio(&program), Some(0));
        // 5% of the limit, or 20k, whichever is more...
        assert_eq!(table.estimate(&program, 1_400_000), 70_000);
        assert_eq!(table.estimate(&program, 200_000), 20_000);
        // ...but never more than the limit itself.
        assert_eq!(table.estimate(&program, 150), 150);
    }

    #[test]
    fn test_table_is_bounded() {
        let table = ObservedCu::new();
        for _ in 0..MAX_PROGRAMS + 10 {
            table.record(&Pubkey::new_unique(), 1_000, 500);
        }
        assert_eq!(table.len(), MAX_PROGRAMS);
        // A program seen only once the table is full is estimated at its limit.
        let late = Pubkey::new_unique();
        table.record(&late, 1_000, 500);
        assert_eq!(table.estimate(&late, 1_000), 1_000);
        // Zero requests are not observations.
        table.record(&late, 0, 0);
        assert_eq!(table.ratio(&late), None);
    }

    #[test]
    fn test_program_key_skips_the_compute_budget_program() {
        let payer = Keypair::new();
        let to = Pubkey::new_unique();
        let message = Message::new(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(50_000),
                ComputeBudgetInstruction::set_compute_unit_price(1),
                system_instruction::transfer(&payer.pubkey(), &to, 1),
            ],
            Some(&payer.pubkey()),
        );
        let tx = RuntimeTransaction::from_transaction_for_tests(Transaction::new(
            &[&payer],
            message,
            solana_hash::Hash::new_unique(),
        ));
        assert_eq!(
            program_key(&tx),
            Some(&solana_sdk_ids::system_program::id())
        );

        // Off (the default in tests): the requested limit, whatever was recorded.
        assert!(!enabled());
        record_transaction(&tx, 50_000, 150);
        assert_eq!(scheduling_execution_cost(&tx, 50_000), 50_000);
        assert!(table().is_empty());
    }
}
