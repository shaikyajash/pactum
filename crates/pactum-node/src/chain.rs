use alloy::{
    eips::BlockNumberOrTag,
    network::EthereumWallet,
    primitives::{Address, B256, Bytes, U256},
    providers::{DynProvider, Provider, ProviderBuilder},
    rpc::types::TransactionReceipt,
    signers::local::PrivateKeySigner,
};
use anyhow::{Context, bail};
use pactum_core::Order;
use serde::Deserialize;

use crate::contracts::{IERC20, PactumIntents, to_binding};

/// Native asset sentinel, as in `HashLock.NATIVE`.
pub const NATIVE: Address = Address::ZERO;

#[derive(Clone, Debug, Deserialize)]
pub struct ChainConfig {
    pub id: u64,
    pub name: String,
    pub rpc: String,
    pub contract: Address,
}

#[derive(Clone, Debug)]
pub struct Escrow {
    pub initiator: Address,
    pub expiry: u64,
    pub settled: bool,
    pub redeemer: Address,
    pub token: Address,
    pub amount: U256,
    pub secret_hash: B256,
}

#[derive(Clone, Debug)]
pub struct Lock {
    pub swap_id: B256,
    pub initiator: Address,
    pub redeemer: Address,
    pub token: Address,
    pub amount: U256,
    pub secret_hash: B256,
    pub expiry: u64,
}

#[derive(Clone, Debug)]
pub struct Filled {
    pub swap_id: B256,
    pub resolver: Address,
    pub fill_amount: U256,
    pub secret_index: usize,
    pub owed: U256,
}

/// One PactumIntents deployment plus a signing provider.
#[derive(Clone)]
pub struct Chain {
    pub id: u64,
    pub name: String,
    pub contract: Address,
    pub account: Address,
    provider: DynProvider,
}

impl Chain {
    pub fn connect(cfg: &ChainConfig, signer: PrivateKeySigner) -> anyhow::Result<Self> {
        let account = signer.address();
        let provider = ProviderBuilder::new()
            .wallet(EthereumWallet::from(signer))
            .connect_http(cfg.rpc.parse().context("rpc url")?)
            .erased();
        Ok(Self { id: cfg.id, name: cfg.name.clone(), contract: cfg.contract, account, provider })
    }

    fn htlc(&self) -> PactumIntents::PactumIntentsInstance<DynProvider> {
        PactumIntents::new(self.contract, self.provider.clone())
    }

    pub fn provider(&self) -> &DynProvider {
        &self.provider
    }

    pub async fn now(&self) -> anyhow::Result<u64> {
        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await?
            .context("no latest block")?;
        Ok(block.header.timestamp)
    }

    pub async fn head(&self) -> anyhow::Result<u64> {
        Ok(self.provider.get_block_number().await?)
    }

    pub async fn escrow(&self, swap_id: B256) -> anyhow::Result<Escrow> {
        let s = self.htlc().swaps(swap_id).call().await?;
        Ok(Escrow {
            initiator: s.initiator,
            expiry: s.expiry,
            settled: s.settled,
            redeemer: s.redeemer,
            token: s.token,
            amount: s.amount,
            secret_hash: s.secretHash,
        })
    }

    pub async fn deposit(&self, swap_id: B256) -> anyhow::Result<U256> {
        Ok(self.htlc().deposits(swap_id).call().await?)
    }

    pub async fn progress(&self, order_hash: B256) -> anyhow::Result<(U256, usize)> {
        let f = self.htlc().fills(order_hash).call().await?;
        Ok((f.filled, f.nextIndex as usize))
    }

    async fn confirm(
        &self,
        what: &str,
        receipt: TransactionReceipt,
    ) -> anyhow::Result<TransactionReceipt> {
        if !receipt.status() {
            bail!("{}: {what} reverted in {}", self.name, receipt.transaction_hash);
        }
        Ok(receipt)
    }

