/**
 * Timelock policy for the follower (counter-party) leg.
 *
 * The initiator holds the secret, so they can wait until the very end of the
 * follower's lock to redeem it. The follower then needs enough time to see the
 * secret and redeem on the initiator's chain *before* that lock expires —
 * otherwise the initiator could take both legs. So:
 *
 *   followerExpiry + safetyWindow <= initiatorExpiry
 */
export interface TimelockPolicy {
  /** Time the follower needs to observe the secret and land its redeem tx. */
  safetyWindow: bigint;
  /** Shortest follower lock worth committing capital to. */
  minFollowerLock: bigint;
}

export const DEFAULT_POLICY: TimelockPolicy = {
  safetyWindow: 2n * 60n * 60n, // 2h
  minFollowerLock: 30n * 60n, // 30m
};

export type FollowerExpiry =
  | { ok: true; expiry: bigint }
  | { ok: false; reason: string };

export function followerExpiry(
  initiatorExpiry: bigint,
  now: bigint,
  policy: TimelockPolicy = DEFAULT_POLICY,
): FollowerExpiry {
  const expiry = initiatorExpiry - policy.safetyWindow;
  if (expiry - now < policy.minFollowerLock) {
    return { ok: false, reason: "initiator lock too short for a safe counter-leg" };
  }
  return { ok: true, expiry };
}
