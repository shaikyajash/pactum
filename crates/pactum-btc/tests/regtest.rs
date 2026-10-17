//! HTLC spends validated by a real Bitcoin Core node (regtest, in Docker).
//! Skips when Docker is unavailable.

use std::{process::Command, time::Duration};

use bitcoin::{
    Address, Amount, Network, OutPoint,
    hashes::{Hash, sha256},
    key::{Keypair, Secp256k1},
    secp256k1::rand::thread_rng,
};
use pactum_btc::{BitcoinRpc, Htlc};
use serde_json::json;

const IMAGE: &str = "bitcoin/bitcoin:28.1";

struct Node {
    id: String,
    rpc: BitcoinRpc,
    wallet: BitcoinRpc,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["stop", &self.id]).output();
    }
}

async fn start() -> Option<Node> {
    let docker = Command::new("docker").arg("version").output();
    if !docker.is_ok_and(|o| o.status.success()) {
        eprintln!("skipping: docker not available");
        return None;
    }
    let out = Command::new("docker")
        .args(["run", "-d", "--rm", "-p", "127.0.0.1::18443", IMAGE])
        .args(["-regtest", "-server", "-rpcuser=pactum", "-rpcpassword=pactum"])
        .args(["-rpcbind=0.0.0.0", "-rpcallowip=0.0.0.0/0", "-fallbackfee=0.0002", "-txindex=1"])
        .output()
        .unwrap();
    assert!(out.status.success(), "docker run: {}", String::from_utf8_lossy(&out.stderr));
    let id = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    let port = String::from_utf8(
        Command::new("docker").args(["port", &id, "18443"]).output().unwrap().stdout,
    )
    .unwrap();
    let port = port.lines().next().unwrap().rsplit(':').next().unwrap().to_owned();

    let rpc = BitcoinRpc::new(format!("http://127.0.0.1:{port}"), "pactum", "pactum");
    for _ in 0..60 {
        if rpc.block_count().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let _: serde_json::Value = rpc.call("createwallet", json!(["miner"])).await.unwrap();
    let wallet = rpc.wallet("miner");
    let node = Node { id, rpc, wallet };
    node.mine(101).await; // mature a coinbase
    Some(node)
}

impl Node {
    async fn new_address(&self) -> Address {
        let a: String = self.wallet.call("getnewaddress", json!(["", "bech32m"])).await.unwrap();
        a.parse::<Address<_>>().unwrap().require_network(Network::Regtest).unwrap()
    }

    async fn mine(&self, n: u64) {
        let to = self.new_address().await;
        let _: serde_json::Value =
            self.rpc.call("generatetoaddress", json!([n, to.to_string()])).await.unwrap();
    }

    /// Fund `htlc` with `btc`, confirm it, and return the funding outpoint.
    async fn fund(&self, htlc: &Htlc, btc: f64) -> (OutPoint, Amount) {
        let addr = htlc.address(&Secp256k1::new(), Network::Regtest).unwrap();
        let _: String =
            self.wallet.call("sendtoaddress", json!([addr.to_string(), btc])).await.unwrap();
        self.mine(1).await;
        let utxos = self.rpc.utxos(&addr).await.unwrap();
        assert_eq!(utxos.len(), 1);
        (utxos[0].outpoint, utxos[0].value)
    }
}

/// HTLC refundable at block `refund_height` (heights are easy to drive on regtest;
/// production locks are Unix timestamps).
fn htlc(refund_height: u32) -> (Htlc, Keypair, Keypair, [u8; 32]) {
    let secp = Secp256k1::new();
    let redeemer = Keypair::new(&secp, &mut thread_rng());
    let refunder = Keypair::new(&secp, &mut thread_rng());
    let secret: [u8; 32] = bitcoin::secp256k1::rand::random();
    let h = Htlc {
        secret_hash: sha256::Hash::hash(&secret).to_byte_array(),
        redeemer: redeemer.x_only_public_key().0,
        refunder: refunder.x_only_public_key().0,
        refund_after: bitcoin::absolute::LockTime::from_height(refund_height).unwrap(),
    };
    (h, redeemer, refunder, secret)
}

#[tokio::test]
async fn redeem_with_secret_and_recover_it_from_the_chain() {
    let Some(node) = start().await else { return };
    let (h, redeemer, refunder, secret) = htlc(10_000);
    let (funding, value) = node.fund(&h, 0.5).await;
    let fee = Amount::from_sat(1_000);
    let dest = node.new_address().await.script_pubkey();

    // the refunder can't take it early: the lock height is far away
    let early = h.refund_tx(funding, value, dest.clone(), fee, &refunder).unwrap();
    let err = node.rpc.send_raw(&early).await.unwrap_err().to_string();
    assert!(err.contains("non-final"), "{err}");

    // the redeemer spends with the secret; Bitcoin Core validates the script path
    let tx = h.redeem_tx(funding, value, dest, fee, &secret, &redeemer).unwrap();
    let txid = node.rpc.send_raw(&tx).await.unwrap();
    node.mine(1).await;

    // a watcher that only knows the HTLC recovers the secret from the block
    let tip = node.rpc.block_hash(node.rpc.block_count().await.unwrap()).await.unwrap();
    let mined = node.rpc.block_txs(tip).await.unwrap();
    let spend = mined.iter().find(|t| t.compute_txid() == txid).expect("redeem mined");
    assert_eq!(h.extract_secret(spend, funding), Some(secret));

    let addr = h.address(&Secp256k1::new(), Network::Regtest).unwrap();
    assert!(node.rpc.utxos(&addr).await.unwrap().is_empty(), "HTLC spent");
}

#[tokio::test]
async fn refund_after_absolute_lock() {
    let Some(node) = start().await else { return };
    // the chain is at height 101 after setup; lock until 112
    let (h, _, refunder, _) = htlc(112);
    let (funding, value) = node.fund(&h, 0.25).await; // mines block 102
    let fee = Amount::from_sat(1_000);
    let dest = node.new_address().await.script_pubkey();
    let refund = h.refund_tx(funding, value, dest, fee, &refunder).unwrap();

    // a tx with nLockTime = H is valid only in blocks after H
    node.mine(9).await; // tip 111: next block is 112, still not > 112
    let err = node.rpc.send_raw(&refund).await.unwrap_err().to_string();
    assert!(err.contains("non-final"), "{err}");

    node.mine(1).await; // tip 112: next block 113
    node.rpc.send_raw(&refund).await.unwrap();
    node.mine(1).await;
    let addr = h.address(&Secp256k1::new(), Network::Regtest).unwrap();
    assert!(node.rpc.utxos(&addr).await.unwrap().is_empty(), "HTLC refunded");
}

#[tokio::test]
async fn wrong_key_cannot_redeem_even_with_the_secret() {
    let Some(node) = start().await else { return };
    let (h, _, refunder, secret) = htlc(10_000);
    let (funding, value) = node.fund(&h, 0.1).await;
    let dest = node.new_address().await.script_pubkey();

    // someone who saw the secret but isn't the redeemer
    let thief = Htlc { redeemer: refunder.x_only_public_key().0, ..h.clone() };
    let mut tx =
        thief.redeem_tx(funding, value, dest, Amount::from_sat(1_000), &secret, &refunder).unwrap();
    // graft the real HTLC's control block so the tx targets the funded output
    let real = h.spend_info(&Secp256k1::new()).unwrap();
    let control =
        real.control_block(&(h.redeem_script(), bitcoin::taproot::LeafVersion::TapScript)).unwrap();
    let mut w: Vec<Vec<u8>> = tx.input[0].witness.iter().map(|x| x.to_vec()).collect();
    w[2] = h.redeem_script().to_bytes();
    w[3] = control.serialize();
    tx.input[0].witness = bitcoin::Witness::from_slice(&w);

    let err = node.rpc.send_raw(&tx).await.unwrap_err().to_string();
    assert!(err.contains("Invalid Schnorr signature") || err.contains("script-verify"), "{err}");
}
