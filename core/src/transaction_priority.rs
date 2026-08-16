use {
    agave_transaction_view::transaction_view::SanitizedTransactionView,
    log::info,
    solana_cost_model::cost_model::CostModel,
    solana_fee::FeeFeatures,
    solana_pubkey::Pubkey,
    solana_runtime::bank::{Bank, CollectorFeeDetails},
    solana_runtime_transaction::{
        runtime_transaction::RuntimeTransaction,
        sanitize_config::sanitize_config,
        transaction_meta::{TransactionConfiguration, TransactionMeta},
    },
    solana_sdk_ids::system_program,
    solana_svm_transaction::svm_message::SVMStaticMessage,
    solana_transaction::sanitized::MessageHash,
    std::{collections::HashSet, sync::OnceLock},
};

/// FLOWRA PoC: process-global set of tip-payment PDAs used by the optional
/// tip-aware priority calculation. Populated once at validator boot where the
/// `TipManager` is constructed.
static TIP_ACCOUNTS: OnceLock<HashSet<Pubkey>> = OnceLock::new();

/// Returns true if tip-aware priority is enabled via
/// `FLOWRA_TIP_AWARE_PRIORITY=1`. The env check is cached after first use.
fn tip_aware_priority_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var("FLOWRA_TIP_AWARE_PRIORITY").is_ok_and(|value| value == "1"))
}

/// FLOWRA PoC: register the tip-payment PDAs for tip-aware priority.
/// Subsequent calls are no-ops.
pub fn set_tip_accounts(tip_accounts: impl IntoIterator<Item = Pubkey>) {
    let tip_accounts: HashSet<Pubkey> = tip_accounts.into_iter().collect();
    let num_tip_accounts = tip_accounts.len();
    if TIP_ACCOUNTS.set(tip_accounts).is_ok() && tip_aware_priority_enabled() {
        info!("FLOWRA tip-aware priority: enabled ({num_tip_accounts} tip accounts)");
    }
}

/// Sum the lamports transferred to tip accounts by top-level System-Program
/// `Transfer` instructions in `transaction`. Returns 0 if no static account key
/// is a tip account.
///
/// Only static keys are inspected. `calculate_priority_and_cost` is also called
/// on unresolved views by the pf-floor path, where lookup tables have not been
/// loaded and there is nothing to resolve an ALT index against. A tip whose
/// destination is carried in a lookup table is therefore not counted; the tip
/// PDAs are a small, well-known set that senders pass directly.
fn transaction_tip_lamports(
    transaction: &impl SVMStaticMessage,
    tip_accounts: &HashSet<Pubkey>,
) -> u64 {
    let account_keys = transaction.static_account_keys();
    // Quick check: only walk instructions if a tip account is present at all.
    if !account_keys.iter().any(|key| tip_accounts.contains(key)) {
        return 0;
    }

    // `SystemInstruction::Transfer { lamports }` bincode encoding: 4-byte LE
    // enum discriminant (2) followed by 8-byte LE lamports.
    const TRANSFER_DISCRIMINANT: u32 = 2;
    const TRANSFER_DATA_LEN: usize = 4 + core::mem::size_of::<u64>();
    transaction
        .program_instructions_iter()
        .filter(|(program_id, _)| *program_id == &system_program::id())
        .filter_map(|(_, instruction)| {
            let data = instruction.data;
            if data.len() < TRANSFER_DATA_LEN
                || u32::from_le_bytes(data[0..4].try_into().unwrap()) != TRANSFER_DISCRIMINANT
            {
                return None;
            }
            // Destination is the second account of the transfer.
            let destination_index = usize::from(*instruction.accounts.get(1)?);
            let destination = account_keys.get(destination_index)?;
            tip_accounts
                .contains(destination)
                .then(|| u64::from_le_bytes(data[4..TRANSFER_DATA_LEN].try_into().unwrap()))
        })
        .fold(0u64, u64::saturating_add)
}

