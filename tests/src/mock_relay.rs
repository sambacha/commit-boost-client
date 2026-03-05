use std::{
    collections::BTreeMap,
    net::SocketAddr,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::{
    primitives::{Address, U256},
    rpc::types::beacon::relay::ValidatorRegistration,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use cb_common::{
    pbs::{
        BUILDER_V1_API_PATH, BUILDER_V2_API_PATH, BlobsBundle, BuilderBid, BuilderBidElectra,
        ExecutionPayloadElectra, ExecutionPayloadHeaderElectra, ExecutionRequests, ForkName,
        GET_HEADER_PATH, GET_STATUS_PATH, GetHeaderParams, GetHeaderResponse, GetPayloadInfo,
        PayloadAndBlobs, REGISTER_VALIDATOR_PATH, RegisterValidatorContext,
        RegisterValidatorV2Request, SUBMIT_BLOCK_PATH, SignedBlindedBeaconBlock, SignedBuilderBid,
        SubmitBlindedBlockResponse,
    },
    signature::sign_builder_root,
    types::{BlsSecretKey, Chain},
    utils::{TestRandomSeed, timestamp_of_slot_start_sec},
};
use cb_pbs::MAX_SIZE_SUBMIT_BLOCK_RESPONSE;
use lh_types::KzgProof;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tracing::{debug, warn};
use tree_hash::TreeHash;

const DEBUG_METRICS_PATH: &str = "/debug/metrics";
const DEBUG_REGISTRATIONS_PATH: &str = "/debug/registrations";
const DEBUG_REGISTRATION_BY_PUBKEY_PATH: &str = "/debug/registrations/{pubkey}";
const MAX_IDEMPOTENCY_KEY_LEN: usize = 256;
const MAX_SOURCE_LEN: usize = 256;

pub async fn start_mock_relay_service(state: Arc<MockRelayState>, port: u16) -> eyre::Result<()> {
    let app = mock_relay_app_router(state);

    let socket = SocketAddr::new("0.0.0.0".parse()?, port);
    let listener = TcpListener::bind(socket).await?;

    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RegistrationWireVersion {
    V1,
    V2,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredValidatorRegistration {
    pub pubkey: String,
    pub registration: ValidatorRegistration,
    pub wire_version: RegistrationWireVersion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MockRelayMetricsSnapshot {
    pub received_get_header: u64,
    pub received_get_status: u64,
    pub received_register_validator: u64,
    pub received_register_validator_v1: u64,
    pub received_register_validator_v2: u64,
    pub received_submit_block: u64,
    pub register_validator_inflight: u64,
    pub register_validator_max_inflight: u64,
    pub register_validator_v2_idempotency_keys: u64,
    pub stored_registrations: u64,
    pub stored_registration_upserts: u64,
    pub registration_validation_failures: u64,
}

pub struct MockRelayState {
    pub chain: Chain,
    pub signer: BlsSecretKey,
    large_body: bool,
    supports_submit_block_v2: bool,
    supports_register_validator_v2: bool,
    validate_registrations: bool,
    use_not_found_for_submit_block: bool,
    use_not_found_for_register_validator_v2: bool,
    register_validator_delay_ms: u64,
    received_get_header: Arc<AtomicU64>,
    received_get_status: Arc<AtomicU64>,
    received_register_validator: Arc<AtomicU64>,
    received_register_validator_v1: Arc<AtomicU64>,
    received_register_validator_v2: Arc<AtomicU64>,
    register_validator_inflight: Arc<AtomicU64>,
    register_validator_max_inflight: Arc<AtomicU64>,
    stored_registration_upserts: Arc<AtomicU64>,
    registration_validation_failures: Arc<AtomicU64>,
    register_validator_v2_idempotency_keys: Arc<RwLock<Vec<String>>>,
    stored_registrations: Arc<RwLock<BTreeMap<String, StoredValidatorRegistration>>>,
    received_submit_block: Arc<AtomicU64>,
    response_override: RwLock<Option<StatusCode>>,
}

impl MockRelayState {
    pub fn received_get_header(&self) -> u64 {
        self.received_get_header.load(Ordering::Relaxed)
    }
    pub fn received_get_status(&self) -> u64 {
        self.received_get_status.load(Ordering::Relaxed)
    }
    pub fn received_register_validator(&self) -> u64 {
        self.received_register_validator.load(Ordering::Relaxed)
    }
    pub fn received_register_validator_v1(&self) -> u64 {
        self.received_register_validator_v1.load(Ordering::Relaxed)
    }
    pub fn received_register_validator_v2(&self) -> u64 {
        self.received_register_validator_v2.load(Ordering::Relaxed)
    }
    pub fn register_validator_v2_idempotency_keys(&self) -> Vec<String> {
        self.register_validator_v2_idempotency_keys.read().unwrap().clone()
    }
    pub fn stored_registrations(&self) -> Vec<StoredValidatorRegistration> {
        self.stored_registrations.read().unwrap().values().cloned().collect()
    }
    pub fn stored_registration(&self, pubkey: &str) -> Option<StoredValidatorRegistration> {
        self.stored_registrations.read().unwrap().get(&pubkey.to_ascii_lowercase()).cloned()
    }
    pub fn registration_validation_failures(&self) -> u64 {
        self.registration_validation_failures.load(Ordering::Relaxed)
    }
    pub fn metrics_snapshot(&self) -> MockRelayMetricsSnapshot {
        MockRelayMetricsSnapshot {
            received_get_header: self.received_get_header(),
            received_get_status: self.received_get_status(),
            received_register_validator: self.received_register_validator(),
            received_register_validator_v1: self.received_register_validator_v1(),
            received_register_validator_v2: self.received_register_validator_v2(),
            received_submit_block: self.received_submit_block(),
            register_validator_inflight: self.register_validator_inflight.load(Ordering::Relaxed),
            register_validator_max_inflight: self.max_register_validator_inflight(),
            register_validator_v2_idempotency_keys: self
                .register_validator_v2_idempotency_keys
                .read()
                .unwrap()
                .len() as u64,
            stored_registrations: self.stored_registrations.read().unwrap().len() as u64,
            stored_registration_upserts: self.stored_registration_upserts.load(Ordering::Relaxed),
            registration_validation_failures: self.registration_validation_failures(),
        }
    }
    pub fn max_register_validator_inflight(&self) -> u64 {
        self.register_validator_max_inflight.load(Ordering::Relaxed)
    }
    pub fn received_submit_block(&self) -> u64 {
        self.received_submit_block.load(Ordering::Relaxed)
    }
    pub fn large_body(&self) -> bool {
        self.large_body
    }
    pub fn supports_submit_block_v2(&self) -> bool {
        self.supports_submit_block_v2
    }
    pub fn supports_register_validator_v2(&self) -> bool {
        self.supports_register_validator_v2
    }
    pub fn validate_registrations(&self) -> bool {
        self.validate_registrations
    }
    pub fn use_not_found_for_submit_block(&self) -> bool {
        self.use_not_found_for_submit_block
    }
    pub fn use_not_found_for_register_validator_v2(&self) -> bool {
        self.use_not_found_for_register_validator_v2
    }
    pub fn register_validator_delay_ms(&self) -> u64 {
        self.register_validator_delay_ms
    }
    pub fn set_response_override(&self, status: StatusCode) {
        *self.response_override.write().unwrap() = Some(status);
    }
}

impl MockRelayState {
    pub fn new(chain: Chain, signer: BlsSecretKey) -> Self {
        Self {
            chain,
            signer,
            large_body: false,
            supports_submit_block_v2: true,
            supports_register_validator_v2: true,
            validate_registrations: true,
            use_not_found_for_submit_block: false,
            use_not_found_for_register_validator_v2: false,
            register_validator_delay_ms: 0,
            received_get_header: Default::default(),
            received_get_status: Default::default(),
            received_register_validator: Default::default(),
            received_register_validator_v1: Default::default(),
            received_register_validator_v2: Default::default(),
            register_validator_inflight: Default::default(),
            register_validator_max_inflight: Default::default(),
            stored_registration_upserts: Default::default(),
            registration_validation_failures: Default::default(),
            register_validator_v2_idempotency_keys: Default::default(),
            stored_registrations: Default::default(),
            received_submit_block: Default::default(),
            response_override: RwLock::new(None),
        }
    }

    pub fn with_large_body(self) -> Self {
        Self { large_body: true, ..self }
    }

    pub fn with_no_submit_block_v2(self) -> Self {
        Self { supports_submit_block_v2: false, ..self }
    }

    pub fn with_no_register_validator_v2(self) -> Self {
        Self { supports_register_validator_v2: false, ..self }
    }

    pub fn with_registration_validation_disabled(self) -> Self {
        Self { validate_registrations: false, ..self }
    }

    pub fn with_not_found_for_submit_block(self) -> Self {
        Self { use_not_found_for_submit_block: true, ..self }
    }

    pub fn with_not_found_for_register_validator_v2(self) -> Self {
        Self { use_not_found_for_register_validator_v2: true, ..self }
    }

    pub fn with_register_validator_delay_ms(self, delay_ms: u64) -> Self {
        Self { register_validator_delay_ms: delay_ms, ..self }
    }
}

pub fn mock_relay_app_router(state: Arc<MockRelayState>) -> Router {
    let v1_builder_routes = Router::new()
        .route(GET_HEADER_PATH, get(handle_get_header))
        .route(GET_STATUS_PATH, get(handle_get_status))
        .route(REGISTER_VALIDATOR_PATH, post(handle_register_validator_v1))
        .route(SUBMIT_BLOCK_PATH, post(handle_submit_block_v1));

    let mut v2_builder_routes = Router::new();
    if state.supports_submit_block_v2 {
        v2_builder_routes =
            v2_builder_routes.route(SUBMIT_BLOCK_PATH, post(handle_submit_block_v2));
    }
    if state.supports_register_validator_v2 {
        v2_builder_routes =
            v2_builder_routes.route(REGISTER_VALIDATOR_PATH, post(handle_register_validator_v2));
    }

    let builder_router_v1 = Router::new().nest(BUILDER_V1_API_PATH, v1_builder_routes);
    let builder_router_v2 = Router::new().nest(BUILDER_V2_API_PATH, v2_builder_routes);
    Router::new()
        .merge(builder_router_v1)
        .merge(builder_router_v2)
        .route(DEBUG_METRICS_PATH, get(handle_debug_metrics))
        .route(DEBUG_REGISTRATIONS_PATH, get(handle_debug_registrations))
        .route(DEBUG_REGISTRATION_BY_PUBKEY_PATH, get(handle_debug_registration_by_pubkey))
        .with_state(state)
}

async fn handle_get_header(
    State(state): State<Arc<MockRelayState>>,
    Path(GetHeaderParams { parent_hash, .. }): Path<GetHeaderParams>,
) -> Response {
    state.received_get_header.fetch_add(1, Ordering::Relaxed);

    let mut header = ExecutionPayloadHeaderElectra {
        parent_hash: parent_hash.into(),
        block_hash: Default::default(),
        timestamp: timestamp_of_slot_start_sec(0, state.chain),
        ..ExecutionPayloadHeaderElectra::test_random()
    };

    header.block_hash.0[0] = 1;

    let message = BuilderBid::Electra(BuilderBidElectra {
        header,
        blob_kzg_commitments: Default::default(),
        execution_requests: ExecutionRequests::default(),
        value: U256::from(10),
        pubkey: state.signer.public_key().into(),
    });

    let object_root = message.tree_hash_root();
    let signature = sign_builder_root(state.chain, &state.signer, object_root);
    let response = SignedBuilderBid { message, signature };

    let response = GetHeaderResponse {
        version: ForkName::Electra,
        data: response,
        metadata: Default::default(),
    };
    (StatusCode::OK, Json(response)).into_response()
}

async fn handle_get_status(State(state): State<Arc<MockRelayState>>) -> impl IntoResponse {
    state.received_get_status.fetch_add(1, Ordering::Relaxed);
    StatusCode::OK
}

async fn handle_register_validator_v1(
    State(state): State<Arc<MockRelayState>>,
    Json(validators): Json<Vec<ValidatorRegistration>>,
) -> impl IntoResponse {
    let inflight = state.register_validator_inflight.fetch_add(1, Ordering::Relaxed) + 1;
    state.register_validator_max_inflight.fetch_max(inflight, Ordering::Relaxed);
    state.received_register_validator.fetch_add(1, Ordering::Relaxed);
    state.received_register_validator_v1.fetch_add(1, Ordering::Relaxed);
    debug!("Received {} registrations", validators.len());
    if state.register_validator_delay_ms() > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(state.register_validator_delay_ms()))
            .await;
    }

    let validation_error = if state.validate_registrations() {
        validate_registration_batch(&validators).err()
    } else {
        None
    };
    if let Some(error_msg) = validation_error.as_ref() {
        state.registration_validation_failures.fetch_add(1, Ordering::Relaxed);
        warn!("Rejecting v1 validator registration batch: {error_msg}");
    }

    let status = if validation_error.is_some() {
        StatusCode::BAD_REQUEST
    } else if let Some(status) = state.response_override.read().unwrap().as_ref() {
        *status
    } else {
        StatusCode::OK
    };

    if status.is_success() {
        persist_registration_batch(&state, &validators, RegistrationWireVersion::V1, None);
    }

    state.register_validator_inflight.fetch_sub(1, Ordering::Relaxed);

    status.into_response()
}

async fn handle_register_validator_v2(
    State(state): State<Arc<MockRelayState>>,
    Json(request): Json<RegisterValidatorV2Request>,
) -> impl IntoResponse {
    let inflight = state.register_validator_inflight.fetch_add(1, Ordering::Relaxed) + 1;
    state.register_validator_max_inflight.fetch_max(inflight, Ordering::Relaxed);
    state.received_register_validator.fetch_add(1, Ordering::Relaxed);
    state.received_register_validator_v2.fetch_add(1, Ordering::Relaxed);
    state
        .register_validator_v2_idempotency_keys
        .write()
        .unwrap()
        .push(request.context.idempotency_key.clone());
    debug!("Received {} registrations (v2)", request.registrations.len());
    if state.register_validator_delay_ms() > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(state.register_validator_delay_ms()))
            .await;
    }

    let validation_error =
        if state.validate_registrations() && !state.use_not_found_for_register_validator_v2() {
            validate_registration_batch(&request.registrations)
                .and_then(|_| validate_v2_context(&request.context))
                .err()
        } else {
            None
        };
    if let Some(error_msg) = validation_error.as_ref() {
        state.registration_validation_failures.fetch_add(1, Ordering::Relaxed);
        warn!("Rejecting v2 validator registration batch: {error_msg}");
    }

    let status = if state.use_not_found_for_register_validator_v2() {
        StatusCode::NOT_FOUND
    } else if validation_error.is_some() {
        StatusCode::BAD_REQUEST
    } else if let Some(status) = state.response_override.read().unwrap().as_ref() {
        *status
    } else {
        StatusCode::OK
    };

    if status.is_success() {
        persist_registration_batch(
            &state,
            &request.registrations,
            RegistrationWireVersion::V2,
            Some(&request.context),
        );
    }

    state.register_validator_inflight.fetch_sub(1, Ordering::Relaxed);

    status.into_response()
}

async fn handle_submit_block_v1(
    State(state): State<Arc<MockRelayState>>,
    Json(submit_block): Json<SignedBlindedBeaconBlock>,
) -> Response {
    if state.use_not_found_for_submit_block() {
        return StatusCode::NOT_FOUND.into_response();
    }
    state.received_submit_block.fetch_add(1, Ordering::Relaxed);
    if state.large_body() {
        (StatusCode::OK, Json(vec![1u8; 1 + MAX_SIZE_SUBMIT_BLOCK_RESPONSE])).into_response()
    } else {
        let mut execution_payload = ExecutionPayloadElectra::test_random();
        execution_payload.block_hash = submit_block.block_hash().into();

        let mut blobs_bundle = BlobsBundle::default();

        blobs_bundle.blobs.push(Default::default()).unwrap();
        blobs_bundle.commitments =
            submit_block.as_electra().unwrap().message.body.blob_kzg_commitments.clone();
        blobs_bundle.proofs.push(KzgProof([0; 48])).unwrap();

        let response =
            PayloadAndBlobs { execution_payload: execution_payload.into(), blobs_bundle };

        let response = SubmitBlindedBlockResponse {
            version: ForkName::Electra,
            metadata: Default::default(),
            data: response,
        };

        (StatusCode::OK, Json(response)).into_response()
    }
}
async fn handle_submit_block_v2(State(state): State<Arc<MockRelayState>>) -> Response {
    if state.use_not_found_for_submit_block() {
        return StatusCode::NOT_FOUND.into_response();
    }
    state.received_submit_block.fetch_add(1, Ordering::Relaxed);
    (StatusCode::ACCEPTED, "").into_response()
}

async fn handle_debug_metrics(State(state): State<Arc<MockRelayState>>) -> impl IntoResponse {
    (StatusCode::OK, Json(state.metrics_snapshot()))
}

async fn handle_debug_registrations(State(state): State<Arc<MockRelayState>>) -> impl IntoResponse {
    (StatusCode::OK, Json(state.stored_registrations()))
}

async fn handle_debug_registration_by_pubkey(
    State(state): State<Arc<MockRelayState>>,
    Path(pubkey): Path<String>,
) -> impl IntoResponse {
    let key = pubkey.to_ascii_lowercase();
    if let Some(registration) = state.stored_registration(&key) {
        (StatusCode::OK, Json(registration)).into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

fn persist_registration_batch(
    state: &MockRelayState,
    registrations: &[ValidatorRegistration],
    wire_version: RegistrationWireVersion,
    context: Option<&RegisterValidatorContext>,
) {
    let mut stored = state.stored_registrations.write().unwrap();
    let now = unix_now_ms();
    for registration in registrations {
        let key = registration.message.pubkey.to_string().to_ascii_lowercase();
        let record = StoredValidatorRegistration {
            pubkey: key.clone(),
            registration: registration.clone(),
            wire_version,
            idempotency_key: context.map(|ctx| ctx.idempotency_key.clone()),
            source: context.and_then(|ctx| ctx.source.clone()),
            mode: context.map(|ctx| match ctx.mode {
                cb_common::pbs::RegistrationMode::Any => "any".to_string(),
                cb_common::pbs::RegistrationMode::All => "all".to_string(),
            }),
            updated_at_unix_ms: now,
        };
        stored.insert(key, record);
        state.stored_registration_upserts.fetch_add(1, Ordering::Relaxed);
    }
}

fn validate_registration_batch(registrations: &[ValidatorRegistration]) -> Result<(), String> {
    if registrations.is_empty() {
        return Err("validator registration batch is empty".to_string());
    }

    for (index, registration) in registrations.iter().enumerate() {
        if registration.message.fee_recipient == Address::ZERO {
            return Err(format!("validator registration at index {index} has zero fee_recipient"));
        }
        if registration.message.gas_limit == 0 {
            return Err(format!("validator registration at index {index} has zero gas_limit"));
        }
        if registration.message.timestamp == 0 {
            return Err(format!("validator registration at index {index} has zero timestamp"));
        }
        if registration.message.pubkey.is_zero() {
            return Err(format!("validator registration at index {index} has zero pubkey"));
        }
        if registration.signature.is_zero() {
            return Err(format!("validator registration at index {index} has zero signature"));
        }
    }

    Ok(())
}

fn validate_v2_context(context: &RegisterValidatorContext) -> Result<(), String> {
    let idempotency_key = context.idempotency_key.trim();
    if idempotency_key.is_empty() {
        return Err("v2 context idempotency_key is empty".to_string());
    }
    if idempotency_key.len() > MAX_IDEMPOTENCY_KEY_LEN {
        return Err(format!(
            "v2 context idempotency_key exceeds {MAX_IDEMPOTENCY_KEY_LEN} characters"
        ));
    }

    if let Some(source) = context.source.as_deref() {
        if source.trim().is_empty() {
            return Err("v2 context source is empty".to_string());
        }
        if source.len() > MAX_SOURCE_LEN {
            return Err(format!("v2 context source exceeds {MAX_SOURCE_LEN} characters"));
        }
    }

    Ok(())
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock should be after unix epoch")
        .as_millis() as u64
}
