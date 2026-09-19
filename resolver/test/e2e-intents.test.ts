import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { ChildProcess } from "node:child_process";
import { createPublicClient, createWalletClient, erc20Abi, http, parseEther, type Address } from "viem";
import { privateKeyToAccount } from "viem/accounts";
import { Chain } from "../src/chain.js";
import { Maker } from "../src/maker.js";
import { IntentFiller } from "../src/filler.js";
import { RelayClient, RelayServer } from "../src/relay.js";
import { NATIVE } from "../src/swap.js";
import { advance, buildContracts, deploy, hasAnvil, KEYS, mintAbi, startAnvil } from "./helpers.js";

/**
 * Maker sells 1000 USDC on chain A for ETH on chain B via a signed intent.
 * Two resolvers compete in the Dutch auction and split the order. The maker
 * sends no transaction other than a one-time token approval.
 */
describe.skipIf(!hasAnvil)("e2e: gasless split intent, USDC on A -> ETH on B", () => {
  const A = { id: 31337, name: "alpha", rpc: "http://127.0.0.1:18547" };
  const B = { id: 31338, name: "beta", rpc: "http://127.0.0.1:18548" };
  const procs: ChildProcess[] = [];
  const relay = new RelayServer();
  let usdc: Address;
  let chains: { A: typeof A & { contract: Address }; B: typeof B & { contract: Address } };

  beforeAll(async () => {
    buildContracts();
    procs.push(await startAnvil(18547, A.id), await startAnvil(18548, B.id));
    await relay.listen(18600);

    chains = {
      A: { ...A, contract: await deploy(A.rpc, KEYS.deployer, "PactumIntents.sol", "PactumIntents") },
      B: { ...B, contract: await deploy(B.rpc, KEYS.deployer, "PactumIntents.sol", "PactumIntents") },
    };
    usdc = await deploy(A.rpc, KEYS.deployer, "MockERC20.sol", "MockERC20", ["USD Coin", "USDC"]);

    const maker = privateKeyToAccount(KEYS.maker);
    const deployer = createWalletClient({
      account: privateKeyToAccount(KEYS.deployer),
      transport: http(A.rpc),
      chain: null,
    });
    const makerWallet = createWalletClient({ account: maker, transport: http(A.rpc), chain: null });
    const pub = createPublicClient({ transport: http(A.rpc) });
    await pub.waitForTransactionReceipt({
      hash: await deployer.writeContract({
        address: usdc,
        abi: mintAbi,
        functionName: "mint",
        args: [maker.address, 1_000n * 10n ** 18n],
        chain: null,
      }),
    });
    // the maker's only transaction ever: a one-time approval
    await pub.waitForTransactionReceipt({
      hash: await makerWallet.writeContract({
        address: usdc,
        abi: erc20Abi,
        functionName: "approve",
        args: [chains.A.contract, 2n ** 256n - 1n],
        chain: null,
      }),
    });
  }, 60_000);

  afterAll(async () => {
    procs.forEach((p) => p.kill());
    await relay.close();
  });

  it("two resolvers split the order and the maker is paid without sending a tx", async () => {
    const client = new RelayClient("http://127.0.0.1:18600");
    const makerAcct = privateKeyToAccount(KEYS.maker);
    const maker = new Maker(new Chain(chains.A, makerAcct), new Chain(chains.B, makerAcct), client);

    const filler = (key: `0x${string}`, ethPer1000: string, sliceBps: bigint, capacity: bigint) => {
      const acct = privateKeyToAccount(key);
      let used = 0n;
      const f: IntentFiller = new IntentFiller(
        new Map([
          [A.id, new Chain(chains.A, acct)],
          [B.id, new Chain(chains.B, acct)],
        ]),
        client,
        {
          // stop bidding once this resolver's inventory is committed
          bid: (_o, amount) => {
            used = f?.jobs.length ? capacity : 0n;
            if (used + amount > capacity) return undefined;
            return (amount * parseEther(ethPer1000)) / (1_000n * 10n ** 18n);
          },
          maxFill: (o) => (o.srcAmount * sliceBps) / 10_000n,
        },
      );
      return f;
    };
    const usdc1000 = 1_000n * 10n ** 18n;
    // A pays more (fills earlier in the auction) but only has inventory for 60%
    const resolverA = filler(KEYS.deployer, "0.40", 6_000n, (usdc1000 * 6n) / 10n);
    const resolverB = filler(KEYS.resolverB, "0.30", 10_000n, usdc1000);

    const intent = await maker.open({
      srcToken: usdc,
      srcAmount: 1_000n * 10n ** 18n,
      dstToken: NATIVE,
      startDstAmount: parseEther("0.5"),
      endDstAmount: parseEther("0.25"),
      auctionDuration: 600n,
      ttl: 3600n,
      srcLock: 6n * 3600n,
      parts: 4,
    });

    const pubB = createPublicClient({ transport: http(B.rpc) });
    const ethBefore = await pubB.getBalance({ address: makerAcct.address });

    // walk the auction forward; each resolver fills once the price reaches its bid
    for (let step = 0; step < 12; step++) {
      await resolverA.tick();
      await resolverB.tick();
      await maker.tick(intent);
      await resolverA.tick();
      await resolverB.tick();
      if ([...resolverA.jobs, ...resolverB.jobs].filter((j) => j.status === "settled").length === 2) break;
      await advance(A.rpc, 60);
    }

    expect(resolverA.jobs).toHaveLength(1);
    expect(resolverB.jobs).toHaveLength(1);
    expect(resolverA.jobs[0]!.index).toBe(2); // 60% -> bucket 2
    expect(resolverB.jobs[0]!.index).toBe(4); // final fill -> last secret
    expect([...resolverA.jobs, ...resolverB.jobs].every((j) => j.status === "settled")).toBe(true);

    // maker received ETH on B at auction prices between the floor and the ceiling
    const gained = (await pubB.getBalance({ address: makerAcct.address })) - ethBefore;
    expect(gained).toBeGreaterThan(parseEther("0.25"));
    expect(gained).toBeLessThan(parseEther("0.5"));

    // resolvers hold the USDC
    const pubA = createPublicClient({ transport: http(A.rpc) });
    const bal = (a: Address) =>
      pubA.readContract({ address: usdc, abi: erc20Abi, functionName: "balanceOf", args: [a] });
    expect(await bal(privateKeyToAccount(KEYS.deployer).address)).toBe(600n * 10n ** 18n);
    expect(await bal(privateKeyToAccount(KEYS.resolverB).address)).toBe(400n * 10n ** 18n);

    // gasless: maker never sent a transaction on B, and only the approval on A
    expect(await pubB.getTransactionCount({ address: makerAcct.address })).toBe(0);
    expect(await pubA.getTransactionCount({ address: makerAcct.address })).toBe(1);
  }, 120_000);
});
