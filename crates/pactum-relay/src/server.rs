use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use alloy::primitives::{B256, Bytes, Signature};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use pactum_core::{SecretTree, secret};
use serde_json::json;
use thiserror::Error;
use tokio::sync::RwLock;

use crate::{OpenOrder, SecretRelease, SignedOrder};

#[derive(Debug, Error)]
pub enum RelayError {
    #[error("malformed signature")]
    MalformedSignature,
    #[error("signature is not from the maker")]
    WrongSigner,
    #[error("secret hashes do not match secretsRoot")]
    RootMismatch,
    #[error("unknown order")]
    UnknownOrder,
    #[error("secret not committed by this order")]
    BadSecret,
}

impl IntoResponse for RelayError {
    fn into_response(self) -> Response {
        let code = match self {
            RelayError::UnknownOrder => StatusCode::NOT_FOUND,
            _ => StatusCode::BAD_REQUEST,
        };
        (code, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

struct Entry {
    signed: SignedOrder,
    secrets: HashMap<usize, Bytes>,
}

/// In-memory relay state. Cheap to clone; all clones share the same store.
#[derive(Clone, Default)]
pub struct Relay {
    orders: Arc<RwLock<HashMap<B256, Entry>>>,
}

impl Relay {
    pub async fn add(&self, signed: SignedOrder) -> Result<B256, RelayError> {
        let hash = signed.hash();
        let sig = Signature::try_from(signed.signature.as_ref())
            .map_err(|_| RelayError::MalformedSignature)?;
        let signer =
            sig.recover_address_from_prehash(&hash).map_err(|_| RelayError::MalformedSignature)?;
        // EIP-1271 makers can't be checked off-chain; the contract verifies them at fill time
        if signer != signed.order.maker {
            return Err(RelayError::WrongSigner);
        }
        if SecretTree::new(signed.secret_hashes.clone()).root() != signed.order.secretsRoot {
            return Err(RelayError::RootMismatch);
        }
        self.orders.write().await.insert(hash, Entry { signed, secrets: HashMap::new() });
        Ok(hash)
    }

    pub async fn open(&self) -> Vec<OpenOrder> {
        self.orders
            .read()
            .await
            .iter()
            .map(|(hash, e)| OpenOrder { hash: *hash, signed: e.signed.clone() })
            .collect()
    }

    pub async fn release(&self, hash: B256, r: SecretRelease) -> Result<(), RelayError> {
        let mut orders = self.orders.write().await;
        let entry = orders.get_mut(&hash).ok_or(RelayError::UnknownOrder)?;
        let secret_hash = secret::sha256(&r.secret);
        if !SecretTree::verify(entry.signed.order.secretsRoot, r.index, secret_hash, &r.proof) {
            return Err(RelayError::BadSecret);
        }
        entry.secrets.insert(r.index, r.secret);
        Ok(())
    }

    pub async fn secret(&self, hash: B256, index: usize) -> Option<Bytes> {
        self.orders.read().await.get(&hash)?.secrets.get(&index).cloned()
    }
}

async fn post_order(
    State(relay): State<Relay>,
    Json(signed): Json<SignedOrder>,
) -> Result<impl IntoResponse, RelayError> {
    let hash = relay.add(signed).await?;
    tracing::info!(%hash, "order posted");
    Ok((StatusCode::CREATED, Json(json!({ "hash": hash }))))
}

async fn list_orders(State(relay): State<Relay>) -> Json<Vec<OpenOrder>> {
    Json(relay.open().await)
}

async fn post_secret(
    State(relay): State<Relay>,
    Path(hash): Path<B256>,
    Json(r): Json<SecretRelease>,
) -> Result<impl IntoResponse, RelayError> {
    let index = r.index;
    relay.release(hash, r).await?;
    tracing::info!(%hash, index, "secret released");
    Ok(StatusCode::CREATED)
}

async fn get_secret(
    State(relay): State<Relay>,
    Path((hash, index)): Path<(B256, usize)>,
) -> Response {
    match relay.secret(hash, index).await {
        Some(secret) => Json(json!({ "secret": secret })).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "not released" }))).into_response(),
    }
}

pub fn router(relay: Relay) -> Router {
    Router::new()
        .route("/orders", post(post_order).get(list_orders))
        .route("/orders/{hash}/secrets", post(post_secret))
        .route("/orders/{hash}/secrets/{index}", get(get_secret))
        .with_state(relay)
}

pub async fn serve(addr: SocketAddr, relay: Relay) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "relay listening");
    axum::serve(listener, router(relay)).await?;
    Ok(())
}
