use std::{net::SocketAddr, sync::Arc};

use axum::{
    Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use serde::Serialize;

use crate::{
    config::{FanoutModeConfig, PlatformConfig},
    pbs_engine::PbsEngine,
    signer_engine::SignerEngine,
    types::{
        BuilderApiVersion, FanoutMode, ProxyKeyRequest, RegisterValidatorV2Request,
        SignatureRequest, ValidatorRegistration,
    },
};

#[derive(Clone)]
pub struct GatewayState {
    pub config: Arc<PlatformConfig>,
    pub pbs: Arc<PbsEngine>,
    pub signer: Arc<SignerEngine>,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

#[derive(Debug, Clone)]
pub struct ServerHandle {
    pub address: SocketAddr,
}

pub fn build_router(state: GatewayState) -> Router {
    let max_body_bytes =
        state.config.pbs.max_request_body_bytes.max(state.config.signer.max_request_body_bytes);

    Router::new()
        .route("/eth/v1/builder/status", get(pbs_status))
        .route("/eth/v1/builder/header/{slot}/{parent_hash}/{pubkey}", get(get_header))
        .route("/eth/v1/builder/validators", post(register_validators_v1))
        .route("/eth/v2/builder/validators", post(register_validators_v2))
        .route("/eth/v1/builder/blinded_blocks", post(submit_blinded_block_v1))
        .route("/eth/v2/builder/blinded_blocks", post(submit_blinded_block_v2))
        .route("/signer/v1/get_pubkeys", get(signer_get_pubkeys))
        .route("/signer/v1/request_signature", post(signer_request_signature))
        .route("/signer/v1/generate_proxy_key", post(signer_generate_proxy_key))
        .route("/status", get(management_status))
        .with_state(state)
        .layer(axum::extract::DefaultBodyLimit::max(max_body_bytes))
}

pub async fn serve(
    state: GatewayState,
    bind_address: &str,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> eyre::Result<ServerHandle> {
    let listener = tokio::net::TcpListener::bind(bind_address).await?;
    let address = listener.local_addr()?;
    let router = build_router(state);
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, router).with_graceful_shutdown(shutdown).await {
            tracing::error!(%err, "gateway server failed");
        }
    });

    Ok(ServerHandle { address })
}

async fn pbs_status(State(state): State<GatewayState>) -> impl IntoResponse {
    let relays = state.config.relays.clone();
    match state.pbs.get_status(relays).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => (StatusCode::BAD_GATEWAY, axum::Json(ErrorBody { error: err.to_string() }))
            .into_response(),
    }
}

async fn get_header(
    State(state): State<GatewayState>,
    Path((slot, parent_hash, pubkey)): Path<(u64, String, String)>,
) -> impl IntoResponse {
    let relays = state.config.relays.clone();
    let expected_timestamp = slot.saturating_mul(12);

    match state.pbs.get_header(relays, slot, &parent_hash, &pubkey, expected_timestamp).await {
        Ok(Some(header)) => (StatusCode::OK, axum::Json(header)).into_response(),
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => (StatusCode::BAD_GATEWAY, axum::Json(ErrorBody { error: err.to_string() }))
            .into_response(),
    }
}

async fn register_validators_v1(
    State(state): State<GatewayState>,
    axum::Json(registrations): axum::Json<Vec<ValidatorRegistration>>,
) -> impl IntoResponse {
    let fanout_mode = fanout_mode(&state.config);
    let relays = state.config.relays.clone();
    match state.pbs.register_validators(relays, registrations, None, fanout_mode).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => (StatusCode::BAD_GATEWAY, axum::Json(ErrorBody { error: err.to_string() }))
            .into_response(),
    }
}

async fn register_validators_v2(
    State(state): State<GatewayState>,
    axum::Json(request): axum::Json<RegisterValidatorV2Request>,
) -> impl IntoResponse {
    let fanout_mode = fanout_mode(&state.config);
    let relays = state.config.relays.clone();
    match state
        .pbs
        .register_validators(relays, request.registrations, Some(request.context), fanout_mode)
        .await
    {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => (StatusCode::BAD_GATEWAY, axum::Json(ErrorBody { error: err.to_string() }))
            .into_response(),
    }
}

async fn submit_blinded_block_v1(
    State(state): State<GatewayState>,
    body: Bytes,
) -> impl IntoResponse {
    submit_blinded_block(state, BuilderApiVersion::V1, body).await
}

async fn submit_blinded_block_v2(
    State(state): State<GatewayState>,
    body: Bytes,
) -> impl IntoResponse {
    submit_blinded_block(state, BuilderApiVersion::V2, body).await
}

async fn submit_blinded_block(
    state: GatewayState,
    version: BuilderApiVersion,
    body: Bytes,
) -> impl IntoResponse {
    let relays = state.config.relays.clone();
    match state.pbs.submit_blinded_block(relays, version, body).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(err) => (StatusCode::BAD_GATEWAY, axum::Json(ErrorBody { error: err.to_string() }))
            .into_response(),
    }
}

async fn signer_get_pubkeys(
    State(state): State<GatewayState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(response) = authorize_signer(&state, &headers) {
        return response;
    }

    let keys = state.signer.get_pubkeys();
    (StatusCode::OK, axum::Json(keys)).into_response()
}

async fn signer_request_signature(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<SignatureRequest>,
) -> impl IntoResponse {
    if let Err(response) = authorize_signer(&state, &headers) {
        return response;
    }

    let signature = state.signer.request_signature(request);
    (StatusCode::OK, axum::Json(signature)).into_response()
}

async fn signer_generate_proxy_key(
    State(state): State<GatewayState>,
    headers: HeaderMap,
    axum::Json(request): axum::Json<ProxyKeyRequest>,
) -> impl IntoResponse {
    if let Err(response) = authorize_signer(&state, &headers) {
        return response;
    }

    let response = state.signer.generate_proxy_key(request);
    (StatusCode::OK, axum::Json(response)).into_response()
}

async fn management_status() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

fn fanout_mode(config: &PlatformConfig) -> FanoutMode {
    match config.pbs.registration_fanout_mode {
        FanoutModeConfig::AnySuccess => FanoutMode::AnySuccess,
        FanoutModeConfig::AllMustSucceed => FanoutMode::AllMustSucceed,
    }
}

fn authorize_signer(
    state: &GatewayState,
    headers: &HeaderMap,
) -> Result<(), axum::response::Response> {
    let expected = match &state.config.signer.api_key {
        Some(api_key) => api_key,
        None => return Ok(()),
    };

    let Some(auth_header) = headers.get("authorization") else {
        return Err((
            StatusCode::UNAUTHORIZED,
            axum::Json(ErrorBody { error: "missing authorization header".to_string() }),
        )
            .into_response());
    };

    let Ok(value) = auth_header.to_str() else {
        return Err((
            StatusCode::UNAUTHORIZED,
            axum::Json(ErrorBody { error: "invalid authorization header".to_string() }),
        )
            .into_response());
    };

    let expected_value = format!("Bearer {expected}");
    if value != expected_value {
        return Err((
            StatusCode::UNAUTHORIZED,
            axum::Json(ErrorBody { error: "invalid api key".to_string() }),
        )
            .into_response());
    }

    Ok(())
}
