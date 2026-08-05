import { describe, expect, it } from "vitest";
import { followerExpiry } from "../src/timelock.js";

const policy = { safetyWindow: 7200n, minFollowerLock: 1800n };

describe("followerExpiry", () => {
  it("leaves a safety window before the initiator's expiry", () => {
    const r = followerExpiry(100_000n, 0n, policy);
    expect(r).toEqual({ ok: true, expiry: 100_000n - 7200n });
  });

  it("rejects locks that leave too little time", () => {
    const now = 1_000n;
    const r = followerExpiry(now + 7200n + 1799n, now, policy);
    expect(r.ok).toBe(false);
  });

  it("accepts the exact minimum", () => {
    const now = 1_000n;
    const r = followerExpiry(now + 7200n + 1800n, now, policy);
    expect(r.ok).toBe(true);
  });
});
