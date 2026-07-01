//! Minimal in-crate SDK shim replacing the `jito-tip-distribution` and `jito-tip-payment`
//! on-chain program crates (which pull in `anchor-lang` and a conflicting `solana-program`).
//!
//! Layouts, discriminators and account orderings mirror the deployed jito-programs at
//! commit `ad91ebfb0eaa10dc0d5cb896f542609881ee0705` (jito v0.1.10 `.so` binaries):
//!   * tip-payment  `T1pyyaTNZsKv2WcRAB8oVnk93mLJw2XzjtVYqCsaHqt`
//!   * tip-distrib  `4R3gSG8BpU4t19KYj8CfnbtRpnT8gtk4dvTHxVRwc2r7`
//!
//! Cross-checked against `core/src/tip_manager/tip_distribution.rs` &
//! `core/src/tip_manager/tip_payment.rs` (known-correct against the same `.so`):
//!   * Config account discriminator `[155,12,170,224,30,250,204,130]` matches
//!     `JitoTipDistributionConfig::DISCRIMINATOR`.
//!   * Anchor account/instruction discriminators are the 8-byte sha256 sighashes
//!     (`account:<Name>` / `global:<snake_name>`).
//!
//! Anchor's 8-byte account discriminator precedes every account's borsh body
//! (`HEADER_SIZE == 8`). `AccountDeserialize::try_deserialize` skips it.

#![allow(clippy::arithmetic_side_effects)]

use {
    borsh::{BorshDeserialize, BorshSerialize},
    solana_instruction::AccountMeta,
    solana_pubkey::Pubkey,
};

/// Anchor account header (8-byte discriminator) size.
pub const HEADER_SIZE: usize = 8;

/// Minimal replacement for `anchor_lang::AccountDeserialize`: reads (and discards) the
/// 8-byte anchor discriminator, then borsh-deserializes the remaining bytes.
pub trait AccountDeserialize: Sized {
    fn try_deserialize(buf: &mut &[u8]) -> Result<Self, std::io::Error>;
}

fn deserialize_with_discriminator<T: BorshDeserialize>(
    buf: &mut &[u8],
) -> Result<T, std::io::Error> {
    if buf.len() < HEADER_SIZE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "account data too short for anchor discriminator",
        ));
    }
    *buf = &buf[HEADER_SIZE..];
    T::deserialize(buf)
}

pub mod merkle_proof {
    //! Mirror of `jito_tip_distribution::merkle_proof::verify`.
    pub fn verify(proof: Vec<[u8; 32]>, root: [u8; 32], leaf: [u8; 32]) -> bool {
        let mut computed_hash = leaf;
        for proof_element in proof.into_iter() {
            if computed_hash <= proof_element {
                computed_hash =
                    solana_program::hash::hashv(&[&[1u8], &computed_hash, &proof_element])
                        .to_bytes();
            } else {
                computed_hash =
                    solana_program::hash::hashv(&[&[1u8], &proof_element, &computed_hash])
                        .to_bytes();
            }
        }
        computed_hash == root
    }
}

pub mod state {
    use super::*;

    #[derive(Clone, Default, BorshSerialize, BorshDeserialize, Debug, PartialEq, Eq)]
    pub struct MerkleRoot {
        pub root: [u8; 32],
        pub max_total_claim: u64,
        pub max_num_nodes: u64,
        pub total_funds_claimed: u64,
        pub num_nodes_claimed: u64,
    }

    #[derive(Clone, Default, BorshSerialize, BorshDeserialize, Debug, PartialEq, Eq)]
    pub struct TipDistributionAccount {
        pub validator_vote_account: Pubkey,
        pub merkle_root_upload_authority: Pubkey,
        pub merkle_root: Option<MerkleRoot>,
        pub epoch_created_at: u64,
        pub validator_commission_bps: u16,
        pub expires_at: u64,
        pub bump: u8,
    }

    impl TipDistributionAccount {
        pub const SEED: &'static [u8] = b"TIP_DISTRIBUTION_ACCOUNT";
        /// `HEADER_SIZE + size_of::<Self>()` per the on-chain program.
        pub const SIZE: usize = HEADER_SIZE + std::mem::size_of::<Self>();
        pub const DISCRIMINATOR: [u8; 8] = [85, 64, 113, 198, 234, 94, 120, 123];
    }

