# flowra-validator

**Flowra Validator** — a fork of [jito-solana](https://github.com/jito-foundation/jito-solana) (agave-validator 4.1.0-alpha.0) configured for the Flowra MEV stack.

## Changes from upstream

### CLI additions

Two new flags added to `validator/src/commands/run/args.rs`:

| Flag | Default | Description |
|------|---------|-------------|
| `--bundle-cu-reserve-pct` | `15` | Percentage of block CU budget reserved for bundles (0–100) |
| `--bundle-reserve-release-pct` | `70` | Percentage of bundle CU reservation released when no bundles pending |

These are logged at startup:
```
Flowra bundle CU settings: reserve_pct=15%, release_pct=70%
```

### Tip programs

The validator is configured to use the canonical Jito tip program addresses (baked into genesis):
- `T1pyyaTNZsKv2WcRAB8oVnk93mLJw2XzjtVYqCsaHqt` — tip payment program
- `4R3gSG8BpU4t19KYj8CfnbtRpnT8gtk4dvTHxVRwc2r7` — tip distribution program

### BAM protos

The private Jito BAM (Block Auction Mechanism) protos were reverse-engineered from source references and added to `jito-protos/bam-protos/`:
- `bam_api.proto` — `BamNodeApi` service (InitSchedulerStream, GetBuilderConfig, GetAuthChallenge)
- `bam_types.proto` — all message types (AtomicTxnBatch, LeaderState, ValidatorHeartBeat, etc.)

## Build

```bash
source ~/.cargo/env

# Validator binary
cargo build --release --bin agave-validator

# Supporting tools
cargo build --release --bin solana-keygen
cargo build --release --bin solana-genesis
cargo build --release --bin solana
```

The toolchain is pinned to `1.93.1` via `rust-toolchain.toml`.

## Local testnet setup

### 1. Generate keys

```bash
BIN=./target/release

$BIN/solana-keygen new -o keys/validator-identity.json
$BIN/solana-keygen new -o keys/validator-vote.json
$BIN/solana-keygen new -o keys/validator-stake.json
$BIN/solana-keygen new -o keys/faucet.json
$BIN/solana-keygen new -o keys/merkle-root-upload-authority.json
```

### 2. Create genesis

The tip programs must be baked in at genesis:

```bash
$BIN/solana-genesis \
  --ledger ./ledger \
  --cluster-type development \
  --bootstrap-validator <IDENTITY> <VOTE> <STAKE> \
  --bootstrap-validator-lamports 10000000000000 \
  --bootstrap-validator-stake-lamports 1000000000000 \
  --faucet-pubkey <FAUCET> \
  --faucet-lamports 1000000000000000 \
  --slots-per-epoch 150 \
  --ticks-per-slot 8 \
  --hashes-per-tick sleep \
  --upgradeable-program T1pyyaTNZsKv2WcRAB8oVnk93mLJw2XzjtVYqCsaHqt \
      BPFLoaderUpgradeab1e11111111111111111111111 \
      program-binaries/src/programs/spl-jito_tip_payment-0.1.10.so none \
  --upgradeable-program 4R3gSG8BpU4t19KYj8CfnbtRpnT8gtk4dvTHxVRwc2r7 \
      BPFLoaderUpgradeab1e11111111111111111111111 \
      program-binaries/src/programs/spl-jito_tip_distribution-0.1.10.so none
```

### 3. Run validator

```bash
./target/release/agave-validator \
  --identity          keys/validator-identity.json \
  --vote-account      keys/validator-vote.json \
  --ledger            ./ledger \
  --rpc-port          8899 \
  --rpc-bind-address  127.0.0.1 \
  --full-rpc-api \
  --enable-rpc-transaction-history \
  --dynamic-port-range 8100-8200 \
  --gossip-port       8101 \
  --no-os-network-limits-test \
  --no-poh-speed-test \
  --no-snapshot-fetch \
  --no-genesis-fetch \
  --block-engine-url  http://127.0.0.1:8003 \
  --disable-block-engine-autoconfig \
  --trust-block-engine-packets \
  --tip-payment-program-pubkey      T1pyyaTNZsKv2WcRAB8oVnk93mLJw2XzjtVYqCsaHqt \
  --tip-distribution-program-pubkey 4R3gSG8BpU4t19KYj8CfnbtRpnT8gtk4dvTHxVRwc2r7 \
  --merkle-root-upload-authority    <MERKLE_PUBKEY> \
  --commission-bps    800 \
  --no-wait-for-vote-to-start-leader \
  --bundle-cu-reserve-pct  15 \
  --bundle-reserve-release-pct 70 \
  --log ./logs/validator.log
```

### Key flags

| Flag | Required | Description |
|------|----------|-------------|
| `--block-engine-url` | Yes | flowra-engine validator port (8003) |
| `--disable-block-engine-autoconfig` | Recommended | Use URL as-is (skip RTT probing) |
| `--trust-block-engine-packets` | Recommended | Skip sigverify for engine packets |
| `--tip-payment-program-pubkey` | Yes (voting) | Must match genesis-deployed program |
| `--tip-distribution-program-pubkey` | Yes (voting) | Must match genesis-deployed program |
| `--merkle-root-upload-authority` | Yes (voting) | Authority for merkle root uploads |
| `--enable-rpc-transaction-history` | Yes (testing) | Required for `getSignatureStatuses` to work |
| `--full-rpc-api` | Yes | Required for `prioritization_fee_cache` (needed by BundleStage) |

## Important notes

### Ledger replay

On restart, the validator replays all slots in the ledger before going live. With a fresh genesis this is instant; after running for hours it can take minutes. The `BlockEngineStage` only connects **after** replay is complete.

Use `bash start.sh --fresh` in the testnet scripts to reset the ledger and avoid replay.

### Tip program crank

Every slot, the `BundleStage` calls `handle_tip_programs()` which:
1. Initializes the tip payment config PDA (once)
2. Sets the tip receiver (TipDistribution PDA for current epoch) and block builder

This crank must succeed for any user bundles to execute. If it fails (`tip_programs_error=1` in logs), check that:
- Tip programs are deployed at the correct addresses
- `--block-builder-pubkey` in flowra-engine is a real funded account (not system program)

## Testnet scripts

Use the convenience scripts in `/home/ubuntu/flowra-testnet/`:

```bash
bash start.sh           # start full stack (engine + validator + relayer)
bash start.sh --fresh   # reset ledger from scratch and start
bash stop.sh            # stop all services
bash status.sh          # quick health check
```