/// FLOWRA PoC: anti-cannibalization damping mode for the tip term, selected by
/// `FLOWRA_TIP_DAMPING` (only consulted when tip-aware priority is enabled).
#[derive(Clone, Copy)]
enum TipDamping {
    /// Full linear tip contribution: `reward + tip`.
    Off,
    /// Diminishing returns: tip added in full up to the fee, excess damped by a
    /// geometric-mean term so a tip that dwarfs the fee grows sub-linearly.
    Sqrt,
    /// Tip may boost priority up to `mult x` the real fee, no more.
    Cap { mult: u64 },
}

/// Returns the tip-damping mode from `FLOWRA_TIP_DAMPING`. Parsed once.
///
/// - unset / `"off"` (or any unrecognized value) -> `Off` (byte-identical to
///   the un-damped patch),
/// - `"sqrt"` -> `Sqrt`,
/// - `"cap"`  -> `Cap` with `mult` from `FLOWRA_TIP_DAMPING_CAP_MULT` (default 4).
fn tip_damping_mode() -> TipDamping {
    static MODE: OnceLock<TipDamping> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("FLOWRA_TIP_DAMPING").as_deref() {
        Ok("sqrt") => TipDamping::Sqrt,
        Ok("cap") => TipDamping::Cap {
            mult: std::env::var("FLOWRA_TIP_DAMPING_CAP_MULT")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(4),
        },
        _ => TipDamping::Off,
    })
}

/// FLOWRA PoC: fold the MEV `tip` into the priority numerator alongside the
/// pre-tip `reward` (priority_fee + 0.5*base_fee), applying the damping mode
/// selected by `FLOWRA_TIP_DAMPING`. Pure and unit-testable via `combine_tip`.
fn apply_tip(reward: u64, tip: u64) -> u64 {
    combine_tip(tip_damping_mode(), reward, tip)
}

/// Core of [`apply_tip`], parameterized on `mode` so each branch is testable
/// without touching the process-global env cache. All arithmetic saturates.
fn combine_tip(mode: TipDamping, reward: u64, tip: u64) -> u64 {
    match mode {
        // Full linear tip: identical to the un-damped tip-aware patch.
        TipDamping::Off => reward.saturating_add(tip),
        // Add tip in full up to the fee; damp the excess-over-fee by
        // `isqrt((tip - fee) * fee)`, which tends to fee-scaled sqrt growth so
        // a tip that vastly exceeds the fee yields a much smaller boost.
        TipDamping::Sqrt => {
            let fee = reward;
            let damped_tip = if tip <= fee {
                tip
            } else {
                let excess = tip - fee;
                fee.saturating_add(excess.saturating_mul(fee).isqrt())
            };
            reward.saturating_add(damped_tip)
        }
        // Tip can boost priority up to `mult x` the real fee, no more.
        TipDamping::Cap { mult } => reward.saturating_add(tip.min(reward.saturating_mul(mult))),
    }
}

/// Calculate priority and cost for a transaction:
///
/// Cost is calculated through the `CostModel`,
/// and priority is calculated through a formula here that attempts to sell
/// blockspace to the highest bidder.
///
/// The priority is calculated as:
/// P = R / (1 + C)
/// where P is the priority, R is the reward,
/// and C is the cost towards block-limits.
///
/// Current minimum costs are on the order of several hundred,
/// so the denominator is effectively C, and the +1 is simply
/// to avoid any division by zero due to a bug - these costs
/// are calculated by the cost-model and are not direct
/// from user input. They should never be zero.
/// Any difference in the prioritization is negligible for
/// the current transaction costs.
pub(crate) fn calculate_priority_and_cost<Tx: TransactionMeta + SVMStaticMessage>(
    bank: &Bank,
    transaction: &Tx,
    transaction_configuration: &TransactionConfiguration,
) -> (u64, u64) {
    let cost = CostModel::calculate_cost(transaction, &bank.feature_set).sum();
    let fee_details = solana_fee::calculate_fee_details(
        transaction,
        bank.fee_structure().lamports_per_signature,
        transaction_configuration.priority_fee_lamports,
        FeeFeatures::from(bank.feature_set.as_ref()),
    );
    let mut reward = bank
        .calculate_reward_and_burn_fee_details(&CollectorFeeDetails::from(fee_details))
        .get_deposit();

    // FLOWRA PoC: fold MEV tip lamports into the priority numerator when
    // `FLOWRA_TIP_AWARE_PRIORITY=1`. Off (unset/other) leaves the priority
    // calculation byte-identical to stock behavior.
    if tip_aware_priority_enabled() {
        if let Some(tip_accounts) = TIP_ACCOUNTS.get() {
            let tip = transaction_tip_lamports(transaction, tip_accounts);
            reward = apply_tip(reward, tip);
        }
    }

    // We need a multiplier here to avoid rounding down too aggressively.
    // For many transactions, the cost will be greater than the fees in terms of raw lamports.
    // For the purposes of calculating prioritization, we multiply the fees by a large number so that
    // the cost is a small fraction.
    // An offset of 1 is used in the denominator to explicitly avoid division by zero.
    const MULTIPLIER: u64 = 1_000_000;
    (
        reward
            .saturating_mul(MULTIPLIER)
            .saturating_div(cost.saturating_add(1)),
        cost,
    )
}