    impl AccountDeserialize for TipDistributionAccount {
        fn try_deserialize(buf: &mut &[u8]) -> Result<Self, std::io::Error> {
            deserialize_with_discriminator(buf)
        }
    }

    #[derive(Clone, Default, BorshSerialize, BorshDeserialize, Debug, PartialEq, Eq)]
    pub struct ClaimStatus {
        pub is_claimed: bool,
        pub claimant: Pubkey,
        pub claim_status_payer: Pubkey,
        pub slot_claimed_at: u64,
        pub amount: u64,
        pub expires_at: u64,
        pub bump: u8,
    }

    impl ClaimStatus {
        pub const SEED: &'static [u8] = b"CLAIM_STATUS";
        pub const SIZE: usize = HEADER_SIZE + std::mem::size_of::<Self>();
        pub const DISCRIMINATOR: [u8; 8] = [22, 183, 249, 157, 247, 95, 150, 96];
    }

    impl AccountDeserialize for ClaimStatus {
        fn try_deserialize(buf: &mut &[u8]) -> Result<Self, std::io::Error> {
            deserialize_with_discriminator(buf)
        }
    }

    #[derive(Clone, Default, BorshSerialize, BorshDeserialize, Debug, PartialEq, Eq)]
    pub struct Config {
        pub authority: Pubkey,
        pub expired_funds_account: Pubkey,
        pub num_epochs_valid: u64,
        pub max_validator_commission_bps: u16,
        pub bump: u8,
    }

    impl Config {
        pub const SEED: &'static [u8] = b"CONFIG_ACCOUNT";
        pub const SIZE: usize = HEADER_SIZE + std::mem::size_of::<Self>();
        pub const DISCRIMINATOR: [u8; 8] = [155, 12, 170, 224, 30, 250, 204, 130];
    }

    impl AccountDeserialize for Config {
        fn try_deserialize(buf: &mut &[u8]) -> Result<Self, std::io::Error> {
            deserialize_with_discriminator(buf)
        }
    }
}

/// Replaces `jito_tip_distribution::instruction::*` (anchor codegen): instruction data is
/// the 8-byte global sighash followed by the borsh-encoded args.
pub mod instruction {
    use super::*;

    const CLAIM_DISCRIMINATOR: [u8; 8] = [62, 198, 214, 193, 213, 159, 108, 210];

    pub struct Claim {
        pub proof: Vec<[u8; 32]>,
        pub amount: u64,
        pub bump: u8,
    }

    impl Claim {
        /// On-chain handler arg order is `(bump, amount, proof)`.
        pub fn data(&self) -> Vec<u8> {
            let mut data = Vec::with_capacity(8 + 1 + 8 + 4 + self.proof.len() * 32);
            data.extend_from_slice(&CLAIM_DISCRIMINATOR);
            self.bump.serialize(&mut data).unwrap();
            self.amount.serialize(&mut data).unwrap();
            self.proof.serialize(&mut data).unwrap();
            data
        }
    }
}

/// Replaces `jito_tip_distribution::accounts::*` (anchor codegen): metas are emitted in the
/// on-chain `#[derive(Accounts)]` struct declaration order.
pub mod accounts {
    use super::*;

    pub struct Claim {
        pub config: Pubkey,
        pub tip_distribution_account: Pubkey,
        pub merkle_root_upload_authority: Pubkey,
        pub claimant: Pubkey,
        pub claim_status: Pubkey,
        pub payer: Pubkey,
        pub system_program: Pubkey,
    }

