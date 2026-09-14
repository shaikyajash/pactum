import { createServer, type Server } from "node:http";
import { recoverTypedDataAddress, type Address, type Hex } from "viem";
import { domain, hashOrder, ORDER_TYPES, type Order } from "./order.js";
import { SecretTree } from "./merkle.js";
import { matchesHash } from "./secret.js";

/**
 * Off-chain order relay. Makers post signed orders here; resolvers poll for
 * open orders and for secrets the maker releases after checking a fill.
 *
 * The relay is untrusted: it only stores data whose validity anyone can check
 * (signatures, Merkle proofs), and it never sees a secret before the maker
 * decides to release it.
 */
export interface SignedOrder {
  order: Order;
  signature: Hex;
  srcChainId: number;
  contract: Address;
}

interface Entry extends SignedOrder {
  hash: Hex;
  secrets: Map<number, Hex>;
}

// bigint-safe JSON
const enc = (v: unknown) => JSON.stringify(v, (_, x) => (typeof x === "bigint" ? `${x}n` : x));
const dec = <T>(s: string): T =>
  JSON.parse(s, (_, x) => (typeof x === "string" && /^\d+n$/.test(x) ? BigInt(x.slice(0, -1)) : x));

export class RelayServer {
  private readonly orders = new Map<Hex, Entry>();
  private server?: Server;

  async add(o: SignedOrder): Promise<Hex> {
    const signer = await recoverTypedDataAddress({
      domain: domain(o.srcChainId, o.contract),
      types: ORDER_TYPES,
      primaryType: "Order",
      message: o.order,
      signature: o.signature,
    });
    // EIP-1271 makers are verified on-chain at fill time instead
    if (signer.toLowerCase() !== o.order.maker.toLowerCase()) throw new Error("bad signature");
    const hash = hashOrder(o.order, o.srcChainId, o.contract);
    this.orders.set(hash, { ...o, hash, secrets: new Map() });
    return hash;
  }

  publishSecret(hash: Hex, index: number, secret: Hex, secretHash: Hex, proof: Hex[]): void {
    const e = this.orders.get(hash);
    if (!e) throw new Error("unknown order");
    if (!matchesHash(secret, secretHash) || !SecretTree.verify(e.order.secretsRoot, index, secretHash, proof)) {
      throw new Error("secret not committed by this order");
    }
    e.secrets.set(index, secret);
  }

  listen(port: number): Promise<void> {
    this.server = createServer(async (req, res) => {
      const send = (code: number, body: unknown) => {
        res.writeHead(code, { "content-type": "application/json" });
        res.end(enc(body));
      };
      let body = "";
      for await (const chunk of req) body += chunk;
      try {
        const url = new URL(req.url ?? "/", "http://relay");
        const parts = url.pathname.split("/").filter(Boolean);

        if (req.method === "POST" && url.pathname === "/orders") {
          return send(201, { hash: await this.add(dec<SignedOrder>(body)) });
        }
        if (req.method === "GET" && url.pathname === "/orders") {
          const open = [...this.orders.values()].map(({ secrets, ...o }) => o);
          return send(200, open);
        }
        if (parts[0] === "orders" && parts[2] === "secrets") {
          const hash = parts[1] as Hex;
          if (req.method === "POST") {
            const b = dec<{ index: number; secret: Hex; secretHash: Hex; proof: Hex[] }>(body);
            this.publishSecret(hash, b.index, b.secret, b.secretHash, b.proof);
            return send(201, { ok: true });
          }
          const secret = this.orders.get(hash)?.secrets.get(Number(parts[3]));
          return secret ? send(200, { secret }) : send(404, { error: "not released" });
        }
        send(404, { error: "not found" });
      } catch (e) {
        send(400, { error: String(e) });
      }
    });
    return new Promise((ok) => this.server!.listen(port, ok));
  }

  close(): Promise<void> {
    return new Promise((ok) => (this.server ? this.server.close(() => ok()) : ok()));
  }
}

/** HTTP client for the relay. */
export class RelayClient {
  constructor(private readonly base: string) {}

  private async req<T>(method: string, path: string, body?: unknown): Promise<{ status: number; data: T }> {
    const r = await fetch(this.base + path, { method, body: body === undefined ? undefined : enc(body) });
    return { status: r.status, data: dec<T>(await r.text()) };
  }

  async post(o: SignedOrder): Promise<Hex> {
    const r = await this.req<{ hash: Hex; error?: string }>("POST", "/orders", o);
    if (r.status !== 201) throw new Error(r.data.error);
    return r.data.hash;
  }

  async orders(): Promise<(SignedOrder & { hash: Hex })[]> {
    return (await this.req<(SignedOrder & { hash: Hex })[]>("GET", "/orders")).data;
  }

  async release(hash: Hex, index: number, secret: Hex, secretHash: Hex, proof: Hex[]) {
    const r = await this.req<{ error?: string }>("POST", `/orders/${hash}/secrets`, {
      index,
      secret,
      secretHash,
      proof,
    });
    if (r.status !== 201) throw new Error(r.data.error);
  }

  async secret(hash: Hex, index: number): Promise<Hex | undefined> {
    const r = await this.req<{ secret?: Hex }>("GET", `/orders/${hash}/secrets/${index}`);
    return r.status === 200 ? r.data.secret : undefined;
  }
}
