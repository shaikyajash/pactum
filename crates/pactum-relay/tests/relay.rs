use alloy::{
    primitives::{Address, U256},
    signers::{SignerSync, local::PrivateKeySigner},
};
use pactum_core::{Order, OrderSecrets};
use pactum_relay::{Relay, RelayClient, SecretRelease, SignedOrder, router};

async fn spawn() -> RelayClient {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(Relay::default())).await.unwrap() });
    RelayClient::new(format!("http://{addr}"))
}

fn signed(signer: &PrivateKeySigner, maker: Address, secrets: &OrderSecrets) -> SignedOrder {
    let order = Order {
        maker,
        receiver: maker,
        srcToken: Address::repeat_byte(1),
        srcAmount: U256::from(1000),
        parts: 4,
        secretsRoot: secrets.root(),
        ..Default::default()
    };
    let contract = Address::repeat_byte(0xcc);
    let sig = signer.sign_hash_sync(&order.digest(1, contract)).unwrap();
    SignedOrder {
        order,
        signature: sig.as_bytes().into(),
        src_chain_id: 1,
        contract,
        secret_hashes: secrets.hashes(),
    }
}

#[tokio::test]
async fn post_list_and_release() {
    let relay = spawn().await;
    let maker = PrivateKeySigner::random();
    let secrets = OrderSecrets::random(4);
    let o = signed(&maker, maker.address(), &secrets);

    let hash = relay.post(&o).await.unwrap();
    assert_eq!(hash, o.hash());

    let open = relay.orders().await.unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].signed.order, o.order);

    assert_eq!(relay.secret(hash, 2).await.unwrap(), None);
    let release = SecretRelease {
        index: 2,
        secret: secrets.secrets[2].secret.clone(),
        proof: secrets.tree.proof(2),
    };
    relay.release(hash, &release).await.unwrap();
    assert_eq!(relay.secret(hash, 2).await.unwrap(), Some(secrets.secrets[2].secret.clone()));
}

#[tokio::test]
async fn rejects_orders_not_signed_by_maker() {
    let relay = spawn().await;
    let secrets = OrderSecrets::random(4);
    let o = signed(&PrivateKeySigner::random(), Address::repeat_byte(7), &secrets);
    assert!(relay.post(&o).await.unwrap_err().to_string().contains("not from the maker"));
}

#[tokio::test]
async fn rejects_hashes_that_do_not_match_root() {
    let relay = spawn().await;
    let maker = PrivateKeySigner::random();
    let mut o = signed(&maker, maker.address(), &OrderSecrets::random(4));
    o.secret_hashes = OrderSecrets::random(4).hashes();
    assert!(relay.post(&o).await.unwrap_err().to_string().contains("secretsRoot"));
}

#[tokio::test]
async fn rejects_uncommitted_secrets() {
    let relay = spawn().await;
    let maker = PrivateKeySigner::random();
    let secrets = OrderSecrets::random(4);
    let hash = relay.post(&signed(&maker, maker.address(), &secrets)).await.unwrap();

    // right proof, wrong preimage
    let bogus = SecretRelease {
        index: 1,
        secret: secrets.secrets[0].secret.clone(),
        proof: secrets.tree.proof(1),
    };
    assert!(relay.release(hash, &bogus).await.is_err());
    assert_eq!(relay.secret(hash, 1).await.unwrap(), None);
}