    impl Claim {
        /// On-chain `Claim` accounts order (jito-programs tip-distribution 0.1.10):
        /// config, tip_distribution_account (mut), merkle_root_upload_authority (signer),
        /// claim_status (init/mut), claimant (mut), payer (mut, signer), system_program.
        /// NOTE: claims are PERMISSIONED — the `merkle_root_upload_authority` must sign
        /// (program comment: "Only the merkle_root_upload_authority has the authority to claim").
        pub fn to_account_metas(&self) -> Vec<AccountMeta> {
            vec![
                AccountMeta::new_readonly(self.config, false),
                AccountMeta::new(self.tip_distribution_account, false),
                AccountMeta::new_readonly(self.merkle_root_upload_authority, true),
                AccountMeta::new(self.claim_status, false),
                AccountMeta::new(self.claimant, false),
                AccountMeta::new(self.payer, true),
                AccountMeta::new_readonly(self.system_program, false),
            ]
        }
    }
}

/// Replaces `jito_tip_distribution::sdk::*` helpers used by the upload / reclaim workflows.
pub mod sdk {
    use {super::*, solana_instruction::Instruction};

    pub fn derive_config_account_address(tip_distribution_program_id: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[state::Config::SEED], tip_distribution_program_id)
    }

    pub mod instruction {
        use super::*;

        const UPLOAD_MERKLE_ROOT_DISCRIMINATOR: [u8; 8] = [70, 3, 110, 29, 199, 190, 205, 176];
        const CLOSE_CLAIM_STATUS_DISCRIMINATOR: [u8; 8] = [163, 214, 191, 165, 245, 188, 17, 185];
        const CLOSE_TIP_DISTRIBUTION_ACCOUNT_DISCRIMINATOR: [u8; 8] =
            [47, 136, 208, 190, 125, 243, 74, 227];

        pub struct UploadMerkleRootArgs {
            pub root: [u8; 32],
            pub max_total_claim: u64,
            pub max_num_nodes: u64,
        }

        pub struct UploadMerkleRootAccounts {
            pub config: Pubkey,
            pub merkle_root_upload_authority: Pubkey,
            pub tip_distribution_account: Pubkey,
        }

        pub fn upload_merkle_root_ix(
            program_id: Pubkey,
            args: UploadMerkleRootArgs,
            accounts: UploadMerkleRootAccounts,
        ) -> Instruction {
            let mut data = Vec::with_capacity(8 + 32 + 16);
            data.extend_from_slice(&UPLOAD_MERKLE_ROOT_DISCRIMINATOR);
            args.root.serialize(&mut data).unwrap();
            args.max_total_claim.serialize(&mut data).unwrap();
            args.max_num_nodes.serialize(&mut data).unwrap();
            // On-chain UploadMerkleRoot accounts order:
            // config, tip_distribution_account (mut), merkle_root_upload_authority (mut, signer).
            Instruction {
                program_id,
                data,
                accounts: vec![
                    AccountMeta::new_readonly(accounts.config, false),
                    AccountMeta::new(accounts.tip_distribution_account, false),
                    AccountMeta::new(accounts.merkle_root_upload_authority, true),
                ],
            }
        }

        pub struct CloseClaimStatusArgs;

        pub struct CloseClaimStatusAccounts {
            pub config: Pubkey,
            pub claim_status: Pubkey,
            pub claim_status_payer: Pubkey,
        }

        pub fn close_claim_status_ix(
            program_id: Pubkey,
            _args: CloseClaimStatusArgs,
            accounts: CloseClaimStatusAccounts,
        ) -> Instruction {
            // On-chain CloseClaimStatus accounts order:
            // config, claim_status (mut, close), claim_status_payer (mut).
            Instruction {
                program_id,
                data: CLOSE_CLAIM_STATUS_DISCRIMINATOR.to_vec(),
                accounts: vec![
                    AccountMeta::new_readonly(accounts.config, false),
                    AccountMeta::new(accounts.claim_status, false),
                    AccountMeta::new(accounts.claim_status_payer, false),
                ],
            }
        }

        pub struct CloseTipDistributionAccountArgs {
            pub _epoch: u64,
        }

        pub struct CloseTipDistributionAccounts {
            pub config: Pubkey,
            pub tip_distribution_account: Pubkey,
            pub validator_vote_account: Pubkey,
            pub expired_funds_account: Pubkey,
            pub signer: Pubkey,
        }

        pub fn close_tip_distribution_account_ix(
            program_id: Pubkey,
            args: CloseTipDistributionAccountArgs,
            accounts: CloseTipDistributionAccounts,
        ) -> Instruction {
            let mut data = Vec::with_capacity(8 + 8);
            data.extend_from_slice(&CLOSE_TIP_DISTRIBUTION_ACCOUNT_DISCRIMINATOR);
            args._epoch.serialize(&mut data).unwrap();
            // On-chain CloseTipDistributionAccount accounts order:
            // config, expired_funds_account (mut), tip_distribution_account (mut, close),
            // validator_vote_account (mut), signer (mut, signer).
            Instruction {
                program_id,
                data,
                accounts: vec![
                    AccountMeta::new_readonly(accounts.config, false),
                    AccountMeta::new(accounts.expired_funds_account, false),
                    AccountMeta::new(accounts.tip_distribution_account, false),
                    AccountMeta::new(accounts.validator_vote_account, false),
                    AccountMeta::new(accounts.signer, true),
                ],
            }
        }
    }
}

