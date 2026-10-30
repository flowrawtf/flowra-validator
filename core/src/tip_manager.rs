use {
    crate::{
        proxy::block_engine_stage::BlockBuilderFeeInfo,
        tip_manager::{
            tip_distribution::{
                InitializeTipDistributionAccountInstruction,
                InitializeTipDistributionConfigInstruction, JitoTipDistributionConfig,
                TipDistributionAccount, TipDistributionError,
            },
            tip_payment::{
                ChangeBlockBuilderInstruction, ChangeTipReceiverInstruction,
                InitializeTipPaymentInstruction, JitoTipPaymentConfig, TipPaymentError,
            },
        },
    },
    smallvec::SmallVec,
    solana_account::ReadableAccount,
    solana_clock::Epoch,
    solana_instruction::{AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_runtime::bank::Bank,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_sdk_ids::system_program,
    solana_signer::Signer,
    solana_transaction::{
        Transaction,
        sanitized::{MessageHash, SanitizedTransaction},
        versioned::VersionedTransaction,
    },
    std::collections::HashSet,
    thiserror::Error,
};

pub(crate) mod tip_distribution;
pub(crate) mod tip_payment;

#[derive(Debug, Clone, PartialEq, Error)]
pub enum TipManagerError {
    #[error("Account missing")]
    AccountMissing,
    #[error("Tip payment error: {0}")]
    TipPaymentError(#[from] TipPaymentError),
    #[error("Tip distribution error: {0}")]
    TipDistributionError(#[from] TipDistributionError),
}

pub type Result<T> = std::result::Result<T, TipManagerError>;

#[derive(Debug, Clone)]
struct TipPaymentProgramInfo {
    program_id: Pubkey,

    config_pda_bump: (Pubkey, u8),
    tip_pda_0: (Pubkey, u8),
    tip_pda_1: (Pubkey, u8),
    tip_pda_2: (Pubkey, u8),
    tip_pda_3: (Pubkey, u8),
    tip_pda_4: (Pubkey, u8),
    tip_pda_5: (Pubkey, u8),
    tip_pda_6: (Pubkey, u8),
    tip_pda_7: (Pubkey, u8),
}

/// Contains metadata regarding the tip-distribution account.
/// The PDAs contained in this struct are presumed to be owned by the program.
#[derive(Debug, Clone)]
struct TipDistributionProgramInfo {
    /// The tip-distribution program_id.
    program_id: Pubkey,

    /// Singleton [Config] PDA and bump tuple.
    config_pda_and_bump: (Pubkey, u8),
}

/// This config is used on each invocation to the `initialize_tip_distribution_account` instruction.
#[derive(Debug, Clone)]
pub struct TipDistributionAccountConfig {
    /// The account with authority to upload merkle-roots to this validator's [TipDistributionAccount].
    pub merkle_root_upload_authority: Pubkey,

    /// This validator's vote account.
    pub vote_account: Pubkey,

    /// This validator's commission rate BPS for tips in the [TipDistributionAccount].
    pub commission_bps: u16,
}

impl Default for TipDistributionAccountConfig {
    fn default() -> Self {
        Self {
            merkle_root_upload_authority: Pubkey::new_unique(),
            vote_account: Pubkey::new_unique(),
            commission_bps: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TipManager {
    tip_payment_program_info: TipPaymentProgramInfo,
    tip_distribution_program_info: TipDistributionProgramInfo,
    tip_distribution_account_config: TipDistributionAccountConfig,
    tip_accounts: HashSet<Pubkey>,
}

#[derive(Clone)]
pub struct TipManagerConfig {
    pub tip_payment_program_id: Pubkey,
    pub tip_distribution_program_id: Pubkey,
    pub tip_distribution_account_config: TipDistributionAccountConfig,
}

impl Default for TipManagerConfig {
    fn default() -> Self {
        TipManagerConfig {
            tip_payment_program_id: Pubkey::new_unique(),
            tip_distribution_program_id: Pubkey::new_unique(),
            tip_distribution_account_config: TipDistributionAccountConfig::default(),
        }
    }
}

impl TipManager {
    pub fn new(config: TipManagerConfig) -> TipManager {
        let TipManagerConfig {
            tip_payment_program_id,
            tip_distribution_program_id,
            tip_distribution_account_config,
        } = config;

        // https://github.com/jito-foundation/jito-programs/blob/8f55af0a9b31ac2192415b59ce2c47329ee255a2/mev-programs/programs/tip-payment/src/lib.rs#L33C42-L33C56
        let tip_payment_config_pda_bump =
            JitoTipPaymentConfig::find_program_address(&tip_payment_program_id);
        let tip_payment_account_pdas =
            JitoTipPaymentConfig::find_tip_payment_account_pdas(&tip_payment_program_id);

        let tip_distribution_config_pubkey_bump =
            JitoTipDistributionConfig::find_program_address(&tip_distribution_program_id);

        let tip_accounts = HashSet::from_iter(tip_payment_account_pdas.iter().map(|pda| pda.0));

        TipManager {
            tip_payment_program_info: TipPaymentProgramInfo {
                program_id: tip_payment_program_id,
                config_pda_bump: tip_payment_config_pda_bump,
                tip_pda_0: tip_payment_account_pdas[0],
                tip_pda_1: tip_payment_account_pdas[1],
                tip_pda_2: tip_payment_account_pdas[2],
                tip_pda_3: tip_payment_account_pdas[3],
                tip_pda_4: tip_payment_account_pdas[4],
                tip_pda_5: tip_payment_account_pdas[5],
                tip_pda_6: tip_payment_account_pdas[6],
                tip_pda_7: tip_payment_account_pdas[7],
            },
            tip_distribution_program_info: TipDistributionProgramInfo {
                program_id: tip_distribution_program_id,
                config_pda_and_bump: tip_distribution_config_pubkey_bump,
            },
            tip_distribution_account_config,
            tip_accounts,
        }
    }

    pub fn tip_payment_program_id(&self) -> Pubkey {
        self.tip_payment_program_info.program_id
    }

    pub fn tip_distribution_program_id(&self) -> Pubkey {
        self.tip_distribution_program_info.program_id
    }

    /// Returns the [Config] account owned by the tip-payment program.
    pub fn tip_payment_config_pubkey(&self) -> Pubkey {
        self.tip_payment_program_info.config_pda_bump.0
    }

    /// Returns the [Config] account owned by the tip-distribution program.
    pub fn tip_distribution_config_pubkey(&self) -> Pubkey {
        self.tip_distribution_program_info.config_pda_and_bump.0
    }

    pub fn get_tip_accounts(&self) -> &HashSet<Pubkey> {
        &self.tip_accounts
    }

    fn get_tip_payment_config_account(&self, bank: &Bank) -> Result<JitoTipPaymentConfig> {
        let config_data = bank
            .get_account(&self.tip_payment_program_info.config_pda_bump.0)
            .ok_or(TipManagerError::AccountMissing)?;

        JitoTipPaymentConfig::from_account_shared_data(
            &config_data,
            &self.tip_payment_program_info.program_id,
        )
        .map_err(TipManagerError::TipPaymentError)
    }

    /// Only called once during contract creation.
    pub fn initialize_tip_payment_program_tx(
        &self,
        bank: &Bank,
        keypair: &Keypair,
    ) -> Result<RuntimeTransaction<SanitizedTransaction>> {
        let init_ix = Instruction {
            program_id: self.tip_payment_program_info.program_id,
            data: InitializeTipPaymentInstruction::to_instruction_data(
                self.tip_payment_program_info.config_pda_bump.1,
                self.tip_payment_program_info.tip_pda_0.1,
                self.tip_payment_program_info.tip_pda_1.1,
                self.tip_payment_program_info.tip_pda_2.1,
                self.tip_payment_program_info.tip_pda_3.1,
                self.tip_payment_program_info.tip_pda_4.1,
                self.tip_payment_program_info.tip_pda_5.1,
                self.tip_payment_program_info.tip_pda_6.1,
                self.tip_payment_program_info.tip_pda_7.1,
            )?,
            accounts: vec![
                AccountMeta::new(self.tip_payment_program_info.config_pda_bump.0, false),
                AccountMeta::new(self.tip_payment_program_info.tip_pda_0.0, false),
                AccountMeta::new(self.tip_payment_program_info.tip_pda_1.0, false),
                AccountMeta::new(self.tip_payment_program_info.tip_pda_2.0, false),
                AccountMeta::new(self.tip_payment_program_info.tip_pda_3.0, false),
                AccountMeta::new(self.tip_payment_program_info.tip_pda_4.0, false),
                AccountMeta::new(self.tip_payment_program_info.tip_pda_5.0, false),
                AccountMeta::new(self.tip_payment_program_info.tip_pda_6.0, false),
                AccountMeta::new(self.tip_payment_program_info.tip_pda_7.0, false),
                AccountMeta::new_readonly(system_program::id(), false),
                AccountMeta::new(keypair.pubkey(), true),
            ],
        };
        let tx = VersionedTransaction::from(Transaction::new_signed_with_payer(
            &[init_ix],
            Some(&keypair.pubkey()),
            &[keypair],
            bank.last_blockhash(),
        ));
        Ok(RuntimeTransaction::try_create(
            tx,
            MessageHash::Compute,
            None,
            bank,
            bank.get_reserved_account_keys(),
        )
        .unwrap())
    }

    /// Returns this validator's [TipDistributionAccount] PDA derived from the provided epoch.
    pub fn get_my_tip_distribution_pda(&self, epoch: Epoch) -> Pubkey {
        TipDistributionAccount::find_program_address(
            &self.tip_distribution_program_info.program_id,
            &self.tip_distribution_account_config.vote_account,
            epoch,
        )
        .0
    }

    /// Returns whether or not the tip-payment program should be initialized.
    pub fn should_initialize_tip_payment_program(&self, bank: &Bank) -> bool {
        match bank.get_account(&self.tip_payment_config_pubkey()) {
            None => true,
            Some(account) => account.owner() != &self.tip_payment_program_info.program_id,
        }
    }

    /// Returns whether or not the tip-distribution program's [Config] PDA should be initialized.
    pub fn should_initialize_tip_distribution_config(&self, bank: &Bank) -> bool {
        match bank.get_account(&self.tip_distribution_config_pubkey()) {
            None => true,
            Some(account) => account.owner() != &self.tip_distribution_program_info.program_id,
        }
    }

    /// Returns whether or not the current [TipDistributionAccount] PDA should be initialized for this epoch.
    pub fn should_init_tip_distribution_account(&self, bank: &Bank) -> bool {
        let pda = self.get_my_tip_distribution_pda(bank.epoch());
        match bank.get_account(&pda) {
            None => true,
            // Since anyone can derive the PDA and send it lamports we must also check the owner is the program.
            Some(account) => account.owner() != &self.tip_distribution_program_info.program_id,
        }
    }

    /// Creates an [Initialize] transaction object.
    pub fn initialize_tip_distribution_config_tx(
        &self,
        bank: &Bank,
        kp: &Keypair,
    ) -> Result<RuntimeTransaction<SanitizedTransaction>> {
        let ix = Instruction {
            program_id: self.tip_distribution_program_info.program_id,
            data: InitializeTipDistributionConfigInstruction::to_instruction_data(
                kp.pubkey(),
                kp.pubkey(),
                10,
                10_000,
                self.tip_distribution_program_info.config_pda_and_bump.1,
            )?,
            accounts: vec![
                AccountMeta::new(
                    self.tip_distribution_program_info.config_pda_and_bump.0,
                    false,
                ),
                AccountMeta::new_readonly(system_program::id(), false),
                AccountMeta::new(kp.pubkey(), true),
            ],
        };

        let tx = VersionedTransaction::from(Transaction::new_signed_with_payer(
            &[ix],
            Some(&kp.pubkey()),
            &[kp],
            bank.last_blockhash(),
        ));
        Ok(RuntimeTransaction::try_create(
            tx,
            MessageHash::Compute,
            None,
            bank,
            bank.get_reserved_account_keys(),
        )
        .unwrap())
    }

    /// Creates an [InitializeTipDistributionAccount] transaction object using the provided Epoch.
    pub fn initialize_tip_distribution_account_tx(
        &self,
        bank: &Bank,
        kp: &Keypair,
    ) -> Result<RuntimeTransaction<SanitizedTransaction>> {
        let (tip_distribution_account, bump) = TipDistributionAccount::find_program_address(
            &self.tip_distribution_program_info.program_id,
            &self.tip_distribution_account_config.vote_account,
            bank.epoch(),
        );

        let ix = Instruction {
            program_id: self.tip_distribution_program_info.program_id,
            data: InitializeTipDistributionAccountInstruction::to_instruction_data(
                self.tip_distribution_account_config
                    .merkle_root_upload_authority,
                self.tip_distribution_account_config.commission_bps,
                bump,
            )?,
            accounts: vec![
                AccountMeta::new_readonly(
                    self.tip_distribution_program_info.config_pda_and_bump.0,
                    false,
                ),
                AccountMeta::new(tip_distribution_account, false),
                AccountMeta::new_readonly(self.tip_distribution_account_config.vote_account, false),
                AccountMeta::new(kp.pubkey(), true),
                AccountMeta::new_readonly(system_program::id(), false),
            ],
        };

        let tx = VersionedTransaction::from(Transaction::new_signed_with_payer(
            &[ix],
            Some(&kp.pubkey()),
            &[kp],
            bank.last_blockhash(),
        ));
        Ok(RuntimeTransaction::try_create(
            tx,
            MessageHash::Compute,
            None,
            bank,
            bank.get_reserved_account_keys(),
        )
        .unwrap())
    }

    /// Return a bundle that is capable of calling the initialize instructions on the two tip payment programs
    /// This is mainly helpful for local development and shouldn't run on testnet and mainnet, assuming the
    /// correct TipManager configuration is set.
    pub fn get_initialize_tip_programs_bundle(
        &self,
        bank: &Bank,
        keypair: &Keypair,
    ) -> Result<SmallVec<[RuntimeTransaction<SanitizedTransaction>; 2]>> {
        // SmallVec 2: only init-payment-program + init-distribution-config can be produced here.
        let mut transactions = SmallVec::with_capacity(2);
        if self.should_initialize_tip_payment_program(bank) {
            info!("should_initialize_tip_payment_program=true");
            transactions.push(self.initialize_tip_payment_program_tx(bank, keypair)?);
        }

        if self.should_initialize_tip_distribution_config(bank) {
            info!("should_initialize_tip_distribution_config=true");
            transactions.push(self.initialize_tip_distribution_config_tx(bank, keypair)?);
        }

        Ok(transactions)
    }

    /// The crank, decomposed into independently-executed steps.
    ///
    /// Bundles execute all-or-nothing, so putting every crank instruction in one bundle
    /// makes the weakest instruction able to veto the rest — a rejected
    /// `change_block_builder` would roll back the `initialize_tip_distribution_account`
    /// alongside it, and since the TDA is derived from the *current* epoch, an epoch that
    /// ends without one can never get it back. Each step therefore executes on its own.
    ///
    /// The steps still form a chain, because later ones read state earlier ones write:
    /// `change_tip_receiver` may only point at a TDA that exists, and
    /// `change_block_builder` constrains the passed tip receiver against the config that
    /// `change_tip_receiver` just set. A failing step marked `blocking` therefore stops the
    /// rest of *this* program's chain — but never another program's.
    pub fn get_crank_steps(
        &self,
        bank: &Bank,
        keypair: &Keypair,
        block_builder_fee_info: &BlockBuilderFeeInfo,
    ) -> Result<Vec<CrankStep>> {
        let mut steps = Vec::with_capacity(3);

        if self.should_init_tip_distribution_account(bank) {
            info!(
                "tip_distribution_account missing for epoch {} (program {}), initializing",
                bank.epoch(),
                self.tip_distribution_program_info.program_id
            );
            steps.push(CrankStep {
                label: "init_tip_distribution_account",
                // change_tip_receiver must not point at a TDA that failed to be created:
                // tips swept there afterwards would land on an unowned address.
                blocking: true,
                txs: smallvec::smallvec![
                    self.initialize_tip_distribution_account_tx(bank, keypair)?
                ],
                fallback: vec![],
            });
        }

        let cfg = self.get_tip_payment_config_account(bank)?;
        let my_tip_receiver = self.get_my_tip_distribution_pda(bank.epoch());

        let change_tip_receiver = (cfg.tip_receiver() != my_tip_receiver)
            .then(|| -> Result<CrankStep> {
                Ok(CrankStep {
                    label: "change_tip_receiver",
                    // change_block_builder re-checks the tip receiver against the config.
                    blocking: true,
                    txs: smallvec::smallvec![self.change_tip_receiver_tx(
                        &cfg.tip_receiver(),
                        &my_tip_receiver,
                        &cfg.block_builder(),
                        bank,
                        keypair,
                    )?],
                    fallback: vec![],
                })
            })
            .transpose()?;

        let change_block_builder = (cfg.block_builder() != block_builder_fee_info.block_builder
            || cfg.block_builder_commission_pct()
                != block_builder_fee_info.block_builder_commission)
            .then(|| -> Result<CrankStep> {
                Ok(CrankStep {
                    label: "change_block_builder",
                    // Purely a fee-routing preference; nothing downstream depends on it, so a
                    // misconfigured block builder must not cost us the TDA or the receiver.
                    blocking: false,
                    txs: smallvec::smallvec![self.change_block_builder_tx(
                        &my_tip_receiver,
                        &cfg.block_builder(),
                        &block_builder_fee_info.block_builder,
                        block_builder_fee_info.block_builder_commission,
                        bank,
                        keypair,
                    )?],
                    fallback: vec![],
                })
            })
            .transpose()?;

        match (change_tip_receiver, change_block_builder) {
            // The opening slot of every window needs both. Jito sends them as one
            // transaction, and so do we: one execution and one PoH record round trip
            // instead of two, serialized at the head of the slot before any bundle can run.
            // The separate steps stay as the fallback, so a block builder the program
            // rejects still costs us only that step, never the receiver.
            (Some(receiver), Some(builder)) => steps.push(CrankStep {
                label: "change_tip_receiver_and_block_builder",
                blocking: true,
                txs: smallvec::smallvec![self.change_tip_receiver_and_block_builder_tx(
                    &cfg.tip_receiver(),
                    &my_tip_receiver,
                    &cfg.block_builder(),
                    &block_builder_fee_info.block_builder,
                    block_builder_fee_info.block_builder_commission,
                    bank,
                    keypair,
                )?],
                fallback: vec![receiver, builder],
            }),
            (receiver, builder) => steps.extend(receiver.into_iter().chain(builder)),
        }

        Ok(steps)
    }

    fn sign_tx(
        &self,
        ixs: &[Instruction],
        bank: &Bank,
        keypair: &Keypair,
    ) -> RuntimeTransaction<SanitizedTransaction> {
        let tx = VersionedTransaction::from(Transaction::new_signed_with_payer(
            ixs,
            Some(&keypair.pubkey()),
            &[keypair],
            bank.last_blockhash(),
        ));
        RuntimeTransaction::try_create(
            tx,
            MessageHash::Compute,
            None,
            bank,
            bank.get_reserved_account_keys(),
        )
        .unwrap()
    }

    /// Point the tip-payment program's global tip receiver at `new_tip_receiver`. The
    /// program first drains the eight tip PDAs to `old_tip_receiver`, which is why they are
    /// all passed writable.
    pub fn change_tip_receiver_tx(
        &self,
        old_tip_receiver: &Pubkey,
        new_tip_receiver: &Pubkey,
        old_block_builder: &Pubkey,
        bank: &Bank,
        keypair: &Keypair,
    ) -> Result<RuntimeTransaction<SanitizedTransaction>> {
        let ix = self.change_tip_receiver_ix(
            old_tip_receiver,
            new_tip_receiver,
            old_block_builder,
            keypair,
        );
        Ok(self.sign_tx(&[ix], bank, keypair))
    }

    fn change_tip_receiver_ix(
        &self,
        old_tip_receiver: &Pubkey,
        new_tip_receiver: &Pubkey,
        old_block_builder: &Pubkey,
        keypair: &Keypair,
    ) -> Instruction {
        Instruction {
            program_id: self.tip_payment_program_info.program_id,
            data: ChangeTipReceiverInstruction::to_instruction_data(),
            accounts: self.tip_ix_accounts(
                &[*old_tip_receiver, *new_tip_receiver, *old_block_builder],
                keypair,
            ),
        }
    }

    /// Both metadata changes in one transaction, the way upstream Jito cranks: the receiver
    /// first, then the block builder against the receiver just set.
    #[allow(clippy::too_many_arguments)]
    pub fn change_tip_receiver_and_block_builder_tx(
        &self,
        old_tip_receiver: &Pubkey,
        new_tip_receiver: &Pubkey,
        old_block_builder: &Pubkey,
        new_block_builder: &Pubkey,
        block_builder_commission: u64,
        bank: &Bank,
        keypair: &Keypair,
    ) -> Result<RuntimeTransaction<SanitizedTransaction>> {
        let change_tip_receiver = self.change_tip_receiver_ix(
            old_tip_receiver,
            new_tip_receiver,
            old_block_builder,
            keypair,
        );
        let change_block_builder = self.change_block_builder_ix(
            new_tip_receiver,
            old_block_builder,
            new_block_builder,
            block_builder_commission,
            keypair,
        )?;
        Ok(self.sign_tx(&[change_tip_receiver, change_block_builder], bank, keypair))
    }

    /// Set the block builder and its commission. Must run after [`Self::change_tip_receiver_tx`]
    /// in the same slot: the program constrains the passed tip receiver against the config.
    pub fn change_block_builder_tx(
        &self,
        tip_receiver: &Pubkey,
        old_block_builder: &Pubkey,
        new_block_builder: &Pubkey,
        block_builder_commission: u64,
        bank: &Bank,
        keypair: &Keypair,
    ) -> Result<RuntimeTransaction<SanitizedTransaction>> {
        let ix = self.change_block_builder_ix(
            tip_receiver,
            old_block_builder,
            new_block_builder,
            block_builder_commission,
            keypair,
        )?;
        Ok(self.sign_tx(&[ix], bank, keypair))
    }

    fn change_block_builder_ix(
        &self,
        tip_receiver: &Pubkey,
        old_block_builder: &Pubkey,
        new_block_builder: &Pubkey,
        block_builder_commission: u64,
        keypair: &Keypair,
    ) -> Result<Instruction> {
        Ok(Instruction {
            program_id: self.tip_payment_program_info.program_id,
            data: ChangeBlockBuilderInstruction::to_instruction_data(block_builder_commission)?,
            accounts: {
                let mut accounts = self.tip_ix_accounts(
                    &[*tip_receiver, *old_block_builder, *new_block_builder],
                    keypair,
                );
                // Metadata can cycle back within one blockhash. An unused read-only account
                // makes each required crank distinct without adding an instruction or signer.
                accounts.push(AccountMeta::new_readonly(
                    Pubkey::new_from_array(rand::random()),
                    false,
                ));
                accounts
            },
        })
    }

    /// Account list shared by the tip-payment mutating instructions:
    /// `config, <caller-supplied>, tip_pda_0..7, signer`.
    fn tip_ix_accounts(&self, middle: &[Pubkey], keypair: &Keypair) -> Vec<AccountMeta> {
        let info = &self.tip_payment_program_info;
        let mut accounts = Vec::with_capacity(2 + middle.len() + 8);
        accounts.push(AccountMeta::new(info.config_pda_bump.0, false));
        accounts.extend(middle.iter().map(|k| AccountMeta::new(*k, false)));
        for pda in [
            info.tip_pda_0.0,
            info.tip_pda_1.0,
            info.tip_pda_2.0,
            info.tip_pda_3.0,
            info.tip_pda_4.0,
            info.tip_pda_5.0,
            info.tip_pda_6.0,
            info.tip_pda_7.0,
        ] {
            accounts.push(AccountMeta::new(pda, false));
        }
        accounts.push(AccountMeta::new(keypair.pubkey(), true));
        accounts
    }
}

/// One independently-executed stage of the tip-program crank. See
/// [`TipManager::get_crank_steps`].
pub struct CrankStep {
    pub label: &'static str,
    pub txs: SmallVec<[RuntimeTransaction<SanitizedTransaction>; 2]>,
    /// Whether a failure here invalidates the remaining steps of the same program.
    pub blocking: bool,
    /// Steps that do this step's work in its place when it fails: the same chain, split
    /// into more transactions. Empty for a step that has no other form.
    pub fallback: Vec<CrankStep>,
}

/// How a program's crank chain ended; see [`CrankStep::run_chain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrankChainResult {
    /// Every step committed, directly or through its fallback.
    Committed,
    /// A non-blocking step failed; the chain ran to its end regardless.
    Degraded,
    /// A blocking step failed (and its fallback, if it had one); the rest never ran.
    Broken,
}

impl CrankStep {
    /// Drive one program's chain. `run` executes a step and reports whether it committed;
    /// `on_failure` is told of each step that failed for good, i.e. one without a fallback,
    /// since a step whose fallback commits has not failed at all.
    pub fn run_chain<E>(
        steps: Vec<CrankStep>,
        run: &mut impl FnMut(&CrankStep) -> std::result::Result<(), E>,
        on_failure: &mut impl FnMut(&CrankStep, &E),
    ) -> CrankChainResult {
        let mut result = CrankChainResult::Committed;
        for step in steps {
            let Err(error) = run(&step) else {
                continue;
            };
            if !step.fallback.is_empty() {
                match Self::run_chain(step.fallback, run, on_failure) {
                    CrankChainResult::Committed => {}
                    CrankChainResult::Degraded => result = CrankChainResult::Degraded,
                    CrankChainResult::Broken => return CrankChainResult::Broken,
                }
                continue;
            }
            on_failure(&step, &error);
            if step.blocking {
                return CrankChainResult::Broken;
            }
            result = CrankChainResult::Degraded;
        }
        result
    }
}

/// The tip programs this validator cranks each leader slot.
///
/// More than one is meaningful when the validator accepts order flow from more than one
/// block engine: bundles from an upstream engine tip *that* engine's tip PDAs, which are
/// derived from a different tip-payment program. Unless we crank that program too, those
/// tips are swept to whichever validator cranks it next — we would be supplying the block
/// space and someone else would collect. Each program is cranked independently so one
/// misconfigured program can never disable another.
#[derive(Debug, Clone)]
pub struct TipManagers {
    managers: Vec<TipManager>,
    /// Union of every managed program's tip PDAs.
    tip_accounts: HashSet<Pubkey>,
}

impl TipManagers {
    pub fn new(configs: Vec<TipManagerConfig>) -> Self {
        let managers: Vec<TipManager> = configs.into_iter().map(TipManager::new).collect();
        let tip_accounts = managers
            .iter()
            .flat_map(|m| m.get_tip_accounts().iter().copied())
            .collect();
        Self {
            managers,
            tip_accounts,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &TipManager> {
        self.managers.iter()
    }

    /// Every managed program's tip PDAs. Callers use this to decide whether a transaction
    /// touches tip state at all, so it must cover all programs, not just the primary.
    pub fn get_tip_accounts(&self) -> &HashSet<Pubkey> {
        &self.tip_accounts
    }

    /// The program whose accounts this validator owns end-to-end; used where a single
    /// program id is required (e.g. the fetch-stage account filter).
    pub fn primary(&self) -> &TipManager {
        &self.managers[0]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(label: &'static str, blocking: bool, fallback: Vec<CrankStep>) -> CrankStep {
        CrankStep {
            label,
            txs: SmallVec::new(),
            blocking,
            fallback,
        }
    }

    /// Run `steps` where every label in `failing` fails; returns the chain result, the labels
    /// run in order, and the labels reported as failed for good.
    fn drive(
        steps: Vec<CrankStep>,
        failing: &[&str],
    ) -> (CrankChainResult, Vec<&'static str>, Vec<&'static str>) {
        let mut ran = vec![];
        let mut failed = vec![];
        let result = CrankStep::run_chain(
            steps,
            &mut |step| {
                ran.push(step.label);
                if failing.contains(&step.label) {
                    Err(())
                } else {
                    Ok(())
                }
            },
            &mut |step, ()| failed.push(step.label),
        );
        (result, ran, failed)
    }

    fn merged_chain() -> Vec<CrankStep> {
        vec![
            step("init_tda", true, vec![]),
            step(
                "merged",
                true,
                vec![
                    step("receiver", true, vec![]),
                    step("builder", false, vec![]),
                ],
            ),
        ]
    }

    #[test]
    fn test_run_chain_merged_step_commits_in_one_go() {
        let (result, ran, failed) = drive(merged_chain(), &[]);
        assert_eq!(result, CrankChainResult::Committed);
        assert_eq!(ran, ["init_tda", "merged"]);
        assert!(failed.is_empty());
    }

    #[test]
    fn test_run_chain_falls_back_to_split_steps() {
        // The merged transaction is rejected; the split steps do its work and nothing is
        // reported as failed.
        let (result, ran, failed) = drive(merged_chain(), &["merged"]);
        assert_eq!(result, CrankChainResult::Committed);
        assert_eq!(ran, ["init_tda", "merged", "receiver", "builder"]);
        assert!(failed.is_empty());

        // A block builder the program rejects costs only that step, as before the merge.
        let (result, ran, failed) = drive(merged_chain(), &["merged", "builder"]);
        assert_eq!(result, CrankChainResult::Degraded);
        assert_eq!(ran, ["init_tda", "merged", "receiver", "builder"]);
        assert_eq!(failed, ["builder"]);

        // The receiver failing in the fallback breaks the chain, as it always did.
        let (result, ran, failed) = drive(merged_chain(), &["merged", "receiver"]);
        assert_eq!(result, CrankChainResult::Broken);
        assert_eq!(ran, ["init_tda", "merged", "receiver"]);
        assert_eq!(failed, ["receiver"]);
    }

    #[test]
    fn test_run_chain_blocking_failure_stops_the_chain() {
        let (result, ran, failed) = drive(merged_chain(), &["init_tda"]);
        assert_eq!(result, CrankChainResult::Broken);
        assert_eq!(ran, ["init_tda"]);
        assert_eq!(failed, ["init_tda"]);
    }
}
