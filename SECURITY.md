# Security

Pactum has **not** had a professional third-party audit. Do not use it with
real funds.

## Internal review (October 2026)

The contracts went through two internal reviews: one of `HashLock` and
`PactumIntents`, and a separate one of the CCIP `SecretRelay`. Each finding was
confirmed with a proof-of-concept test before it was fixed, and every fix ships
with a regression test that asserts the attack now fails.

Neither review found anything Critical or High. In particular, the core safety
property held throughout: a maker cannot lose funds without being paid at least
the amount owed on the destination chain, and a forged relay message cannot
move funds anywhere they could not already go.

### HashLock / PactumIntents

| ID | Severity | Finding | Fix | Regression test |
|---|---|---|---|---|
| M-1 | Medium | The safety deposit was paid to whoever settled first, so a bot copying the public secret could take an honest resolver's deposit | Deposits are credited to a pull ledger; `redeem` always credits the swap's redeemer | `Deposits.t.sol::test_frontRunRedeemDoesNotStealDeposit` |
| M-2 | Medium | A griefer could lock a maker's order, refund itself after expiry and recover its whole deposit, so griefing cost only gas | An expiry refund credits a 10% keeper tip to the caller and 90% to the maker | `test_selfRefundGrieferLosesMostOfDeposit` |
| L-1 | Low | Fee-on-transfer tokens left escrows short of funds | Funding checks the balance received and reverts with `UnsupportedToken` | `test_erc20_rejectsFeeOnTransferToken`, `test_fillRejectsFeeOnTransferToken` |
| L-2 | Low | Settlement reverted if the deposit could not be pushed to the caller (contracts without `receive`, blacklisted payouts) | Deposits are never sent inline; parties call `withdrawDeposits` / `withdrawDepositsTo` | `test_contractCallerWithoutReceiveCanRedeem`, `test_settlementSucceedsWhenRedeemerRejectsEth` |
| L-3 | Low | `instantRefund` only accepted EOA signatures, but redeemers are usually contracts | Shared EIP-1271 signature check | `test_instantRefund_contractRedeemerViaEip1271` |
| I-1 | Info | EIP-7702 delegated EOAs were forced down the EIP-1271 path | EIP-1271 first, then fall back to ECDSA | `test_7702DelegatedMakerSignsWithEcdsa` |
| I-2 | Info | EIP-1271 signatures are checked only on the first fill | Documented: makers revoke with `cancel` | — |
| I-3 | Info | Extreme amounts (~2²⁴⁶) overflow pricing math | Reverts only (order unfillable); accepted | — |
| I-4 | Info | Orders sign under the `HashLock` v1 EIP-712 domain | Documented; off-chain signers match it | — |

### SecretRelay (Chainlink CCIP)

| ID | Severity | Finding | Fix | Regression test |
|---|---|---|---|---|
| M-1 | Medium | A redeemer that burns gas could starve later entries in a batch, and an under-gassed message could be marked delivered without redeeming | Fixed per-entry gas budget; a delivery without enough gas reverts the whole message so CCIP can re-execute it | `SecretRelayGas.t.sol::test_gasBurnerCannotStarveFollowingEntry`, `test_underGassedDeliveryNeverSwallowsEntries` |
| L-1 | Low | A re-entrant `claim()` could make the deposit balance-delta underflow and revert a batch | The relay no longer handles ETH at all; HashLock credits deposits directly | `Deposits.t.sol::test_relayedRedeemCreditsRedeemer` |
| L-2 | Low | Deposits owed to redeemers that cannot receive ETH were stuck | Same: deposits go to the ledger, and `withdrawDepositsTo` routes them anywhere | `test_relayedRedeemForEthRejectingRedeemer` |
| I-1 | Info | Balance-delta attribution relied on the CCIP OffRamp's reentrancy guard | Same as L-1 | `test_relayCannotReceiveEth` |
| I-2 | Info | Single-step ownership; a zero owner was allowed | Two-step ownership; the zero address is rejected | `test_admin_twoStepOwnershipTransfer` |
| I-3 | Info | Swaps paying the relay itself would trap funds | Resolved by removing ETH custody from the relay | — |

The review also confirmed that `src/ccip/Client.sol` matches Chainlink CCIP:
struct layouts, the `GenericExtraArgsV2` tag and encoding, and the
`IAny2EVMMessageReceiver` interface id.

## Found during the multichain work (October 2026)

These came up while adding Bitcoin and Solana, and were fixed in the same
change set:

| Severity | Finding | Fix | Regression test |
|---|---|---|---|
| High | A maker released its secret for any counter-lock expiring a safety window before the source escrow, without checking the lock would stay open long enough. A resolver could lock for a few minutes, wait for the secret, refund its lock and claim the source leg with the public secret | Makers require `min_follower_lock` of remaining time, and redeem a released lock themselves if the resolver leaves it open into the last half of that window | `e2e.rs::maker_refuses_short_counter_locks`, `maker_rescues_counter_lock_the_resolver_abandons` (the first fails on the pre-fix code) |
| Medium | `HashLock` accepted preimages of any length; Bitcoin legs require exactly 32 bytes, so a maker could choose a secret that unlocks EVM but never the Bitcoin counter-leg | 32-byte secrets enforced in `redeem`, the Solana program and the relay | `HashLock.t.sol::test_redeem_rejectsNon32ByteSecretEvenIfHashMatches` |
| Medium | The first Bitcoin HTLC used a relative block-count refund lock, which drifts against the EVM leg's wall-clock expiry | Absolute `OP_CHECKLOCKTIMEVERIFY` refund at a Unix time, with a median-time-past margin | `pactum-btc/tests/regtest.rs::refund_after_absolute_lock` |

## Known limitations

- Rebasing tokens are unsupported, and a balance check cannot detect them.
- A native-asset swap whose redeemer rejects ETH cannot be redeemed to it.
  Use WETH for contract redeemers.
- On an expiry refund the deposit goes mostly to the maker. A maker who
  withholds a secret after a valid counter-lock therefore costs the resolver
  its deposit, so resolvers should stop filling for such makers.
- A griefed maker must re-sign with a fresh `secretsRoot`. Never reuse a
  `secretsRoot` across orders.
- The Bitcoin claim uses a flat fee; a production agent should estimate fees
  and be ready to bump with CPFP, since the claim must confirm before the
  refund lock opens.
- The Solana program and the Bitcoin scripts are outside the internal review
  above and have only been tested, not reviewed.

## Reporting

Please report vulnerabilities privately through GitHub security advisories on
this repository rather than in a public issue.
