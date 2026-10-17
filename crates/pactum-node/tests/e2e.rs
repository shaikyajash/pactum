//! End-to-end scenarios on two live anvil chains. In every scenario the maker
//! signs one intent and one EIP-2612 permit and never sends a transaction.

mod common;

use alloy::primitives::{U256, utils::parse_ether};
use common::{Env, advance, rate, thousand};
use pactum_core::SecretTree;
use pactum_node::{Chain, IntentFiller, IntentRequest, JobStatus, Keeper, Maker, NATIVE};

fn request(env: &Env, parts: u16) -> IntentRequest {
    IntentRequest {
        src_token: env.usdc,
        src_amount: thousand(),
        dst_token: NATIVE.into_word(),
        start_dst_amount: parse_ether("0.5").unwrap(),
        end_dst_amount: parse_ether("0.25").unwrap(),
        auction_duration: 600,
        ttl: 3600,
        src_lock: 6 * 3600,
        parts,
        receiver: None,
        min_deposit: parse_ether("0.01").unwrap(),
        permit_version: Some("1".into()),
    }
}

fn maker(env: &Env) -> Maker {
    let m = env.maker();
    Maker::new(
        Chain::connect(&env.cfg_a, m.clone()).unwrap(),
        Chain::connect(&env.cfg_b, m.clone()).unwrap(),
        m,
        env.relay.clone(),
    )
}

/// Two resolvers compete in the Dutch auction and split one order.
#[tokio::test]
async fn gasless_split_intent_across_two_chains() {
    let Some(env) = Env::setup().await else { return };
    let maker_addr = env.maker().address();

    // A pays more, so it fills earlier in the auction, but only has budget for 60%
    let mut filler_a =
        IntentFiller::new(env.chains(&env.resolver_a()), env.relay.clone(), rate("0.40", 6_000))
            .with_budget(parse_ether("600").unwrap());
    let mut filler_b =
        IntentFiller::new(env.chains(&env.resolver_b()), env.relay.clone(), rate("0.30", 10_000));

    let maker = maker(&env);
    let mut intent = maker.open(request(&env, 4), U256::from(1)).await.unwrap();
    let eth_before = env.eth_of(&env.b, maker_addr).await;

    for _ in 0..12 {
        filler_a.tick().await.unwrap();
        filler_b.tick().await.unwrap();
        maker.tick(&mut intent).await.unwrap();
        filler_a.tick().await.unwrap();
        filler_b.tick().await.unwrap();
        if maker.is_complete(&intent).await.unwrap()
            && filler_a.jobs.iter().chain(&filler_b.jobs).all(|j| j.status == JobStatus::Settled)
        {
            break;
        }
        advance(&env.a, 60).await;
    }

    assert_eq!(filler_a.jobs.len(), 1);
    assert_eq!(filler_b.jobs.len(), 1);
    assert_eq!(filler_a.jobs[0].index, 2, "60% lands in bucket 2");
    assert_eq!(filler_b.jobs[0].index, 4, "final fill uses the last secret");
    assert!(maker.is_complete(&intent).await.unwrap());

    let gained = env.eth_of(&env.b, maker_addr).await - eth_before;
    assert!(gained > parse_ether("0.25").unwrap() && gained < parse_ether("0.5").unwrap());
    assert_eq!(env.usdc_of(env.resolver_a().address()).await, parse_ether("600").unwrap());
    assert_eq!(env.usdc_of(env.resolver_b().address()).await, parse_ether("400").unwrap());

    // fully gasless: the maker never sent a transaction on either chain
    assert_eq!(env.nonce(&env.a, maker_addr).await, 0);
    assert_eq!(env.nonce(&env.b, maker_addr).await, 0);

    // resolvers swept their deposits back out of the pull ledger
    let a = Chain::connect(&env.cfg_a, env.maker()).unwrap();
    for r in [env.resolver_a().address(), env.resolver_b().address()] {
        assert_eq!(a.deposit_credit(r).await.unwrap(), U256::ZERO);
    }
}

/// A rogue resolver locks the maker's funds and never mirrors the lock. After
/// the source lock expires a keeper refunds the maker and earns a 10% tip; the
/// other 90% of the rogue's deposit is credited to the maker as compensation.
#[tokio::test]
async fn abandoned_fill_is_refunded_by_keeper() {
    let Some(env) = Env::setup().await else { return };
    let maker_addr = env.maker().address();
    let maker = maker(&env);
    let _intent = maker.open(request(&env, 1), U256::from(2)).await.unwrap();

    let rogue = Chain::connect(&env.cfg_a, env.resolver_b()).unwrap();
    let mut keeper = Keeper::new(env.chains(&env.keeper()), U256::from(1)).from_block(0);

    let posted = &env.relay.orders().await.unwrap()[0];
    let tree = SecretTree::new(posted.signed.secret_hashes.clone());
    let fill = rogue
        .fill(
            &posted.signed.order,
            posted.signed.signature.clone(),
            thousand(),
            0,
            posted.signed.secret_hashes[0],
            tree.proof(0),
            posted.signed.permit.as_ref(),
        )
        .await
        .unwrap();
    assert_eq!(env.usdc_of(maker_addr).await, U256::ZERO, "maker's funds are locked");

    keeper.tick().await.unwrap();
    assert!(keeper.refunded.is_empty(), "nothing to do before expiry");

    advance(&env.a, 6 * 3600).await;
    keeper.tick().await.unwrap();

    assert_eq!(keeper.refunded, vec![(env.cfg_a.id, fill.swap_id)]);
    assert_eq!(env.usdc_of(maker_addr).await, thousand(), "maker made whole");
    assert_eq!(env.nonce(&env.a, maker_addr).await, 0, "without sending a transaction");

    let deposit = parse_ether("0.01").unwrap();
    let maker_chain = Chain::connect(&env.cfg_a, env.maker()).unwrap();
    assert_eq!(
        maker_chain.deposit_credit(maker_addr).await.unwrap(),
        deposit * U256::from(9) / U256::from(10),
        "maker compensated with 90% of the rogue's deposit"
    );
    let keeper_addr = env.keeper().address();
    assert_eq!(
        maker_chain.deposit_credit(keeper_addr).await.unwrap(),
        U256::ZERO,
        "keeper already withdrew its tip"
    );
}

