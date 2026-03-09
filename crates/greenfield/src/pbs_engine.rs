use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use parking_lot::{Mutex, RwLock};
use thiserror::Error;
use tokio::sync::{Semaphore, mpsc};
use uuid::Uuid;

use crate::{
    config::PbsConfig,
    observability::Observability,
    policies::{
        DefaultRegistrationPolicy, DefaultValidationEngine, RegistrationPolicy, RelayError,
        RetryPolicy, ValidationEngine, ValidationError,
    },
    relay::{RelayRequest, RelayTransport},
    types::{
        BuilderApiVersion, EndpointTag, FanoutMode, HeaderBid, RegisterValidatorContext,
        RegisterValidatorV2Request, RegistrationApiMode, RelayCapability, RelayDescriptor,
        ValidatorRegistration,
    },
};

#[derive(Debug, Error)]
pub enum PbsEngineError {
    #[error("validation failed: {0}")]
    Validation(#[from] ValidationError),
    #[error("serialization failed: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("no relay succeeded")]
    NoRelaySucceeded,
    #[error("registration deadline exhausted")]
    RegistrationDeadline,
    #[error("relay error: {0:?}")]
    Relay(RelayError),
    #[error("internal send channel closed")]
    ChannelClosed,
}

#[derive(Debug)]
struct RegistrationBodies {
    registrations: Arc<Vec<ValidatorRegistration>>,
    context: RegisterValidatorContext,
    v1: Mutex<Option<Bytes>>,
    v2: Mutex<Option<Bytes>>,
}

impl RegistrationBodies {
    fn new(registrations: Vec<ValidatorRegistration>, context: RegisterValidatorContext) -> Self {
        Self {
            registrations: Arc::new(registrations),
            context,
            v1: Mutex::new(None),
            v2: Mutex::new(None),
        }
    }

