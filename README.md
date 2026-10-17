# Pactum

[![ci](https://github.com/shaikyajash/pactum/actions/workflows/ci.yml/badge.svg)](https://github.com/shaikyajash/pactum/actions/workflows/ci.yml)

**Gasless, partially fillable cross-chain swaps from EVM chains to EVM, Bitcoin
and Solana, with no bridge and no custodian.**

A user signs one EIP-712 message saying *"sell 1000 USDC on Ethereum for at
least 0.25 ETH on Base"*, or *"…for native BTC"*, or *"…for native SOL"*.
Resolvers compete in a Dutch auction to fill it, possibly in several pieces.
Settlement is enforced by hash time-locked contracts on every chain, all locked
behind the same `sha256` secret, so either both legs of every fill happen or
neither does.

| | |
|---|---|
| **EVM** (Solidity / Foundry) | `HashLock` HTLC · `PactumIntents` signed orders, Dutch auction, Merkle-committed partial fills · `SecretRelay` Chainlink CCIP auto-settlement |
| **Bitcoin** (Rust / rust-bitcoin) | Taproot HTLC: script-path only, `OP_SIZE 32` + `sha256` redeem leaf, absolute `OP_CHECKLOCKTIMEVERIFY` refund leaf |
| **Solana** (native program) | PDA escrows per swap; permissionless redeem to a fixed redeemer; Unix-time refunds |
| **Off-chain** (Rust / alloy / tokio / axum) | `pactum-core` protocol math · `pactum-relay` untrusted order relay · `pactum-btc` / `pactum-sol` chain clients · `pactum-node` maker agent, resolver, keeper and `pactum` CLI |

## How an intent settles

```mermaid
sequenceDiagram
    autonumber
    participant M as Maker
    participant R as Relay (untrusted)
    participant X as Resolver
    participant A as PactumIntents @ src
    participant B as PactumIntents @ dst

    M->>M: N+1 secrets, Merkle root over their hashes
    M->>R: signed Order (root, auction, parts) + public secret hashes
    X->>R: poll open orders
    Note over X: wait until auction price ≤ my bid
    X->>A: fill(order, sig, slice, i, hashᵢ, proof)  — pulls maker's tokens into HTLC → X
    X->>B: initiate(maker, owed, hashᵢ, shorter lock)  — HTLC → maker
    M->>B: verify lock: right token, amount ≥ owed, safe timelock
    M->>R: release secretᵢ
    X->>B: redeem(secretᵢ)  — maker paid, X pays gas
    X->>A: redeem(secretᵢ)  — X paid
```

With a permit-capable token (USDC, DAI, …) the maker signs an EIP-2612 permit
alongside the order and **never sends a transaction on any chain**, not even an
approval.

## What makes it interesting

### 1. Gasless for the maker
`PactumIntents.fill` opens the source escrow from the maker's allowance using
their signature, and `redeem` is permissionless but always pays the escrow's
fixed redeemer. So the resolver can execute **every** transaction, including
the one that pays the maker, without being able to redirect a single wei.

### 2. Dutch-auction price discovery
The ask decays linearly from `startDstAmount` to `endDstAmount`. Each resolver
fills the moment the price crosses its own bid, so the maker gets the best
price the market will support without running an RFQ. The amount owed for a
fill is fixed on-chain at fill time (`OrderFilled.owedDstAmount`, rounded up
for the maker).

### 3. Partial fills without secret reuse
An order with `parts = N` commits to **N+1 secrets** in a Merkle tree. A fill
must use the secret whose index matches the cumulative fill level *after* it,
and indices only move forward:

```
parts = 4, srcAmount = 100
filled after:   1..25 → secret 0    26..50 → 1    51..75 → 2    76..99 → 3    100 → 4
```

Once a secret is revealed to settle one fill, it is public. Because no later
fill can lock behind it, nobody can use it to drain a later escrow. Leaves bind
`(index, hash)`, so a proof for secret 3 can never be passed off as secret 1.

### 4. Safety deposits keep the failure path gasless too
Every fill posts a native deposit (`order.minDeposit`), credited to a pull
ledger when the escrow settles:

| Settlement | Who is credited |
|---|---|
| `redeem` (swap completed) | the resolver gets its deposit back, whoever submits the secret |
| `instantRefund` (resolver consented) | the resolver |
| `refund` after expiry (fill abandoned) | **10%** to the keeper who called it, **90%** to the maker |

So if a resolver locks the maker's funds and walks away, a keeper running
`pactum keeper` is paid to refund the maker, and the maker is compensated
for the wait. A griefer who refunds itself still forfeits 90% of its deposit.
Credits are withdrawn with `withdrawDeposits`, so a party that rejects ETH can
never block settlement.

### 5. Chainlink CCIP for liveness, never for safety
`SecretRelay` carries revealed secrets to the other chain over CCIP and redeems
the other leg automatically. A forged or replayed message can't move funds
anywhere they couldn't already go, because `redeem` re-checks `sha256(secret)`
and pays a fixed address (fuzzed in `testFuzz_forgedMessagesCannotMisdirectFunds`).
Each entry gets a fixed gas budget, so one hostile redeemer can't starve the rest
of a batch, and an under-funded delivery reverts so CCIP can re-execute it.

## Beyond EVM: Bitcoin and Solana

Orders are chain-agnostic. `receiver` and `dstToken` are 32-byte words: a
left-padded EVM address, a Solana pubkey, or a Bitcoin x-only key. EVM chains
use their chain id as `dstChainId`; Bitcoin and Solana use
`keccak256(CAIP-2 id)`, which can't collide with a real EVM id
([`chains.rs`](crates/pactum-core/src/chains.rs)).

|  | EVM destination | Bitcoin destination | Solana destination |
|---|---|---|---|
| Counter-lock | `HashLock.initiate` | Taproot HTLC output | `pactum-solana` escrow PDA |
| How the maker finds it | `Initiated` events | relay announcement, verified on chain | relay announcement, verified on chain |
| Who claims for the maker | the resolver (permissionless redeem) | **the maker** (the redeem leaf needs the maker's signature) | the resolver (permissionless redeem) |
| Maker pays fees | never | the claim fee, taken out of the HTLC itself | never |
| How the resolver learns the secret | relay / `Redeemed` event | the maker's claim witness, read from Bitcoin | relay / the escrow account |
| Refund clock | block timestamp | Unix-time `OP_CLTV` against median-time-past | `Clock::unix_timestamp` |

Design points worth knowing:
- **Every chain accepts only 32-byte secrets.** Bitcoin scripts enforce
  `OP_SIZE 32`. If any leg accepted a longer preimage, a maker could pick a
  secret that unlocks the EVM leg but can never unlock the Bitcoin leg.
- **Bitcoin refunds use absolute time, not block counts.** A relative `CSV`
  lock drifts against the EVM leg's wall-clock expiry, and slow blocks would let
  the maker refund on EVM *and* claim the BTC. The refund lock is set to
  `src_expiry - safety_window - MTP_LAG_SECS`, since Bitcoin checks
  timestamps against median-time-past, which trails wall time.
- **The Bitcoin redeem leaf needs the maker's signature**, not just the secret.
  Otherwise anyone who saw the secret in the mempool could race the claim to
  their own address.
- **The Solana client is SDK-free.** `pactum-sol` encodes legacy transactions,
  derives PDAs and signs with ed25519 directly against the wire format, and is
  validated against a real `solana-test-validator`.

Native assets only for now (BTC, SOL); SPL tokens and BTC→EVM (maker locking
BTC first) are on the roadmap.

## Security model

| Threat | Why it fails |
|---|---|
| Resolver takes the maker's tokens and doesn't pay | The maker releases secretᵢ only after verifying the destination lock pays ≥ owed |
| Resolver underpays | Owed amount is fixed on-chain at fill time; the maker's agent rejects short locks and the source escrow refunds |
| Maker claims dst, then refunds src | Destination lock expires `safetyWindow` before the source lock ([`timelock.rs`](crates/pactum-core/src/timelock.rs)) |
| A revealed secret is reused for a later fill | Secret indices are strictly increasing per order (`WrongSecretIndex`) |
| Resolver tampers with order terms | Any change alters the EIP-712 digest → `InvalidSignature` |
| Relay lies | It stores only verifiable data; signatures, roots and preimages are re-checked on-chain and by agents |
| Smart-contract wallet makers | EIP-1271 (and EIP-7702 delegated EOAs) for fills and instant refunds |
| A resolver abandons a fill | Keeper refunds after expiry; the resolver forfeits 90% of its deposit to the maker |
| MEV takes a resolver's deposit by redeeming first | Deposits are credited to the swap's redeemer, not the caller |
| Resolver opens a counter-lock that expires minutes later, refunds it after the secret is revealed | Makers require `min_follower_lock` of remaining time before releasing, and redeem a released lock themselves if the resolver leaves it open too long |
| Secret of the wrong length unlocks one chain but not another | 32-byte secrets enforced in Solidity, the Bitcoin script, the Solana program and the relay |
| Cross-chain replay | Swap ids and the EIP-712 domain commit to `chainid` and contract address |
| Non-standard ERC20s | `SafeTransfer` handles USDT-style tokens that return nothing |

**Not supported:** fee-on-transfer tokens (rejected on deposit with
`UnsupportedToken`) and rebasing tokens.

The contracts went through an internal security review; findings and fixes are
recorded in [SECURITY.md](SECURITY.md).

## Testing

```
forge test        116 Solidity tests: unit, fuzz, stateful invariants, audit regressions
cargo test        48 Rust tests: unit, HTTP, Bitcoin Core regtest, solana-test-validator,
                  and seven cross-chain e2e scenarios
```

Highlights:
- **Invariants** (`test/invariant/`): balance conservation and settlement
  finality for `HashLock`; no overfill, escrow = fills, one-use secrets, and
  native balance = open deposits + credits under random concurrent fills.
- **Cross-language vectors**: the Rust Merkle tree, swap ids and EIP-712 type
  hash are checked against values produced by the Solidity code.
- **End-to-end** ([`e2e.rs`](crates/pactum-node/tests/e2e.rs)), on two live anvil
  chains with a real relay:
  - two resolvers split one permit-signed order at different auction prices, and
    the maker's transaction count is **0 on both chains**;
  - a rogue resolver abandons a fill, and a keeper refunds the maker for its tip
    while the maker is credited 90% of the forfeited deposit;
  - a resolver crashes mid-swap and a fresh process settles from its state file;
  - **USDC on EVM → native BTC** against a real Bitcoin Core node (regtest);
  - **USDC on EVM → native SOL** against a real `solana-test-validator`, where
    the maker's Solana account doesn't exist until the SOL arrives;
  - two regression scenarios for the short-counter-lock attack and its rescue.

## Layout

```
src/
  HashLock.sol               HTLC: initiate / redeem / refund / instantRefund
  PactumIntents.sol          signed orders, Dutch auction, Merkle partial fills
  lib/MerkleSecrets.sol      index-bound secret commitments
  lib/SafeTransfer.sol       safe ERC20 + native transfers
  interfaces/IERC20Permit.sol
  ccip/SecretRelay.sol       Chainlink CCIP app for automatic settlement
  ccip/Client.sol            CCIP message types (layout-compatible with Chainlink)
programs/
  pactum-solana/             native Solana HTLC program (cargo-build-sbf)
crates/
  pactum-core/               orders, EIP-712, secret trees, auction, timelocks,
                             CAIP-2 chain registry
  pactum-btc/                Taproot HTLC + Bitcoin Core RPC
  pactum-sol/                SDK-free Solana client: tx encoding, PDAs, RPC
  pactum-relay/              axum relay server + reqwest client
  pactum-node/               alloy chain adapter, Maker, IntentFiller, Keeper,
                             crash-safe state, and the `pactum` CLI
script/                      CREATE2 deploys for PactumIntents and SecretRelay
test/                        Foundry unit, fuzz, invariant and e2e tests
```

## Running it

Requires [Foundry](https://getfoundry.sh), Rust 1.85+ and Docker (for the
Bitcoin Core and Solana validator tests).

```bash
git clone --recursive https://github.com/shaikyajash/pactum && cd pactum
make solana   # builds the Solana toolchain image and the SBF program
make test
```

The Solana image is pinned to Agave 2.3 because 3.x requires `io_uring`, which
Docker's x86 emulation on Apple Silicon doesn't provide.

Deploy (same address on every chain):

```bash
forge script script/Deploy.s.sol --rpc-url $RPC --broadcast --account deployer
```

Run a relay, a resolver and an intent (amounts in base units):

```bash
cargo run --release -- relay --listen 0.0.0.0:8080

RESOLVER_KEY=0x... cargo run --release -- resolve -c config.json \
  --rate-num 300000000000000000 --rate-den 1000000000 --max-fill-bps 6000 \
  --state resolver.json

MAKER_KEY=0x... cargo run --release -- intent -c config.json \
  --from sepolia --to base-sepolia \
  --sell-token 0xUSDC --sell-amount 1000000000 \
  --start 500000000000000000 --floor 250000000000000000 --parts 4 \
  --min-deposit 1000000000000000 --permit-version 2 --state maker.json

KEEPER_KEY=0x... cargo run --release -- keeper -c config.json
```

## Roadmap

- [x] Resolver safety deposits with keeper tips and maker compensation
- [x] EIP-2612 permits, so the maker never sends a transaction
- [x] Persistent maker and resolver state
- [x] Chainlink CCIP secret relay
- [x] Native BTC and SOL destinations
- [ ] SPL tokens on Solana
- [ ] BTC → EVM (maker funds a Bitcoin HTLC first)
- [ ] Testnet deployments (Sepolia, Base Sepolia, Bitcoin testnet4, Solana devnet)
- [ ] Permit2 for tokens without EIP-2612
- [ ] Route secrets through `SecretRelay` from the resolver daemon

## License

MIT
