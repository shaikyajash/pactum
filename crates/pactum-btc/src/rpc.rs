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

    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> anyhow::Result<T> {
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
            .map(|t| {
                deserialize_hex(t["hex"].as_str().context("tx without hex")?).map_err(Into::into)
            })
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

    /// Value and confirmations of an unspent output, or `None` if spent or unknown.
    pub async fn tx_out(&self, outpoint: OutPoint) -> anyhow::Result<Option<(Amount, u64)>> {
        let out: Value =
            self.call("gettxout", json!([outpoint.txid, outpoint.vout, true])).await.or_else(
                // Bitcoin Core returns JSON null for spent outputs
                |e| if e.to_string().contains("empty result") { Ok(Value::Null) } else { Err(e) },
            )?;
        if out.is_null() {
            return Ok(None);
        }
        let value = Amount::from_btc(out["value"].as_f64().context("value")?)?;
        Ok(Some((value, out["confirmations"].as_u64().unwrap_or(0))))
    }

    /// Median-time-past of the tip: the clock `OP_CHECKLOCKTIMEVERIFY` timestamps use.
    pub async fn median_time(&self) -> anyhow::Result<u64> {
        let info: Value = self.call("getblockchaininfo", json!([])).await?;
        info["mediantime"].as_u64().context("mediantime")
    }

    /// The transaction spending `outpoint`, searching blocks from `from_height`
    /// to the tip and then the mempool (so a reveal is seen before it confirms).
    pub async fn find_spend(
        &self,
        outpoint: OutPoint,
        from_height: u64,
    ) -> anyhow::Result<Option<Transaction>> {
        let spends = |tx: &Transaction| tx.input.iter().any(|i| i.previous_output == outpoint);
        let tip = self.block_count().await?;
        for h in from_height..=tip {
            let txs = self.block_txs(self.block_hash(h).await?).await?;
            if let Some(tx) = txs.into_iter().find(spends) {
                return Ok(Some(tx));
            }
        }
        let mempool: Vec<Txid> = self.call("getrawmempool", json!([])).await?;
        for txid in mempool {
            if let Ok(tx) = self.raw_tx(txid).await {
                if spends(&tx) {
                    return Ok(Some(tx));
                }
            }
        }
        Ok(None)
    }

    /// Send `amount` to `address` from this (wallet-scoped) client; returns the
    /// outpoint created for `address`.
    pub async fn send_to(&self, address: &Address, amount: Amount) -> anyhow::Result<OutPoint> {
        let txid: Txid =
            self.call("sendtoaddress", json!([address.to_string(), amount.to_btc()])).await?;
        let tx = self.raw_tx(txid).await?;
        let spk = address.script_pubkey();
        let vout = tx
            .output
            .iter()
            .position(|o| o.script_pubkey == spk)
            .context("funding output not found")?;
        Ok(OutPoint { txid, vout: vout as u32 })
    }
}
