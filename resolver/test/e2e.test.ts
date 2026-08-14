import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import {
  createPublicClient,
  createWalletClient,
  erc20Abi,
  http,
  parseEther,
  type Address,
  type Hex,
} from "viem";
import { privateKeyToAccount } from "viem/accounts";
import { Chain } from "../src/chain.js";
import { Resolver } from "../src/resolver.js";
import { swapAcross } from "../src/client.js";
import { NATIVE } from "../src/swap.js";

// anvil's default dev keys
const RESOLVER_KEY: Hex = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const USER_KEY: Hex = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

const hasAnvil = spawnSync("anvil", ["--version"]).status === 0;
const root = resolve(__dirname, "../..");
const artifact = (file: string, name: string) =>
  JSON.parse(readFileSync(resolve(root, `out/${file}/${name}.json`), "utf8")) as {
    abi: unknown[];
    bytecode: { object: Hex };
  };

async function startAnvil(port: number, chainId: number): Promise<ChildProcess> {
  const proc = spawn("anvil", ["--port", String(port), "--chain-id", String(chainId)]);
  await new Promise<void>((ok, fail) => {
    proc.stdout!.on("data", (d) => String(d).includes("Listening") && ok());
    proc.on("exit", () => fail(new Error(`anvil ${port} exited`)));
  });
  return proc;
}

async function deploy(rpc: string, key: Hex, file: string, name: string, args: unknown[] = []) {
  const account = privateKeyToAccount(key);
  const wallet = createWalletClient({ account, transport: http(rpc), chain: null });
  const pub = createPublicClient({ transport: http(rpc) });
  const { abi, bytecode } = artifact(file, name);
  const hash = await wallet.deployContract({ abi, bytecode: bytecode.object, args, chain: null });
  const r = await pub.waitForTransactionReceipt({ hash });
  return r.contractAddress as Address;
}

describe.skipIf(!hasAnvil)("e2e: ETH on chain A -> USDC on chain B", () => {
  const A = { id: 31337, name: "alpha", rpc: "http://127.0.0.1:18545" };
  const B = { id: 31338, name: "beta", rpc: "http://127.0.0.1:18546" };
  const procs: ChildProcess[] = [];
  let usdc: Address;
  let cfgA: typeof A & { contract: Address };
  let cfgB: typeof B & { contract: Address };

  beforeAll(async () => {
    spawnSync("forge", ["build"], { cwd: root });
    procs.push(await startAnvil(18545, A.id), await startAnvil(18546, B.id));

    cfgA = { ...A, contract: await deploy(A.rpc, RESOLVER_KEY, "HashLock.sol", "HashLock") };
    cfgB = { ...B, contract: await deploy(B.rpc, RESOLVER_KEY, "HashLock.sol", "HashLock") };
    usdc = await deploy(B.rpc, RESOLVER_KEY, "MockERC20.sol", "MockERC20", ["USD Coin", "USDC"]);

    // fund the resolver's inventory on B
    const wallet = createWalletClient({
      account: privateKeyToAccount(RESOLVER_KEY),
      transport: http(B.rpc),
      chain: null,
    });
    const pub = createPublicClient({ transport: http(B.rpc) });
    const hash = await wallet.writeContract({
      address: usdc,
      abi: [
        {
          type: "function",
          name: "mint",
          stateMutability: "nonpayable",
          inputs: [
            { name: "to", type: "address" },
            { name: "amount", type: "uint256" },
          ],
          outputs: [],
        },
      ],
      functionName: "mint",
      args: [wallet.account.address, 1_000_000n * 10n ** 18n],
      chain: null,
    });
    await pub.waitForTransactionReceipt({ hash });
  }, 60_000);

  afterAll(() => procs.forEach((p) => p.kill()));

  it("completes the swap and the resolver claims the source leg", async () => {
    const resolverAcct = privateKeyToAccount(RESOLVER_KEY);
    const userAcct = privateKeyToAccount(USER_KEY);
    const resolver = new Resolver(
      [new Chain(cfgA, resolverAcct), new Chain(cfgB, resolverAcct)],
      {
        routes: [
          {
            srcChainId: A.id,
            srcToken: NATIVE,
            dstChainId: B.id,
            dstToken: usdc,
            rateNum: 3000n,
            rateDen: 1n,
            minSrc: parseEther("0.01"),
            maxSrc: parseEther("10"),
          },
        ],
      },
    );
    await resolver.init();

    let running = true;
    const loop = (async () => {
      while (running) {
        await resolver.tick();
        await new Promise((r) => setTimeout(r, 200));
      }
    })();

    const chainA = new Chain(cfgA, userAcct);
    const chainB = new Chain(cfgB, userAcct);
    const resolverEthBefore = await chainA.public.getBalance({ address: resolverAcct.address });

    const result = await swapAcross(chainA, chainB, {
      resolver: resolverAcct.address,
      srcToken: NATIVE,
      srcAmount: parseEther("1"),
      dstToken: usdc,
      minDstAmount: 2_990n * 10n ** 18n,
      lockDuration: 24n * 3600n,
      pollMs: 200,
      timeoutMs: 30_000,
    });

    expect(result.dstAmount).toBe(3_000n * 10n ** 18n);
    const userUsdc = await chainB.public.readContract({
      address: usdc,
      abi: erc20Abi,
      functionName: "balanceOf",
      args: [userAcct.address],
    });
    expect(userUsdc).toBe(3_000n * 10n ** 18n);

    // wait for the resolver to see the secret and claim
    const deadline = Date.now() + 20_000;
    while (resolver.jobs.get(result.srcSwapId)?.status !== "claimed" && Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 200));
    }
    running = false;
    await loop;

    expect(resolver.jobs.get(result.srcSwapId)?.status).toBe("claimed");
    const resolverEthAfter = await chainA.public.getBalance({ address: resolverAcct.address });
    // +1 ETH minus gas for the redeem tx
    expect(resolverEthAfter - resolverEthBefore).toBeGreaterThan(parseEther("0.99"));
    expect((await chainA.swap(result.srcSwapId)).settled).toBe(true);
  }, 60_000);
});
