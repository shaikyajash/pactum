use std::{net::SocketAddr, path::PathBuf, time::Duration};

use alloy::{
    primitives::{Address, U256},
    signers::local::PrivateKeySigner,
};
use anyhow::Context;
use clap::{Parser, Subcommand};
use pactum_node::{Chain, IntentFiller, IntentRequest, Maker, config::Config, filler::FixedRate};
use pactum_relay::{Relay, RelayClient};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Gasless, partially fillable cross-chain swaps settled by HTLCs.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run an order relay.
    Relay {
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: SocketAddr,
    },
    /// Run a resolver that fills intents at a fixed rate once the auction reaches it.
    Resolve {
        #[arg(short, long, default_value = "config.json")]
        config: PathBuf,
        #[arg(long, env = "PACTUM_RELAY", default_value = "http://127.0.0.1:8080")]
        relay: String,
        #[arg(long, env = "RESOLVER_KEY", hide_env_values = true)]
        key: PrivateKeySigner,
        /// Destination base units paid per `rate-den` source base units.
        #[arg(long)]
        rate_num: U256,
        #[arg(long)]
        rate_den: U256,
        /// Largest slice per fill, in basis points of the order.
        #[arg(long, default_value_t = 10_000)]
        max_fill_bps: u64,
        /// Total source base units to buy before stopping.
        #[arg(long)]
        budget: Option<U256>,
        #[arg(long, default_value_t = 2_000)]
        interval_ms: u64,
    },
    /// Sign and post an intent, then release secrets as fills are secured.
    Intent {
        #[arg(short, long, default_value = "config.json")]
        config: PathBuf,
        #[arg(long, env = "PACTUM_RELAY", default_value = "http://127.0.0.1:8080")]
        relay: String,
        #[arg(long, env = "MAKER_KEY", hide_env_values = true)]
        key: PrivateKeySigner,
        /// Source chain name or id.
        #[arg(long)]
        from: String,
        /// Destination chain name or id.
        #[arg(long)]
        to: String,
        #[arg(long)]
        sell_token: Address,
        /// Source amount, in base units.
        #[arg(long)]
        sell_amount: U256,
        /// Destination token; omit for the native asset.
        #[arg(long, default_value_t = Address::ZERO)]
        buy_token: Address,
        /// Opening ask for the whole order, in destination base units.
        #[arg(long)]
        start: U256,
        /// Floor the auction decays to.
        #[arg(long)]
        floor: U256,
        #[arg(long, default_value_t = 600)]
        auction_secs: u64,
        #[arg(long, default_value_t = 3_600)]
        ttl_secs: u64,
        #[arg(long, default_value_t = 6 * 3_600)]
        src_lock_secs: u64,
        /// Number of parts the order may be split into.
        #[arg(long, default_value_t = 1)]
        parts: u16,
        #[arg(long)]
        receiver: Option<Address>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    match Cli::parse().cmd {
        Cmd::Relay { listen } => pactum_relay::serve(listen, Relay::default()).await,

        Cmd::Resolve {
            config,
            relay,
            key,
            rate_num,
            rate_den,
            max_fill_bps,
            budget,
            interval_ms,
        } => {
            let cfg = Config::load(&config)?;
            let chains = cfg
                .chains
                .iter()
                .map(|c| Chain::connect(c, key.clone()))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let mut filler = IntentFiller::new(
                chains,
                RelayClient::new(relay),
                FixedRate { rate_num, rate_den, max_fill_bps },
            );
            if let Some(b) = budget {
                filler = filler.with_budget(b);
            }
            info!(resolver = %key.address(), "resolving");
            let mut tick = tokio::time::interval(Duration::from_millis(interval_ms));
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        if let Err(e) = filler.tick().await {
                            tracing::warn!("tick failed: {e:#}");
                        }
                    }
                    _ = tokio::signal::ctrl_c() => break Ok(()),
                }
            }
        }

        Cmd::Intent {
            config,
            relay,
            key,
            from,
            to,
            sell_token,
            sell_amount,
            buy_token,
            start,
            floor,
            auction_secs,
            ttl_secs,
            src_lock_secs,
            parts,
            receiver,
        } => {
            let cfg = Config::load(&config)?;
            let src = Chain::connect(cfg.chain(&from)?, key.clone())?;
            let dst = Chain::connect(cfg.chain(&to)?, key.clone())?;
            let deadline_chain = src.clone();
            let maker = Maker::new(src, dst, key, RelayClient::new(relay));

            let nonce = U256::from(
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos(),
            );
            let mut intent = maker
                .open(
                    IntentRequest {
                        src_token: sell_token,
                        src_amount: sell_amount,
                        dst_token: buy_token,
                        start_dst_amount: start,
                        end_dst_amount: floor,
                        auction_duration: auction_secs,
                        ttl: ttl_secs,
                        src_lock: src_lock_secs,
                        parts,
                        receiver,
                    },
                    nonce,
                )
                .await
                .context("opening intent")?;
            println!("{}", intent.hash);

            let all = pactum_core::OrderSecrets::count_for(parts);
            while intent.released().len() < all {
                if deadline_chain.now().await? > intent.order.deadline + intent.order.srcLock {
                    anyhow::bail!(
                        "intent expired with {} of {all} parts filled",
                        intent.released().len()
                    );
                }
                maker.tick(&mut intent).await?;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            info!("all parts filled");
            Ok(())
        }
    }
}