    pub async fn initiate(
        &self,
        redeemer: Address,
        token: Address,
        amount: U256,
        secret_hash: B256,
        expiry: u64,
        salt: B256,
    ) -> anyhow::Result<B256> {
        let value = if token == NATIVE { amount } else { U256::ZERO };
        let receipt = self
            .htlc()
            .initiate(redeemer, token, amount, secret_hash, expiry, salt)
            .value(value)
            .send()
            .await?
            .get_receipt()
            .await?;
        let receipt = self.confirm("initiate", receipt).await?;
        let log = receipt
            .inner
            .logs()
            .iter()
            .find_map(|l| l.log_decode::<PactumIntents::Initiated>().ok())
            .context("no Initiated log")?;
        Ok(log.inner.data.swapId)
    }

    pub async fn redeem(&self, swap_id: B256, secret: Bytes) -> anyhow::Result<()> {
        let r = self.htlc().redeem(swap_id, secret).send().await?.get_receipt().await?;
        self.confirm("redeem", r).await.map(drop)
    }

    pub async fn refund(&self, swap_id: B256) -> anyhow::Result<()> {
        let r = self.htlc().refund(swap_id).send().await?.get_receipt().await?;
        self.confirm("refund", r).await.map(drop)
    }

    pub async fn ensure_allowance(&self, token: Address, amount: U256) -> anyhow::Result<()> {
        if token == NATIVE {
            return Ok(());
        }
        let erc20 = IERC20::new(token, self.provider.clone());
        if erc20.allowance(self.account, self.contract).call().await? >= amount {
            return Ok(());
        }
        let r = erc20.approve(self.contract, U256::MAX).send().await?.get_receipt().await?;
        self.confirm("approve", r).await.map(drop)
    }

    pub async fn fill(
        &self,
        order: &Order,
        signature: Bytes,
        amount: U256,
        index: usize,
        secret_hash: B256,
        proof: Vec<B256>,
    ) -> anyhow::Result<Filled> {
        let receipt = self
            .htlc()
            .fill(to_binding(order), signature, amount, U256::from(index), secret_hash, proof)
            .value(order.minDeposit)
            .send()
            .await?
            .get_receipt()
            .await?;
        let receipt = self.confirm("fill", receipt).await?;
        receipt
            .inner
            .logs()
            .iter()
            .find_map(|l| l.log_decode::<PactumIntents::OrderFilled>().ok())
            .map(|l| filled(&l.inner.data))
            .context("no OrderFilled log")
    }

    /// Locks on this chain paying `redeemer`, in `[from, to]`.
    pub async fn locks_to(
        &self,
        redeemer: Address,
        from: u64,
        to: u64,
    ) -> anyhow::Result<Vec<Lock>> {
        let logs = self
            .htlc()
            .Initiated_filter()
            .topic3(redeemer.into_word())
            .from_block(from)
            .to_block(to)
            .query()
            .await?;
        Ok(logs
            .into_iter()
            .map(|(e, _)| Lock {
                swap_id: e.swapId,
                initiator: e.initiator,
                redeemer: e.redeemer,
                token: e.token,
                amount: e.amount,
                secret_hash: e.secretHash,
                expiry: e.expiry,
            })
            .collect())
    }

    pub async fn fills_of(
        &self,
        order_hash: B256,
        from: u64,
        to: u64,
    ) -> anyhow::Result<Vec<Filled>> {
        let logs = self
            .htlc()
            .OrderFilled_filter()
            .topic1(order_hash)
            .from_block(from)
            .to_block(to)
            .query()
            .await?;
        Ok(logs.iter().map(|(e, _)| filled(e)).collect())
    }
}

fn filled(e: &PactumIntents::OrderFilled) -> Filled {
    Filled {
        swap_id: e.swapId,
        resolver: e.resolver,
        fill_amount: e.fillAmount,
        secret_index: e.secretIndex.to::<usize>(),
        owed: e.owedDstAmount,
    }
}
