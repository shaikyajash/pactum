//! Minimal Solana JSON-RPC client.

use std::time::Duration;

use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use serde_json::{Value, json};

use crate::tx::{Instruction, Keypair, Pubkey, build_transaction};

#[derive(Clone)]
pub struct SolanaRpc {
    url: String,
    http: reqwest::Client,
}

impl SolanaRpc {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), http: reqwest::Client::new() }
    }

    pub async fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp: Value = self.http.post(&self.url).json(&body).send().await?.json().await?;
        if let Some(err) = resp.get("error") {
            bail!("solana rpc {method}: {err}");
        }
        resp.get("result").cloned().with_context(|| format!("{method}: no result"))
    }

    pub async fn latest_blockhash(&self) -> anyhow::Result<[u8; 32]> {
        let r = self.call("getLatestBlockhash", json!([{ "commitment": "confirmed" }])).await?;
        let s = r["value"]["blockhash"].as_str().context("blockhash")?;
        Ok(s.parse::<Pubkey>()?.0)
    }

    pub async fn balance(&self, key: &Pubkey) -> anyhow::Result<u64> {
        let r = self
            .call("getBalance", json!([key.to_string(), { "commitment": "confirmed" }]))
            .await?;
        r["value"].as_u64().context("balance")
    }

    /// Account data and owner, or `None` if the account doesn't exist.
    pub async fn account(&self, key: &Pubkey) -> anyhow::Result<Option<(Vec<u8>, Pubkey, u64)>> {
        let r = self
            .call(
                "getAccountInfo",
                json!([key.to_string(), { "encoding": "base64", "commitment": "confirmed" }]),
            )
            .await?;
        let v = &r["value"];
        if v.is_null() {
            return Ok(None);
        }
        let data = B64.decode(v["data"][0].as_str().context("data")?)?;
        let owner = v["owner"].as_str().context("owner")?.parse()?;
        Ok(Some((data, owner, v["lamports"].as_u64().context("lamports")?)))
    }

    /// Unix time of the latest confirmed block: the clock the program's
    /// `Clock::unix_timestamp` reads.
    pub async fn chain_time(&self) -> anyhow::Result<i64> {
        let slot = self.call("getSlot", json!([{ "commitment": "confirmed" }])).await?;
        let t = self.call("getBlockTime", json!([slot])).await?;
        t.as_i64().context("block time")
    }

    pub async fn airdrop(&self, to: &Pubkey, lamports: u64) -> anyhow::Result<()> {
        let sig = self.call("requestAirdrop", json!([to.to_string(), lamports])).await?;
        self.confirm(sig.as_str().context("signature")?).await
    }

    /// Sign, send and wait for confirmation.
    pub async fn send(
        &self,
        instructions: &[Instruction],
        payer: &Keypair,
        signers: &[&Keypair],
    ) -> anyhow::Result<String> {
        let blockhash = self.latest_blockhash().await?;
        let tx = build_transaction(instructions, payer, signers, blockhash)?;
        let sig = self
            .call(
                "sendTransaction",
                json!([B64.encode(tx), { "encoding": "base64", "preflightCommitment": "confirmed" }]),
            )
            .await?;
        let sig = sig.as_str().context("signature")?.to_owned();
        self.confirm(&sig).await?;
        Ok(sig)
    }

    async fn confirm(&self, sig: &str) -> anyhow::Result<()> {
        for _ in 0..120 {
            let r = self.call("getSignatureStatuses", json!([[sig]])).await?;
            let st = &r["value"][0];
            if !st.is_null() {
                if !st["err"].is_null() {
                    bail!("transaction {sig} failed: {}", st["err"]);
                }
                let level = st["confirmationStatus"].as_str().unwrap_or("");
                if level == "confirmed" || level == "finalized" {
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        bail!("transaction {sig} not confirmed in time")
    }
}
