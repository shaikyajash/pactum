//! Maker sells 1000 USDC on chain A for ETH on chain B with one signed intent.
//! Two resolvers compete in the Dutch auction and split the order. The maker
//! sends no transaction besides a one-time token approval.

use std::{path::PathBuf, process::Command};

use alloy::{
    network::{EthereumWallet, TransactionBuilder},
    node_bindings::{Anvil, AnvilInstance},
    primitives::{Address, Bytes, U256, utils::parse_ether},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol,
};
use pactum_core::timelock::TimelockPolicy;
use pactum_node::{
    Chain, ChainConfig, IntentFiller, IntentRequest, JobStatus, Maker, NATIVE, filler::FixedRate,
};
use pactum_relay::{Relay, RelayClient, router};

sol! {
    #[sol(rpc)]
    interface MockToken {
        function mint(address to, uint256 amount) external;
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address owner) external view returns (uint256);
    }
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn bytecode(file: &str, name: &str) -> Bytes {
    let path = repo_root().join(format!("out/{file}/{name}.json"));
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("run `forge build` first")).unwrap();
    json["bytecode"]["object"].as_str().unwrap().parse().unwrap()
}

fn signer(anvil: &AnvilInstance, i: usize) -> PrivateKeySigner {
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

async fn advance(anvil: &AnvilInstance, secs: u64) {
    let p = ProviderBuilder::new().connect_http(anvil.endpoint_url());
    let _: serde_json::Value = p.raw_request("evm_increaseTime".into(), (secs,)).await.unwrap();
    let _: serde_json::Value = p.raw_request("evm_mine".into(), ()).await.unwrap();
}

fn tool_available(bin: &str) -> bool {
    Command::new(bin).arg("--version").output().is_ok_and(|o| o.status.success())
}

#[tokio::test]
async fn gasless_split_intent_across_two_chains() {
    if !tool_available("anvil") || !tool_available("forge") {
        eprintln!("skipping: anvil/forge not installed");
        return;
    }
    let build = Command::new("forge").arg("build").current_dir(repo_root()).output().unwrap();
    assert!(build.status.success(), "forge build failed");

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
    let usdc_addr = deploy(&a, {
        let mut code = bytecode("MockERC20.sol", "MockERC20").to_vec();
        code.extend(alloy::sol_types::SolValue::abi_encode_params(&(
            "USD Coin".to_string(),
            "USDC".to_string(),
        )));
        code.into()
    })
    .await;

    // relay on a random port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router(Relay::default())).await.unwrap() });
    let relay = RelayClient::new(&relay_url);

    // accounts: 0 = resolver A (and deployer), 1 = resolver B, 2 = maker
    let (res_a, res_b, maker_key) = (signer(&a, 0), signer(&a, 1), signer(&a, 2));
    let maker_addr = maker_key.address();
    let usdc = |s: PrivateKeySigner| {
        MockToken::new(
            usdc_addr,
            ProviderBuilder::new().wallet(EthereumWallet::from(s)).connect_http(a.endpoint_url()),
        )
    };
    let thousand = parse_ether("1000").unwrap();
    usdc(res_a.clone())
        .mint(maker_addr, thousand)
        .send()
        .await
        .unwrap()
        .get_receipt()
        .await
        .unwrap();
    // the maker's only transaction ever: a one-time approval
    usdc(maker_key.clone())
        .approve(cfg_a.contract, U256::MAX)
        .send()
        .await
        .unwrap()
        .get_receipt()
        .await
        .unwrap();

    let chains = |s: &PrivateKeySigner| {
        vec![Chain::connect(&cfg_a, s.clone()).unwrap(), Chain::connect(&cfg_b, s.clone()).unwrap()]
    };
    let rate = |eth_per_1000: &str, bps| FixedRate {
        rate_num: parse_ether(eth_per_1000).unwrap(),
        rate_den: thousand,
        max_fill_bps: bps,
    };
    // A pays more, so it fills earlier in the auction, but only has budget for 60%
    let mut filler_a = IntentFiller::new(chains(&res_a), relay.clone(), rate("0.40", 6_000))
        .with_budget(parse_ether("600").unwrap());
    let mut filler_b = IntentFiller::new(chains(&res_b), relay.clone(), rate("0.30", 10_000));

    let maker = Maker::new(
        Chain::connect(&cfg_a, maker_key.clone()).unwrap(),
        Chain::connect(&cfg_b, maker_key.clone()).unwrap(),
        maker_key.clone(),
        relay.clone(),
    )
    .with_policy(TimelockPolicy::default());

    let mut intent = maker
        .open(
            IntentRequest {
                src_token: usdc_addr,
                src_amount: thousand,
                dst_token: NATIVE,
                start_dst_amount: parse_ether("0.5").unwrap(),
                end_dst_amount: parse_ether("0.25").unwrap(),
                auction_duration: 600,
                ttl: 3600,
                src_lock: 6 * 3600,
                parts: 4,
                receiver: None,
            },
            U256::from(1),
        )
        .await
        .unwrap();

    let pb = ProviderBuilder::new().connect_http(b.endpoint_url());
    let eth_before = pb.get_balance(maker_addr).await.unwrap();

    // walk the auction forward one minute at a time
    for _ in 0..12 {
        filler_a.tick().await.unwrap();
        filler_b.tick().await.unwrap();
        maker.tick(&mut intent).await.unwrap();
        filler_a.tick().await.unwrap();
        filler_b.tick().await.unwrap();
        let settled = filler_a.jobs.iter().chain(&filler_b.jobs);
        if settled.filter(|j| j.status == JobStatus::Settled).count() == 2 {
            break;
        }
        advance(&a, 60).await;
    }

    assert_eq!(filler_a.jobs.len(), 1);
    assert_eq!(filler_b.jobs.len(), 1);
    assert_eq!(filler_a.jobs[0].index, 2, "60% lands in bucket 2");
    assert_eq!(filler_b.jobs[0].index, 4, "final fill uses the last secret");
    assert!(filler_a.jobs.iter().chain(&filler_b.jobs).all(|j| j.status == JobStatus::Settled));
    assert!(maker.is_complete(&intent).await.unwrap());

    // maker was paid on B at auction prices between floor and ceiling
    let gained = pb.get_balance(maker_addr).await.unwrap() - eth_before;
    assert!(gained > parse_ether("0.25").unwrap() && gained < parse_ether("0.5").unwrap());

    // resolvers hold the USDC
    let token = usdc(res_a.clone());
    assert_eq!(token.balanceOf(res_a.address()).call().await.unwrap(), parse_ether("600").unwrap());
    assert_eq!(token.balanceOf(res_b.address()).call().await.unwrap(), parse_ether("400").unwrap());

    // gasless: nothing sent on B, only the approval on A
    let pa = ProviderBuilder::new().connect_http(a.endpoint_url());
    assert_eq!(pb.get_transaction_count(maker_addr).await.unwrap(), 0);
    assert_eq!(pa.get_transaction_count(maker_addr).await.unwrap(), 1);
}