/// A resolver crashes after filling; a fresh process with the same state file
/// picks the job up and settles it.
#[tokio::test]
async fn resolver_resumes_after_restart() {
    let Some(env) = Env::setup().await else { return };
    let dir = std::env::temp_dir().join(format!("pactum-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let state = dir.join("resolver.json");

    let maker = maker(&env);
    let mut intent = maker.open(request(&env, 1), U256::from(3)).await.unwrap();
    advance(&env.a, 600).await; // auction at its floor

    let make_filler = || {
        IntentFiller::new(env.chains(&env.resolver_a()), env.relay.clone(), rate("0.30", 10_000))
            .with_store(&state)
            .unwrap()
    };
    let mut first = make_filler();
    first.tick().await.unwrap();
    assert_eq!(first.jobs.len(), 1);
    assert_eq!(first.jobs[0].status, JobStatus::Locked);
    drop(first); // crash before the secret is released

    maker.tick(&mut intent).await.unwrap();

    let mut second = make_filler();
    assert_eq!(second.jobs.len(), 1, "job restored from disk");
    second.tick().await.unwrap();
    assert_eq!(second.jobs[0].status, JobStatus::Settled);
    assert_eq!(second.jobs.len(), 1, "did not fill the order twice");
    assert!(maker.is_complete(&intent).await.unwrap());

    std::fs::remove_dir_all(dir).unwrap();
}

/// Cross-ecosystem: the maker sells USDC on an EVM chain for native BTC on
/// Bitcoin (regtest). The resolver fills on EVM and funds a Taproot HTLC; the
/// maker verifies it on chain and claims it, revealing the secret on Bitcoin;
/// the resolver reads the secret from Bitcoin and claims the USDC.
#[tokio::test]
async fn usdc_on_evm_for_native_btc() {
    let Some(env) = Env::setup().await else { return };
    let Some(btc) = common::BtcNode::start().await else { return };
    let maker_addr = env.maker().address();

    let resolver_leg = btc.leg("resolver").await;
    let maker_leg = btc.leg("maker").await;
    btc.mine_to(&resolver_leg.wallet, 101).await; // mature coinbase for the resolver

    let maker = Maker::to_bitcoin(
        Chain::connect(&env.cfg_a, env.maker()).unwrap(),
        maker_leg.clone(),
        env.maker(),
        env.relay.clone(),
    );
    let mut intent = maker
        .open(
            IntentRequest {
                dst_token: alloy::primitives::B256::ZERO,   // native BTC
                start_dst_amount: U256::from(2_000_000u64), // sats
                end_dst_amount: U256::from(1_500_000u64),
                parts: 1,
                ..request(&env, 1)
            },
            U256::from(4),
        )
        .await
        .unwrap();
    assert_eq!(
        intent.order.dstChainId,
        pactum_core::chains::ChainKind::Bitcoin(pactum_core::chains::BitcoinNetwork::Regtest)
            .chain_id()
    );

    // resolver pays 1.6M sats per 1000 USDC
    let mut filler = IntentFiller::new(
        env.chains(&env.resolver_a()),
        env.relay.clone(),
        pactum_node::filler::FixedRate {
            rate_num: U256::from(1_600_000u64),
            rate_den: thousand(),
            max_fill_bps: 10_000,
        },
    )
    .with_bitcoin(resolver_leg.clone());

    let maker_btc_before = btc.balance_sats(&maker_leg.wallet).await;
    for _ in 0..15 {
        filler.tick().await.unwrap();
        btc.mine_to(&resolver_leg.wallet, 1).await; // confirm any funding / claim
        maker.tick(&mut intent).await.unwrap();
        btc.mine_to(&resolver_leg.wallet, 1).await;
        filler.tick().await.unwrap();
        if filler.jobs.first().is_some_and(|j| j.status == JobStatus::Settled) {
            break;
        }
        advance(&env.a, 60).await;
    }

    let job = filler.jobs.first().expect("resolver filled");
    assert_eq!(job.status, JobStatus::Settled);
    assert!(job.btc.is_some(), "counter-lock was on Bitcoin");
    assert!(maker.is_complete(&intent).await.unwrap());

    // maker got the BTC (minus the claim fee), resolver got the USDC
    let got = btc.balance_sats(&maker_leg.wallet).await - maker_btc_before;
    assert!((1_500_000..=1_600_000).contains(&(got + 2_000)), "maker received {got} sats");
    assert_eq!(env.usdc_of(env.resolver_a().address()).await, thousand());
    // and never sent an EVM transaction
    assert_eq!(env.nonce(&env.a, maker_addr).await, 0);
}