/// The tip-distribution program id, replacing `JitoTipDistribution::id()`.
pub fn tip_distribution_program_id() -> Pubkey {
    // `4R3gSG8BpU4t19KYj8CfnbtRpnT8gtk4dvTHxVRwc2r7`
    Pubkey::from_str_const("4R3gSG8BpU4t19KYj8CfnbtRpnT8gtk4dvTHxVRwc2r7")
}

/// Tip-payment config/PDA seeds, replacing the `jito_tip_payment` constants.
pub mod tip_payment {
    pub const CONFIG_ACCOUNT_SEED: &[u8] = b"CONFIG_ACCOUNT";
    pub const TIP_ACCOUNT_SEED_0: &[u8] = b"TIP_ACCOUNT_0";
    pub const TIP_ACCOUNT_SEED_1: &[u8] = b"TIP_ACCOUNT_1";
    pub const TIP_ACCOUNT_SEED_2: &[u8] = b"TIP_ACCOUNT_2";
    pub const TIP_ACCOUNT_SEED_3: &[u8] = b"TIP_ACCOUNT_3";
    pub const TIP_ACCOUNT_SEED_4: &[u8] = b"TIP_ACCOUNT_4";
    pub const TIP_ACCOUNT_SEED_5: &[u8] = b"TIP_ACCOUNT_5";
    pub const TIP_ACCOUNT_SEED_6: &[u8] = b"TIP_ACCOUNT_6";
    pub const TIP_ACCOUNT_SEED_7: &[u8] = b"TIP_ACCOUNT_7";

    use {
        super::{AccountDeserialize, HEADER_SIZE},
        borsh::{BorshDeserialize, BorshSerialize},
        solana_pubkey::Pubkey,
    };

    /// tip-payment `Config`, mirroring
    /// `core/src/tip_manager/tip_payment.rs::JitoTipPaymentConfig`.
    #[derive(Clone, Default, BorshSerialize, BorshDeserialize, Debug, PartialEq, Eq)]
    pub struct Config {
        pub tip_receiver: Pubkey,
        pub block_builder: Pubkey,
        pub block_builder_commission_pct: u64,
        pub bumps: InitBumps,
    }

    impl Config {
        pub const SEED: &'static [u8] = CONFIG_ACCOUNT_SEED;
        pub const SIZE: usize = HEADER_SIZE + std::mem::size_of::<Self>();
        pub const DISCRIMINATOR: [u8; 8] = [155, 12, 170, 224, 30, 250, 204, 130];
    }

    impl AccountDeserialize for Config {
        fn try_deserialize(buf: &mut &[u8]) -> Result<Self, std::io::Error> {
            super::deserialize_with_discriminator(buf)
        }
    }

    #[derive(Clone, Default, BorshSerialize, BorshDeserialize, Debug, PartialEq, Eq)]
    pub struct InitBumps {
        pub config: u8,
        pub tip_payment_account_0: u8,
        pub tip_payment_account_1: u8,
        pub tip_payment_account_2: u8,
        pub tip_payment_account_3: u8,
        pub tip_payment_account_4: u8,
        pub tip_payment_account_5: u8,
        pub tip_payment_account_6: u8,
        pub tip_payment_account_7: u8,
    }
}
