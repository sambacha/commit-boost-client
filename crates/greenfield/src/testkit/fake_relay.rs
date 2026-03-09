use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::types::{RegisterValidatorV2Request, ValidatorRegistration};

#[derive(Debug, Clone)]
pub struct FakeRelayConfig {
    pub supports_register_v2: bool,
    pub not_found_v2: bool,
    pub validate_registrations: bool,
    pub register_validator_delay_ms: u64,
    pub response_override: Option<StatusCode>,
}

impl Default for FakeRelayConfig {
    fn default() -> Self {
        Self {
            supports_register_v2: true,
            not_found_v2: false,
            validate_registrations: true,
            register_validator_delay_ms: 0,
            response_override: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FakeRelayMetrics {
    pub received_get_status: u64,
    pub received_register_validator: u64,
    pub received_register_validator_v1: u64,
    pub received_register_validator_v2: u64,
    pub register_validator_inflight: u64,
    pub register_validator_max_inflight: u64,
    pub register_validator_v2_idempotency_keys: u64,
    pub stored_registrations: u64,
}

#[derive(Debug)]
pub struct FakeRelayState {
    config: FakeRelayConfig,
    received_get_status: AtomicU64,
    received_register_validator: AtomicU64,
    received_register_validator_v1: AtomicU64,
    received_register_validator_v2: AtomicU64,
    register_validator_inflight: AtomicU64,
    register_validator_max_inflight: AtomicU64,
    register_validator_v2_idempotency_keys: RwLock<Vec<String>>,
    stored_registrations: RwLock<HashMap<String, ValidatorRegistration>>,
}

impl FakeRelayState {
    fn new(config: FakeRelayConfig) -> Self {
        Self {
            config,
            received_get_status: AtomicU64::new(0),
            received_register_validator: AtomicU64::new(0),
            received_register_validator_v1: AtomicU64::new(0),
            received_register_validator_v2: AtomicU64::new(0),
            register_validator_inflight: AtomicU64::new(0),
            register_validator_max_inflight: AtomicU64::new(0),
            register_validator_v2_idempotency_keys: RwLock::new(Vec::new()),
            stored_registrations: RwLock::new(HashMap::new()),
        }
    }

    pub fn received_register_validator_v1(&self) -> u64 {
        self.received_register_validator_v1.load(Ordering::Relaxed)
    }

    pub fn received_register_validator_v2(&self) -> u64 {
        self.received_register_validator_v2.load(Ordering::Relaxed)
    }

    pub fn register_validator_max_inflight(&self) -> u64 {
        self.register_validator_max_inflight.load(Ordering::Relaxed)
    }

    pub fn v2_idempotency_keys(&self) -> Vec<String> {
        self.register_validator_v2_idempotency_keys.read().clone()
    }

    pub fn stored_registrations(&self) -> Vec<ValidatorRegistration> {
        self.stored_registrations.read().values().cloned().collect()
    }

    pub fn stored_registration_by_pubkey(&self, pubkey: &str) -> Option<ValidatorRegistration> {
        self.stored_registrations.read().get(&pubkey.to_lowercase()).cloned()
    }

    fn metrics(&self) -> FakeRelayMetrics {
        FakeRelayMetrics {
            received_get_status: self.received_get_status.load(Ordering::Relaxed),
            received_register_validator: self.received_register_validator.load(Ordering::Relaxed),
            received_register_validator_v1: self
                .received_register_validator_v1
                .load(Ordering::Relaxed),
            received_register_validator_v2: self
                .received_register_validator_v2
                .load(Ordering::Relaxed),
            register_validator_inflight: self.register_validator_inflight.load(Ordering::Relaxed),
            register_validator_max_inflight: self
                .register_validator_max_inflight
                .load(Ordering::Relaxed),
            register_validator_v2_idempotency_keys: self
                .register_validator_v2_idempotency_keys
                .read()
                .len() as u64,
            stored_registrations: self.stored_registrations.read().len() as u64,
        }
    }
}

pub struct FakeRelayHandle {
    pub base_url: String,
    pub state: Arc<FakeRelayState>,
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl FakeRelayHandle {
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

pub async fn start_fake_relay(config: FakeRelayConfig) -> eyre::Result<FakeRelayHandle> {
    let state = Arc::new(FakeRelayState::new(config));
    let app = Router::new()
        .route("/eth/v1/builder/status", get(handle_get_status))
        .route("/eth/v1/builder/validators", post(handle_register_validator_v1))
        .route("/eth/v2/builder/validators", post(handle_register_validator_v2))
        .route("/debug/metrics", get(handle_debug_metrics))
        .route("/debug/registrations", get(handle_debug_registrations))
        .route("/debug/registrations/{pubkey}", get(handle_debug_registration_by_pubkey))
        .with_state(Arc::clone(&state));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr: SocketAddr = listener.local_addr()?;
    let base_url = format!("http://{addr}");
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    tokio::spawn(async move {
        let shutdown = async move {
            let _ = shutdown_rx.await;
        };

        if let Err(err) = axum::serve(listener, app).with_graceful_shutdown(shutdown).await {
            tracing::error!(%err, "fake relay server failed");
        }
    });

    Ok(FakeRelayHandle { base_url, state, shutdown_tx: Some(shutdown_tx) })
}

async fn handle_get_status(State(state): State<Arc<FakeRelayState>>) -> impl IntoResponse {
    state.received_get_status.fetch_add(1, Ordering::Relaxed);
    state.config.response_override.unwrap_or(StatusCode::OK).into_response()
}

async fn handle_register_validator_v1(
    State(state): State<Arc<FakeRelayState>>,
    axum::Json(registrations): axum::Json<Vec<ValidatorRegistration>>,
) -> impl IntoResponse {
    registration_request_start(&state, false);

    let status = registration_status(&state, &registrations).await;
    if status.is_success() {
        store_registrations(&state, &registrations);
    }

    registration_request_end(&state);
    status.into_response()
}

async fn handle_register_validator_v2(
    State(state): State<Arc<FakeRelayState>>,
    axum::Json(request): axum::Json<RegisterValidatorV2Request>,
) -> impl IntoResponse {
    if !state.config.supports_register_v2 || state.config.not_found_v2 {
        return StatusCode::NOT_FOUND.into_response();
    }

    registration_request_start(&state, true);
    state
        .register_validator_v2_idempotency_keys
        .write()
        .push(request.context.idempotency_key.clone());

    let status = registration_status(&state, &request.registrations).await;
    if status.is_success() {
        store_registrations(&state, &request.registrations);
    }

    registration_request_end(&state);
    status.into_response()
}

fn registration_request_start(state: &Arc<FakeRelayState>, is_v2: bool) {
    state.received_register_validator.fetch_add(1, Ordering::Relaxed);
    if is_v2 {
        state.received_register_validator_v2.fetch_add(1, Ordering::Relaxed);
    } else {
        state.received_register_validator_v1.fetch_add(1, Ordering::Relaxed);
    }

    let inflight = state.register_validator_inflight.fetch_add(1, Ordering::Relaxed) + 1;
    state.register_validator_max_inflight.fetch_max(inflight, Ordering::Relaxed);
}

async fn registration_status(
    state: &Arc<FakeRelayState>,
    registrations: &[ValidatorRegistration],
) -> StatusCode {
    if state.config.register_validator_delay_ms > 0 {
        tokio::time::sleep(tokio::time::Duration::from_millis(
            state.config.register_validator_delay_ms,
        ))
        .await;
    }

    if state.config.validate_registrations
        && registrations
            .iter()
            .any(|registration| registration.pubkey.is_empty() || registration.signature.is_empty())
    {
        return StatusCode::BAD_REQUEST;
    }

    state.config.response_override.unwrap_or(StatusCode::OK)
}

fn registration_request_end(state: &Arc<FakeRelayState>) {
    state.register_validator_inflight.fetch_sub(1, Ordering::Relaxed);
}

fn store_registrations(state: &Arc<FakeRelayState>, registrations: &[ValidatorRegistration]) {
    let mut guard = state.stored_registrations.write();
    for registration in registrations {
        guard.insert(registration.pubkey.to_lowercase(), registration.clone());
    }
}

async fn handle_debug_metrics(State(state): State<Arc<FakeRelayState>>) -> impl IntoResponse {
    axum::Json(state.metrics())
}

async fn handle_debug_registrations(State(state): State<Arc<FakeRelayState>>) -> impl IntoResponse {
    axum::Json(state.stored_registrations())
}

#[derive(Debug, Serialize, Deserialize)]
struct RegistrationDebugResponse {
    registration: Option<ValidatorRegistration>,
}

async fn handle_debug_registration_by_pubkey(
    State(state): State<Arc<FakeRelayState>>,
    Path(pubkey): Path<String>,
) -> impl IntoResponse {
    axum::Json(RegistrationDebugResponse {
        registration: state.stored_registration_by_pubkey(&pubkey),
    })
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;

    use super::{FakeRelayConfig, start_fake_relay};
    use crate::types::{
        RegisterValidatorContext, RegisterValidatorV2Request, RegistrationMode,
        ValidatorRegistration,
    };

    fn registration(pubkey: &str) -> ValidatorRegistration {
        ValidatorRegistration {
            pubkey: pubkey.to_string(),
            fee_recipient: "0xfee".to_string(),
            gas_limit: 1,
            timestamp: 1,
            signature: "0xsig".to_string(),
        }
    }

    #[tokio::test]
    async fn test_persists_v2_registrations_and_exposes_debug_api() {
        let relay = start_fake_relay(FakeRelayConfig::default()).await.unwrap();
        let client = reqwest::Client::new();

        let response = client
            .post(format!("{}/eth/v2/builder/validators", relay.base_url))
            .json(&RegisterValidatorV2Request {
                registrations: vec![registration("0xabc")],
                context: RegisterValidatorContext {
                    idempotency_key: "idem-1".to_string(),
                    source: Some("test".to_string()),
                    mode: RegistrationMode::All,
                },
            })
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let registrations: Vec<ValidatorRegistration> = client
            .get(format!("{}/debug/registrations", relay.base_url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(registrations.len(), 1);

        let metrics: serde_json::Value = client
            .get(format!("{}/debug/metrics", relay.base_url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(metrics["received_register_validator_v2"], 1);
        assert_eq!(metrics["register_validator_v2_idempotency_keys"], 1);

        relay.shutdown().await;
    }

    #[tokio::test]
    async fn test_returns_404_when_v2_not_supported() {
        let relay =
            start_fake_relay(FakeRelayConfig { supports_register_v2: false, ..Default::default() })
                .await
                .unwrap();
        let client = reqwest::Client::new();

        let response = client
            .post(format!("{}/eth/v2/builder/validators", relay.base_url))
            .json(&RegisterValidatorV2Request {
                registrations: vec![registration("0xabc")],
                context: RegisterValidatorContext {
                    idempotency_key: "idem-2".to_string(),
                    source: None,
                    mode: RegistrationMode::Any,
                },
            })
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        relay.shutdown().await;
    }
}
