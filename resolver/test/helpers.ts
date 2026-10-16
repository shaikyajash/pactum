import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { createPublicClient, createWalletClient, http, type Address, type Hex } from "viem";
import { privateKeyToAccount } from "viem/accounts";

// anvil's default dev keys
export const KEYS = {
  deployer: "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
  user: "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
  resolverB: "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a",
  maker: "0x7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6",
} as const satisfies Record<string, Hex>;

export const hasAnvil = spawnSync("anvil", ["--version"]).status === 0;
export const root = resolve(__dirname, "../..");

export function buildContracts() {
  spawnSync("forge", ["build"], { cwd: root });
}

export async function startAnvil(port: number, chainId: number): Promise<ChildProcess> {
  const proc = spawn("anvil", ["--port", String(port), "--chain-id", String(chainId)]);
  await new Promise<void>((ok, fail) => {
    proc.stdout!.on("data", (d) => String(d).includes("Listening") && ok());
    proc.on("exit", () => fail(new Error(`anvil ${port} exited`)));
  });
  return proc;
}

export async function deploy(rpc: string, key: Hex, file: string, name: string, args: unknown[] = []) {
  const { abi, bytecode } = JSON.parse(
    readFileSync(resolve(root, `out/${file}/${name}.json`), "utf8"),
  ) as { abi: unknown[]; bytecode: { object: Hex } };
  const account = privateKeyToAccount(key);
  const wallet = createWalletClient({ account, transport: http(rpc), chain: null });
  const pub = createPublicClient({ transport: http(rpc) });
  const hash = await wallet.deployContract({ abi, bytecode: bytecode.object, args, chain: null });
  return (await pub.waitForTransactionReceipt({ hash })).contractAddress as Address;
}

export const mintAbi = [
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
] as const;

/** Fast-forward an anvil chain. */
export async function advance(rpc: string, seconds: number) {
  const pub = createPublicClient({ transport: http(rpc) });
  await pub.request({ method: "evm_increaseTime" as never, params: [seconds] as never });
  await pub.request({ method: "evm_mine" as never, params: [] as never });
}