/// Evaluate raw packet bytes against the pf-floor, returning the computed
/// priority.
///
/// Returns `None` if the bytes don't parse as a valid transaction, in which
/// case the caller should leave the packet to downstream stages to reject.
pub(crate) fn calculate_priority_from_bytes(bank: &Bank, data: &[u8]) -> Option<u64> {
    let config = sanitize_config(bank.feature_set.snapshot().limit_instruction_accounts);
    let view = SanitizedTransactionView::try_new_sanitized(data, &config).ok()?;
    let runtime_tx = RuntimeTransaction::<SanitizedTransactionView<_>>::try_new(
        view,
        MessageHash::Compute,
        None,
    )
    .ok()?;
    let transaction_configuration = runtime_tx
        .transaction_configuration(&bank.feature_set)
        .ok()?;
    let (priority, _cost) =
        calculate_priority_and_cost(bank, &runtime_tx, &transaction_configuration);

    Some(priority)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_compute_budget_interface::ComputeBudgetInstruction,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::genesis_utils::{GenesisConfigInfo, create_genesis_config},
        solana_message::Message,
        solana_pubkey::Pubkey,
        solana_signer::Signer,
        solana_system_interface::instruction as system_instruction,
        solana_transaction::{Transaction, versioned::VersionedTransaction},
        std::sync::Arc,
    };

    fn test_bank_with_lamports_per_signature(lamports_per_signature: u64) -> (Arc<Bank>, Keypair) {
        let GenesisConfigInfo {
            mut genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(u64::MAX);
        if lamports_per_signature > 0 {
            genesis_config.fee_rate_governor =
                solana_fee_calculator::FeeRateGovernor::new(lamports_per_signature, 0);
        }
        let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        (bank, mint_keypair)
    }

    fn test_bank() -> (Arc<Bank>, Keypair) {
        test_bank_with_lamports_per_signature(0)
    }

    fn make_tx_bytes(mint: &Keypair, recent_blockhash: Hash, compute_unit_price: u64) -> Vec<u8> {
        let to = Pubkey::new_unique();
        let transfer = system_instruction::transfer(&mint.pubkey(), &to, 1);
        let prioritization = ComputeBudgetInstruction::set_compute_unit_price(compute_unit_price);
        let message = Message::new(&[transfer, prioritization], Some(&mint.pubkey()));
        let tx = Transaction::new(&[mint], message, recent_blockhash);
        bincode::serialize(&VersionedTransaction::from(tx)).unwrap()
    }

    fn priority_from(bank: &Bank, bytes: &[u8]) -> u64 {
        calculate_priority_from_bytes(bank, bytes).unwrap()
    }

    #[test]
    fn priority_from_bytes_returns_none_for_garbage() {
        let (bank, _) = test_bank();
        assert!(calculate_priority_from_bytes(&bank, &[]).is_none());
        assert!(calculate_priority_from_bytes(&bank, &[0u8; 32]).is_none());
    }

    #[test]
    fn priority_is_zero_when_base_and_priority_fees_are_zero() {
        // Test bank has lamports_per_signature = 0, so base fee is 0.
        // With compute_unit_price = 0, priority fee is also 0 → reward 0 → priority 0.
        let (bank, mint) = test_bank();
        assert_eq!(bank.fee_structure().lamports_per_signature, 0);
        let bytes = make_tx_bytes(&mint, bank.last_blockhash(), 0);
        assert_eq!(priority_from(&bank, &bytes), 0);
    }

    #[test]
    fn higher_compute_unit_price_yields_higher_priority() {
        // Need non-zero base fee, otherwise the reward short-circuits to 0
        // and all priorities collapse regardless of compute_unit_price.
        let (bank, mint) = test_bank_with_lamports_per_signature(5_000);
        let low = priority_from(&bank, &make_tx_bytes(&mint, bank.last_blockhash(), 1));
        let high = priority_from(
            &bank,
            &make_tx_bytes(&mint, bank.last_blockhash(), 1_000_000),
        );
        assert!(high > low, "expected high {high} > low {low}");
    }

    #[test]
    fn floor_priority_from_bytes_matches_typed_path() {
        // The bytes-path and the typed-path must agree on the same packet,
        // since the scheduler-side queue priority is computed via the typed
        // path and the sigverify-side floor check via the bytes path.
        let (bank, mint) = test_bank();
        let bytes = make_tx_bytes(&mint, bank.last_blockhash(), 100);

        let from_bytes = priority_from(&bank, &bytes);

        let view = SanitizedTransactionView::try_new_sanitized(
            &bytes[..],
            &sanitize_config(bank.feature_set.snapshot().limit_instruction_accounts),
        )
        .unwrap();
        let runtime_tx = RuntimeTransaction::<SanitizedTransactionView<_>>::try_new(
            view,
            MessageHash::Compute,
            None,
        )
        .unwrap();
        let transaction_configuration = runtime_tx
            .transaction_configuration(&bank.feature_set)
            .unwrap();
        let (from_typed, _cost) =
            calculate_priority_and_cost(&bank, &runtime_tx, &transaction_configuration);

        assert_eq!(from_bytes, from_typed);
    }

    #[test]
    fn test_apply_tip_off_is_linear() {
        // Off mode adds the tip in full, regardless of magnitude, and saturates.
        assert_eq!(combine_tip(TipDamping::Off, 100, 0), 100);
        assert_eq!(combine_tip(TipDamping::Off, 100, 50), 150);
        assert_eq!(combine_tip(TipDamping::Off, 100, 1_000_000), 1_000_100);
        assert_eq!(combine_tip(TipDamping::Off, u64::MAX, 1), u64::MAX);
    }

    #[test]
    fn test_apply_tip_sqrt_dampens_excess() {
        // At or below the fee, the tip is added in full (linear).
        assert_eq!(combine_tip(TipDamping::Sqrt, 100, 40), 140);
        assert_eq!(combine_tip(TipDamping::Sqrt, 100, 100), 200);
        // Above the fee, the excess is damped: fee + isqrt((tip - fee) * fee).
        // fee=100, tip=10_100 -> 100 + isqrt(10_000 * 100)=100+1000=1100, +reward=1_200.
        assert_eq!(combine_tip(TipDamping::Sqrt, 100, 10_100), 1_200);
        // A tip that dwarfs the fee yields far less than the linear reward + tip.
        let linear = combine_tip(TipDamping::Off, 100, 1_000_000);
        assert!(combine_tip(TipDamping::Sqrt, 100, 1_000_000) < linear);
    }

    #[test]
    fn test_apply_tip_cap_clamps() {
        // Tip boosts priority up to `mult x` the real fee, no more (reward=100, K=4 -> cap 400).
        assert_eq!(combine_tip(TipDamping::Cap { mult: 4 }, 100, 200), 300); // below cap
        assert_eq!(combine_tip(TipDamping::Cap { mult: 4 }, 100, 400), 500); // exactly at cap
        assert_eq!(combine_tip(TipDamping::Cap { mult: 4 }, 100, 10_000), 500); // clamped to cap
    }
}
