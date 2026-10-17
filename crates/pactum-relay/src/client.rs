use alloy::primitives::{B256, Bytes};
use anyhow::{Context, bail};
use serde::Deserialize;

use crate::{LockAnnouncement, OpenOrder, SecretRelease, SignedOrder};

/// HTTP client for a relay.
#[derive(Clone)]
pub struct RelayClient {
    base: String,
    http: reqwest::Client,
}

#[derive(Deserialize)]
struct Hash {
    hash: B256,
}

#[derive(Deserialize)]
struct SecretBody {
    secret: Bytes,
}

impl RelayClient {
    pub fn new(base: impl Into<String>) -> Self {
        Self { base: base.into().trim_end_matches('/').to_owned(), http: reqwest::Client::new() }
    }

    pub async fn post(&self, order: &SignedOrder) -> anyhow::Result<B256> {
        let r = self.http.post(format!("{}/orders", self.base)).json(order).send().await?;
        if !r.status().is_success() {
            bail!("relay rejected order: {}", r.text().await?);
        }
        Ok(r.json::<Hash>().await?.hash)
    }

    pub async fn orders(&self) -> anyhow::Result<Vec<OpenOrder>> {
        let r = self.http.get(format!("{}/orders", self.base)).send().await?;
        r.error_for_status()?.json().await.context("decoding orders")
    }

    pub async fn release(&self, hash: B256, release: &SecretRelease) -> anyhow::Result<()> {
        let url = format!("{}/orders/{hash}/secrets", self.base);
        let r = self.http.post(url).json(release).send().await?;
        if !r.status().is_success() {
            bail!("relay rejected secret: {}", r.text().await?);
        }
        Ok(())
    }

    pub async fn announce(&self, hash: B256, lock: &LockAnnouncement) -> anyhow::Result<()> {
        let url = format!("{}/orders/{hash}/locks", self.base);
        let r = self.http.post(url).json(lock).send().await?;
        if !r.status().is_success() {
            bail!("relay rejected lock: {}", r.text().await?);
        }
        Ok(())
    }

    pub async fn locks(&self, hash: B256) -> anyhow::Result<Vec<LockAnnouncement>> {
        let r = self.http.get(format!("{}/orders/{hash}/locks", self.base)).send().await?;
        r.error_for_status()?.json().await.context("decoding locks")
    }

    pub async fn secret(&self, hash: B256, index: usize) -> anyhow::Result<Option<Bytes>> {
        let r =
            self.http.get(format!("{}/orders/{hash}/secrets/{index}", self.base)).send().await?;
        if r.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(r.error_for_status()?.json::<SecretBody>().await?.secret))
    }
}
