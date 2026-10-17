//! Two anvil chains, a deployed PactumIntents on each, a permit-capable USDC
//! on chain A, and a relay on a random local port.

use std::{path::PathBuf, process::Command, sync::Once};

use alloy::{
    network::{EthereumWallet, TransactionBuilder},
    node_bindings::{Anvil, AnvilInstance},
    primitives::{Address, Bytes, U256, utils::parse_ether},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol,
};
use pactum_node::{Chain, ChainConfig, filler::FixedRate};
use pactum_relay::{Relay, RelayClient, router};

sol! {
    #[sol(rpc)]
    interface MockToken {
        function mint(address to, uint256 amount) external;
        function balanceOf(address owner) external view returns (uint256);
    }
}

pub struct Env {
    pub a: AnvilInstance,
    pub b: AnvilInstance,
    pub cfg_a: ChainConfig,
    pub cfg_b: ChainConfig,
    pub usdc: Address,
    pub relay: RelayClient,
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn tool_available(bin: &str) -> bool {
    Command::new(bin).arg("--version").output().is_ok_and(|o| o.status.success())
}

fn bytecode(file: &str, name: &str) -> Bytes {
    let path = repo_root().join(format!("out/{file}/{name}.json"));
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("run `forge build` first")).unwrap();
    json["bytecode"]["object"].as_str().unwrap().parse().unwrap()
}

pub fn signer(anvil: &AnvilInstance, i: usize) -> PrivateKeySigner {
    anvil.keys()[i].clone().into()
}

async fn deploy(anvil: &AnvilInstance, code: Bytes) -> Address {
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(signer(anvil, 0)))
        .connect_http(anvil.endpoint_url());
    let tx = TransactionRequest::default().with_deploy_code(code);
    let receipt = provider.send_transaction(tx).await.unwrap().get_receipt().await.unwrap();
    receipt.contract_address.unwrap()
}

pub async fn advance(anvil: &AnvilInstance, secs: u64) {
    let p = ProviderBuilder::new().connect_http(anvil.endpoint_url());
    let _: serde_json::Value = p.raw_request("evm_increaseTime".into(), (secs,)).await.unwrap();
    let _: serde_json::Value = p.raw_request("evm_mine".into(), ()).await.unwrap();
}

pub fn thousand() -> U256 {
    parse_ether("1000").unwrap()
}

pub fn rate(eth_per_1000: &str, bps: u64) -> FixedRate {
    FixedRate {
        rate_num: parse_ether(eth_per_1000).unwrap(),
        rate_den: thousand(),
        max_fill_bps: bps,
    }
}

static BUILD: Once = Once::new();

impl Env {
    /// `None` when anvil/forge are not installed, so the suite still passes elsewhere.
    pub async fn setup() -> Option<Self> {
        if !tool_available("anvil") || !tool_available("forge") {
            eprintln!("skipping: anvil/forge not installed");
            return None;
        }
        BUILD.call_once(|| {
            let out = Command::new("forge").arg("build").current_dir(repo_root()).output().unwrap();
            assert!(out.status.success(), "forge build failed");
        });

        let a = Anvil::new().chain_id(31337).spawn();
        let b = Anvil::new().chain_id(31338).spawn();
        let intents = bytecode("PactumIntents.sol", "PactumIntents");
        let cfg_a = ChainConfig {
            id: 31337,
            name: "alpha".into(),
            rpc: a.endpoint(),
            contract: deploy(&a, intents.clone()).await,
        };
        let cfg_b = ChainConfig {
            id: 31338,
            name: "beta".into(),
            rpc: b.endpoint(),
            contract: deploy(&b, intents).await,
        };
        let usdc = deploy(&a, {
            let mut code = bytecode("MockPermitERC20.sol", "MockPermitERC20").to_vec();
            code.extend(alloy::sol_types::SolValue::abi_encode_params(&(
                "USD Coin".to_string(),
                "USDC".to_string(),
            )));
            code.into()
        })
        .await;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let relay_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router(Relay::default())).await.unwrap() });

        let env = Self { a, b, cfg_a, cfg_b, usdc, relay: RelayClient::new(&relay_url) };
        env.token()
            .mint(env.maker().address(), thousand())
            .send()
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();
        Some(env)
    }

    // accounts: 0 = resolver A (and deployer), 1 = resolver B, 2 = maker, 3 = keeper
    pub fn resolver_a(&self) -> PrivateKeySigner {
        signer(&self.a, 0)
    }
    pub fn resolver_b(&self) -> PrivateKeySigner {
        signer(&self.a, 1)
    }
    pub fn maker(&self) -> PrivateKeySigner {
        signer(&self.a, 2)
    }
    pub fn keeper(&self) -> PrivateKeySigner {
        signer(&self.a, 3)
    }

    pub fn chains(&self, s: &PrivateKeySigner) -> Vec<Chain> {
        vec![
            Chain::connect(&self.cfg_a, s.clone()).unwrap(),
            Chain::connect(&self.cfg_b, s.clone()).unwrap(),
        ]
    }

    pub fn token(&self) -> MockToken::MockTokenInstance<impl Provider + Clone> {
        MockToken::new(
            self.usdc,
            ProviderBuilder::new()
                .wallet(EthereumWallet::from(self.resolver_a()))
                .connect_http(self.a.endpoint_url()),
        )
    }

    pub async fn usdc_of(&self, who: Address) -> U256 {
        self.token().balanceOf(who).call().await.unwrap()
    }

    pub async fn nonce(&self, anvil: &AnvilInstance, who: Address) -> u64 {
        ProviderBuilder::new()
            .connect_http(anvil.endpoint_url())
            .get_transaction_count(who)
            .await
            .unwrap()
    }

    pub async fn eth_of(&self, anvil: &AnvilInstance, who: Address) -> U256 {
        ProviderBuilder::new().connect_http(anvil.endpoint_url()).get_balance(who).await.unwrap()
    }
}