    fn body_for(&self, version: BuilderApiVersion) -> Result<Bytes, serde_json::Error> {
        match version {
            BuilderApiVersion::V1 => {
                let mut guard = self.v1.lock();
                if let Some(payload) = guard.clone() {
                    return Ok(payload);
                }

                let payload = Bytes::from(serde_json::to_vec(self.registrations.as_ref())?);
                *guard = Some(payload.clone());
                Ok(payload)
            }
            BuilderApiVersion::V2 => {
                let mut guard = self.v2.lock();
                if let Some(payload) = guard.clone() {
                    return Ok(payload);
                }

                let payload = Bytes::from(serde_json::to_vec(&RegisterValidatorV2Request {
                    registrations: self.registrations.as_ref().clone(),
                    context: self.context.clone(),
                })?);
                *guard = Some(payload.clone());
                Ok(payload)
            }
        }
    }
}

#[derive(Clone)]
pub struct PbsEngine {
    config: Arc<PbsConfig>,
    transport: Arc<dyn RelayTransport>,
    metrics: Observability,
    retry_policy: RetryPolicy,
    registration_policy: Arc<dyn RegistrationPolicy>,
    validation_engine: Arc<dyn ValidationEngine>,
    registration_capability_cache: Arc<RwLock<HashMap<String, RelayCapability>>>,
}

impl PbsEngine {
    pub fn new(
        config: PbsConfig,
        transport: Arc<dyn RelayTransport>,
        metrics: Observability,
    ) -> Self {
        let retry_policy =
            RetryPolicy::new(config.register_validator_retry_limit, Duration::from_millis(250));

        Self {
            config: Arc::new(config),
            transport,
            metrics,
            retry_policy,
            registration_policy: Arc::new(DefaultRegistrationPolicy),
            validation_engine: Arc::new(DefaultValidationEngine),
            registration_capability_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn with_validation_engine(mut self, validation_engine: Arc<dyn ValidationEngine>) -> Self {
        self.validation_engine = validation_engine;
        self
    }

    pub fn with_registration_policy(
        mut self,
        registration_policy: Arc<dyn RegistrationPolicy>,
    ) -> Self {
        self.registration_policy = registration_policy;
        self
    }

    pub fn metrics(&self) -> &Observability {
        &self.metrics
    }

    pub fn registration_capability(&self, relay_id: &str) -> RelayCapability {
        self.registration_capability_cache
            .read()
            .get(relay_id)
            .copied()
            .unwrap_or(RelayCapability::Unknown)
    }

    fn set_registration_capability(&self, relay_id: &str, capability: RelayCapability) {
        self.registration_capability_cache.write().insert(relay_id.to_string(), capability);
    }

    pub async fn get_status(&self, relays: Vec<RelayDescriptor>) -> Result<(), PbsEngineError> {
        if !self.config.relay_check {
            return Ok(());
        }
        if relays.is_empty() {
            return Err(PbsEngineError::NoRelaySucceeded);
        }

        let total = relays.len();
        let (tx, mut rx) = mpsc::channel(total);
        for relay in relays {
            let tx = tx.clone();
            let engine = self.clone();
            tokio::spawn(async move {
                let result = engine.send_status_once(&relay).await;
                let _ = tx.send(result).await;
            });
        }
        drop(tx);

        let mut failures = 0usize;
        while let Some(result) = rx.recv().await {
            match result {
                Ok(()) => return Ok(()),
                Err(_) => {
                    failures += 1;
                    if failures == total {
                        break;
                    }
                }
            }
        }

        Err(PbsEngineError::NoRelaySucceeded)
    }

    async fn send_status_once(&self, relay: &RelayDescriptor) -> Result<(), PbsEngineError> {
        let timeout = Duration::from_millis(self.config.timeout_get_header_ms);
        let request = RelayRequest {
            endpoint: EndpointTag::Status,
            method: reqwest::Method::GET,
            version: BuilderApiVersion::V1,
            path_suffix: "/status".to_string(),
            body: None,
            headers: Vec::new(),
        };

        let response = self.transport.send(relay, request, timeout).await;
        self.record_outcome(relay, EndpointTag::Status, &response);

        match response {
            Ok(response) if response.is_success() => Ok(()),
            Ok(response) => Err(PbsEngineError::Relay(RelayError::HttpStatus(response.status))),
            Err(err) => Err(PbsEngineError::Relay(err)),
        }
    }

    pub async fn get_header(
        &self,
        relays: Vec<RelayDescriptor>,
        slot: u64,
        parent_hash: &str,
        pubkey: &str,
        expected_timestamp: u64,
    ) -> Result<Option<HeaderBid>, PbsEngineError> {
        let mut best: Option<HeaderBid> = None;

        for relay in relays {
            let timeout = Duration::from_millis(self.config.timeout_get_header_ms);
            let request = RelayRequest {
                endpoint: EndpointTag::GetHeader,
                method: reqwest::Method::GET,
                version: BuilderApiVersion::V1,
                path_suffix: format!("/header/{slot}/{parent_hash}/{pubkey}"),
                body: None,
                headers: Vec::new(),
            };

            let response = self.transport.send(&relay, request, timeout).await;
            self.record_outcome(&relay, EndpointTag::GetHeader, &response);

            let response = match response {
                Ok(response) if response.is_success() => response,
                Ok(_) => continue,
                Err(_) => continue,
            };

            let bid: HeaderBid = response.json().map_err(PbsEngineError::Relay)?;
            self.validation_engine.validate_header(
                &bid.block_hash,
                &bid.parent_hash,
                parent_hash,
                &bid.tx_root,
                bid.value,
                self.config.min_bid_wei,
                bid.timestamp,
                expected_timestamp,
            )?;

            let should_replace = best.as_ref().map(|old| bid.value > old.value).unwrap_or(true);
            if should_replace {
                best = Some(bid);
            }
        }

        Ok(best)
    }

    pub async fn submit_blinded_block(
        &self,
        relays: Vec<RelayDescriptor>,
        version: BuilderApiVersion,
        body: Bytes,
    ) -> Result<(), PbsEngineError> {
        self.validation_engine.validate_submit_block("0x1")?;

        let mut any_success = false;
        for relay in relays {
            let request = RelayRequest {
                endpoint: EndpointTag::SubmitBlindedBlock,
                method: reqwest::Method::POST,
                version,
                path_suffix: "/blinded_blocks".to_string(),
                body: Some(body.clone()),
                headers: Vec::new(),
            };

            let timeout = Duration::from_millis(self.config.timeout_submit_block_ms);
            let response = self.transport.send(&relay, request, timeout).await;
            self.record_outcome(&relay, EndpointTag::SubmitBlindedBlock, &response);

            if matches!(response, Ok(resp) if resp.is_success()) {
                any_success = true;
            }
        }

        if any_success { Ok(()) } else { Err(PbsEngineError::NoRelaySucceeded) }
    }

    pub async fn register_validators(
        &self,
        relays: Vec<RelayDescriptor>,
        registrations: Vec<ValidatorRegistration>,
        context: Option<RegisterValidatorContext>,
        fanout_mode: FanoutMode,
    ) -> Result<(), PbsEngineError> {
        self.validation_engine.validate_registrations(&registrations)?;
        if relays.is_empty() {
            return Err(PbsEngineError::NoRelaySucceeded);
        }

        let context = context.unwrap_or_else(|| RegisterValidatorContext {
            idempotency_key: Uuid::now_v7().to_string(),
            source: Some("greenfield-pbs".to_string()),
            mode: fanout_mode.as_registration_mode(),
        });

        let bodies = Arc::new(RegistrationBodies::new(registrations, context));
        let semaphore =
            Arc::new(Semaphore::new(self.config.register_validator_max_in_flight as usize));
        let total = relays.len();
        let (tx, mut rx) = mpsc::channel(total);

        for relay in relays {
            let tx = tx.clone();
            let engine = self.clone();
            let bodies = Arc::clone(&bodies);
            let semaphore = Arc::clone(&semaphore);

            tokio::spawn(async move {
                let result = async {
                    let _permit = semaphore
                        .acquire_owned()
                        .await
                        .map_err(|_| PbsEngineError::ChannelClosed)?;
                    engine.send_register_with_retry(&relay, bodies).await
                }
                .await;

                let _ = tx.send(result).await;
            });
        }

        drop(tx);

        match fanout_mode {
            FanoutMode::AnySuccess => {
                let mut failures = 0usize;
                while let Some(result) = rx.recv().await {
                    if result.is_ok() {
                        return Ok(());
                    }

                    failures += 1;
                    if failures == total {
                        break;
                    }
                }
                Err(PbsEngineError::NoRelaySucceeded)
            }
            FanoutMode::AllMustSucceed => {
                let mut received = 0usize;
                let mut all_succeeded = true;

                while let Some(result) = rx.recv().await {
                    received += 1;
                    if result.is_err() {
                        all_succeeded = false;
                    }
                    if received == total {
                        break;
                    }
                }

                if all_succeeded { Ok(()) } else { Err(PbsEngineError::NoRelaySucceeded) }
            }
        }
    }

    async fn send_register_with_retry(
        &self,
        relay: &RelayDescriptor,
        bodies: Arc<RegistrationBodies>,
    ) -> Result<(), PbsEngineError> {
        let deadline = Instant::now() + self.config.register_timeout();
        let mut attempt = 0u32;
        let initial_capability = self.registration_capability(&relay.id);
        let mut version = self.registration_policy.select_version(
            relay.registration_api,
            initial_capability,
            self.config.register_validator_probe_cache,
        );

        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(PbsEngineError::RegistrationDeadline);
            }
            let timeout = deadline.saturating_duration_since(now);

            self.metrics.record_registration_wire_version(&relay.id, version);
            let payload = bodies.body_for(version)?;
            let request = RelayRequest {
                endpoint: EndpointTag::RegisterValidator,
                method: reqwest::Method::POST,
                version,
                path_suffix: "/validators".to_string(),
                body: Some(payload),
                headers: Vec::new(),
            };

            let response = self.transport.send(relay, request, timeout).await;
            self.record_outcome(relay, EndpointTag::RegisterValidator, &response);

            match response {
                Ok(response) if response.is_success() => {
                    if self.config.register_validator_probe_cache
                        && matches!(relay.registration_api, RegistrationApiMode::Auto)
                        && matches!(version, BuilderApiVersion::V2)
                    {
                        self.set_registration_capability(&relay.id, RelayCapability::V2Supported);
                    }
                    return Ok(());
                }
                Ok(response)
                    if response.status == 404
                        && matches!(relay.registration_api, RegistrationApiMode::Auto)
                        && matches!(version, BuilderApiVersion::V2) =>
                {
                    if self.config.register_validator_probe_cache {
                        self.set_registration_capability(&relay.id, RelayCapability::V1Only);
                    }
                    self.metrics.record_registration_fallback(
                        &relay.id,
                        BuilderApiVersion::V2,
                        BuilderApiVersion::V1,
                        "404",
                    );
                    version = BuilderApiVersion::V1;
                    continue;
                }
                Ok(response) => {
                    let err = RelayError::HttpStatus(response.status);
                    if let Some(wait) = self.retry_policy.should_retry(attempt, &err, now, deadline)
                    {
                        attempt = attempt.saturating_add(1);
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Err(PbsEngineError::Relay(err));
                }
                Err(err) => {
                    if let Some(wait) = self.retry_policy.should_retry(attempt, &err, now, deadline)
                    {
                        attempt = attempt.saturating_add(1);
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                    return Err(PbsEngineError::Relay(err));
                }
            }
        }
    }

    fn record_outcome(
        &self,
        relay: &RelayDescriptor,
        endpoint: EndpointTag,
        outcome: &Result<crate::relay::RelayResponse, RelayError>,
    ) {
        let status = match outcome {
            Ok(response) => response.status.to_string(),
            Err(error) => error.status_code_label().to_string(),
        };
        self.metrics.record_status_code(&relay.id, endpoint, &status);
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Arc, time::Duration};

    use async_trait::async_trait;
    use bytes::Bytes;
    use parking_lot::Mutex;

    use super::{PbsEngine, PbsEngineError};
    use crate::{
        config::PlatformConfig,
        observability::Observability,
        policies::RelayError,
        relay::{RelayRequest, RelayResponse, RelayTransport},
        types::{FanoutMode, RegistrationApiMode, RelayDescriptor, ValidatorRegistration},
    };

    #[derive(Debug, Default)]
    struct SequenceTransport {
        responses: Mutex<VecDeque<Result<RelayResponse, RelayError>>>,
    }

    impl SequenceTransport {
        fn with_responses(responses: Vec<Result<RelayResponse, RelayError>>) -> Self {
            Self { responses: Mutex::new(responses.into()) }
        }
    }

    #[async_trait]
    impl RelayTransport for SequenceTransport {
        async fn send(
            &self,
            _relay: &RelayDescriptor,
            _request: RelayRequest,
            _timeout: Duration,
        ) -> Result<RelayResponse, RelayError> {
            self.responses
                .lock()
                .pop_front()
                .unwrap_or(Err(RelayError::Internal("missing mock response".to_string())))
        }
    }

    fn relay() -> RelayDescriptor {
        RelayDescriptor {
            id: "relay-a".to_string(),
            base_url: "http://127.0.0.1:1".to_string(),
            registration_api: RegistrationApiMode::Auto,
        }
    }

    fn registration() -> ValidatorRegistration {
        ValidatorRegistration {
            pubkey: "0xabc".to_string(),
            fee_recipient: "0xfee".to_string(),
            gas_limit: 30_000_000,
            timestamp: 123,
            signature: "0xsig".to_string(),
        }
    }

    #[tokio::test]
    async fn test_register_uses_404_fallback_and_records_metrics() {
        let transport = Arc::new(SequenceTransport::with_responses(vec![
            Ok(RelayResponse { status: 404, body: Bytes::new() }),
            Ok(RelayResponse { status: 200, body: Bytes::new() }),
        ]));

        let metrics = Observability::default();
        let cfg = PlatformConfig::default();
        let engine = PbsEngine::new(cfg.pbs, transport, metrics.clone());

        engine
            .register_validators(vec![relay()], vec![registration()], None, FanoutMode::AnySuccess)
            .await
            .expect("registration should succeed");

        let snapshot = metrics.snapshot();
        assert_eq!(
            snapshot
                .relay_registration_fallback_total
                .get(&(
                    "relay-a".to_string(),
                    "v2".to_string(),
                    "v1".to_string(),
                    "404".to_string()
                ))
                .copied(),
            Some(1)
        );
        assert_eq!(
            snapshot
                .relay_status_code_total
                .get(&("relay-a".to_string(), "register_validator".to_string(), "404".to_string()))
                .copied(),
            Some(1)
        );
        assert_eq!(
            snapshot
                .relay_status_code_total
                .get(&("relay-a".to_string(), "register_validator".to_string(), "200".to_string()))
                .copied(),
            Some(1)
        );
    }

    #[tokio::test]
    async fn test_retry_budget_returns_deadline_error() {
        let transport = Arc::new(SequenceTransport::with_responses(vec![
            Err(RelayError::Timeout),
            Err(RelayError::Timeout),
            Err(RelayError::Timeout),
        ]));

        let mut cfg = PlatformConfig::default();
        cfg.pbs.timeout_register_validator_ms = 1;
        cfg.pbs.register_validator_retry_limit = 10;

        let engine = PbsEngine::new(cfg.pbs, transport, Observability::default());

        let result = engine
            .register_validators(vec![relay()], vec![registration()], None, FanoutMode::AnySuccess)
            .await;

        assert!(matches!(
            result,
            Err(PbsEngineError::RegistrationDeadline)
                | Err(PbsEngineError::Relay(RelayError::Timeout))
                | Err(PbsEngineError::NoRelaySucceeded)
        ));
    }

    #[tokio::test]
    async fn test_metrics_emit_one_outcome_per_attempt() {
        let transport = Arc::new(SequenceTransport::with_responses(vec![
            Err(RelayError::Timeout),
            Err(RelayError::Timeout),
            Ok(RelayResponse { status: 200, body: Bytes::new() }),
        ]));
        let metrics = Observability::default();

        let mut cfg = PlatformConfig::default();
        cfg.pbs.register_validator_retry_limit = 3;
        cfg.pbs.timeout_register_validator_ms = 5_000;

        let engine = PbsEngine::new(cfg.pbs, transport, metrics.clone());
        engine
            .register_validators(vec![relay()], vec![registration()], None, FanoutMode::AnySuccess)
            .await
            .expect("should eventually succeed");

        let snapshot = metrics.snapshot();
        let timeout_count = snapshot
            .relay_status_code_total
            .get(&("relay-a".to_string(), "register_validator".to_string(), "timeout".to_string()))
            .copied()
            .unwrap_or_default();
        let ok_count = snapshot
            .relay_status_code_total
            .get(&("relay-a".to_string(), "register_validator".to_string(), "200".to_string()))
            .copied()
            .unwrap_or_default();

        assert_eq!(timeout_count + ok_count, 3);
    }

    #[tokio::test]
    async fn test_status_returns_failure_when_all_relays_fail() {
        let transport = Arc::new(SequenceTransport::with_responses(vec![
            Err(RelayError::Connect),
            Err(RelayError::Timeout),
        ]));
        let engine =
            PbsEngine::new(PlatformConfig::default().pbs, transport, Observability::default());

        let result = engine.get_status(vec![relay(), relay()]).await;
        assert!(matches!(result, Err(PbsEngineError::NoRelaySucceeded)));
    }

    #[tokio::test]
    async fn test_all_must_succeed_fanout_mode_requires_all_successes() {
        let transport = Arc::new(SequenceTransport::with_responses(vec![
            Ok(RelayResponse { status: 200, body: Bytes::new() }),
            Err(RelayError::Connect),
        ]));
        let engine =
            PbsEngine::new(PlatformConfig::default().pbs, transport, Observability::default());

        let result = engine
            .register_validators(
                vec![relay(), relay()],
                vec![registration()],
                None,
                FanoutMode::AllMustSucceed,
            )
            .await;
        assert!(matches!(result, Err(PbsEngineError::NoRelaySucceeded)));
    }
}
