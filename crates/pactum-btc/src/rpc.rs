//! Minimal async Bitcoin Core JSON-RPC client: just what the HTLC leg needs.

use anyhow::{Context, bail};
use bitcoin::{
    Address, Amount, BlockHash, OutPoint, Transaction, Txid,
    consensus::encode::{deserialize_hex, serialize_hex},
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

#[derive(Clone)]
pub struct BitcoinRpc {
    url: String,
    user: String,
    pass: String,
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct Response<T> {
    result: Option<T>,
    error: Option<Value>,
}

/// An unspent output found by `scantxoutset`.
#[derive(Clone, Debug)]
pub struct Utxo {
    pub outpoint: OutPoint,
    pub value: Amount,
    pub height: u64,
}

impl BitcoinRpc {
    pub fn new(url: impl Into<String>, user: impl Into<String>, pass: impl Into<String>) -> Self {
        Self { url: url.into(), user: user.into(), pass: pass.into(), http: reqwest::Client::new() }
    }

    /// Same client scoped to a loaded wallet (`/wallet/<name>`).
    pub fn wallet(&self, name: &str) -> Self {
        Self { url: format!("{}/wallet/{name}", self.url.trim_end_matches('/')), ..self.clone() }
    }

    pub async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> anyhow::Result<T> {
        let body = json!({ "jsonrpc": "1.0", "id": "pactum", "method": method, "params": params });
        let resp: Response<T> = self
            .http
            .post(&self.url)
            .basic_auth(&self.user, Some(&self.pass))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("rpc {method}"))?
            .json()
            .await
            .with_context(|| format!("decoding rpc {method}"))?;
        if let Some(err) = resp.error.filter(|e| !e.is_null()) {
            bail!("rpc {method}: {err}");
        }
        resp.result.with_context(|| format!("rpc {method}: empty result"))
    }

    pub async fn block_count(&self) -> anyhow::Result<u64> {
        self.call("getblockcount", json!([])).await
    }

    pub async fn block_hash(&self, height: u64) -> anyhow::Result<BlockHash> {
        self.call("getblockhash", json!([height])).await
    }

    /// Transactions of a block, as consensus-decoded transactions.
    pub async fn block_txs(&self, hash: BlockHash) -> anyhow::Result<Vec<Transaction>> {
        let block: Value = self.call("getblock", json!([hash, 2])).await?;
        block["tx"]
            .as_array()
            .context("getblock: no tx array")?
            .iter()
            .map(|t| deserialize_hex(t["hex"].as_str().context("tx without hex")?).map_err(Into::into))
            .collect()
    }

    pub async fn send_raw(&self, tx: &Transaction) -> anyhow::Result<Txid> {
        self.call("sendrawtransaction", json!([serialize_hex(tx)])).await
    }

    pub async fn raw_tx(&self, txid: Txid) -> anyhow::Result<Transaction> {
        let hex: String = self.call("getrawtransaction", json!([txid])).await?;
        Ok(deserialize_hex(&hex)?)
    }

    /// Unspent outputs paying `address`, without importing it into a wallet.
    pub async fn utxos(&self, address: &Address) -> anyhow::Result<Vec<Utxo>> {
        let scan: Value =
            self.call("scantxoutset", json!(["start", [format!("addr({address})")]])).await?;
        scan["unspents"]
            .as_array()
            .context("scantxoutset: no unspents")?
            .iter()
            .map(|u| {
                Ok(Utxo {
                    outpoint: OutPoint {
                        txid: u["txid"].as_str().context("txid")?.parse()?,
                        vout: u["vout"].as_u64().context("vout")? as u32,
                    },
                    value: Amount::from_btc(u["amount"].as_f64().context("amount")?)?,
                    height: u["height"].as_u64().context("height")?,
                })
            })
            .collect()
    }
}
