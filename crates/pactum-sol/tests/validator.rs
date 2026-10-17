//! The HTLC program running on a real `solana-test-validator` (Docker).
//! Needs `programs/pactum-solana/target/deploy/pactum_solana.so`, built with
//! `cargo-build-sbf`; skips when Docker or the build artifact is missing.

use std::{path::PathBuf, process::Command, time::Duration};

use pactum_sol::{EscrowState, HtlcProgram, Keypair, Pubkey, SolanaRpc};
use sha2::{Digest, Sha256};

pub struct Validator {
    id: String,
    pub program: HtlcProgram,
}

impl Drop for Validator {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["stop", &self.id]).output();
    }
}

fn deploy_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../programs/pactum-solana/target/deploy")
}

/// Deterministic program id for tests.
pub fn program_id() -> Pubkey {
    Keypair::from_seed([0x50; 32]).pubkey()
}

pub async fn start() -> Option<Validator> {
    let so = deploy_dir().join("pactum_solana.so");
    let docker = Command::new("docker").arg("version").output();
    if !docker.is_ok_and(|o| o.status.success()) || !so.exists() {
        eprintln!("skipping: needs docker and {}", so.display());
        return None;
    }
    let out = Command::new("docker")
        .args(["run", "-d", "--rm", "--platform", "linux/amd64", "--entrypoint", "sh"])
        .args(["-p", "127.0.0.1::8899", "-v"])
        .arg(format!("{}:/prog:ro", deploy_dir().display()))
        .arg("pactum-solana:dev")
        .arg("-c")
        .arg(format!(
            "solana-test-validator --reset --quiet --ledger /tmp/l --bpf-program {} /prog/pactum_solana.so",
            program_id()
        ))
        .output()
        .unwrap();
    assert!(out.status.success(), "docker run: {}", String::from_utf8_lossy(&out.stderr));
    let id = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    let port = String::from_utf8(
        Command::new("docker").args(["port", &id, "8899"]).output().unwrap().stdout,
    )
    .unwrap();
    let port = port.lines().next().unwrap().rsplit(':').next().unwrap().to_owned();
    let rpc = SolanaRpc::new(format!("http://127.0.0.1:{port}"));
    for _ in 0..120 {
        if rpc.latest_blockhash().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Some(Validator { id, program: HtlcProgram { id: program_id(), rpc } })
}

fn sha(b: &[u8]) -> [u8; 32] {
    Sha256::digest(b).into()
}

#[tokio::test]
async fn htlc_program_on_a_live_validator() {
    let Some(v) = start().await else { return };
    let (p, rpc) = (&v.program, &v.program.rpc);

    let initiator = Keypair::generate();
    let redeemer = Keypair::generate().pubkey();
    let relayer = Keypair::generate(); // pays fees; never receives the funds
    rpc.airdrop(&initiator.pubkey(), 5_000_000_000).await.unwrap();
    rpc.airdrop(&relayer.pubkey(), 1_000_000_000).await.unwrap();

    // --- redeem: a third party settles, the fixed redeemer is paid
    let secret = [0x42u8; 32];
    let swap = sha(b"swap-1");
    let now = rpc.chain_time().await.unwrap();
    p.initiate(&initiator, swap, redeemer, sha(&secret), 1_000_000_000, now + 3600).await.unwrap();

    let e = p.escrow(&swap).await.unwrap().expect("escrow exists");
    assert_eq!((e.initiator, e.redeemer, e.amount), (initiator.pubkey(), redeemer, 1_000_000_000));
    assert_eq!(e.state, EscrowState::Open);

    // the same swap id can't be opened twice
    assert!(p.initiate(&initiator, swap, redeemer, sha(&secret), 1, now + 3600).await.is_err());
    // wrong secret is rejected; early refund is rejected
    assert!(p.redeem(&relayer, swap, redeemer, [0u8; 32]).await.is_err());
    assert!(p.refund(&relayer, swap, initiator.pubkey()).await.is_err());
    // redeemer must be the stored one
    assert!(p.redeem(&relayer, swap, relayer.pubkey(), secret).await.is_err());

    p.redeem(&relayer, swap, redeemer, secret).await.unwrap();
    assert_eq!(rpc.balance(&redeemer).await.unwrap(), 1_000_000_000);
    let e = p.escrow(&swap).await.unwrap().unwrap();
    assert_eq!(e.state, EscrowState::Redeemed);
    assert_eq!(e.secret, Some(secret), "watchers read the secret from the escrow");
    assert!(p.redeem(&relayer, swap, redeemer, secret).await.is_err(), "no double redeem");

    // initiator reclaims the escrow's rent
    let before = rpc.balance(&initiator.pubkey()).await.unwrap();
    p.close(&initiator, swap).await.unwrap();
    assert!(rpc.balance(&initiator.pubkey()).await.unwrap() > before);
    assert_eq!(p.escrow(&swap).await.unwrap(), None);

    // --- refund: after expiry anyone can return the funds to the initiator
    let swap2 = sha(b"swap-2");
    let now = rpc.chain_time().await.unwrap();
    p.initiate(&initiator, swap2, redeemer, sha(&[7u8; 32]), 500_000_000, now + 4).await.unwrap();
    let before = rpc.balance(&initiator.pubkey()).await.unwrap();
    let mut refunded = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if p.refund(&relayer, swap2, initiator.pubkey()).await.is_ok() {
            refunded = true;
            break;
        }
    }
    assert!(refunded, "refund opens at expiry");
    assert_eq!(rpc.balance(&initiator.pubkey()).await.unwrap(), before + 500_000_000);
    assert_eq!(p.escrow(&swap2).await.unwrap().unwrap().state, EscrowState::Refunded);
}
