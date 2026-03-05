use std::{
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use alloy::{primitives::Bytes, rpc::types::beacon::relay::ValidatorRegistration};
use axum::http::{HeaderMap, HeaderValue};
use cb_common::{
    config::RegistrationApi,
    pbs::{
        BuilderApiVersion, HEADER_START_TIME_UNIX_MS, RegisterValidatorContext,
        RegisterValidatorV2Request, RegistrationMode, RelayClient, RelayRegistrationCapability,
        error::PbsError,
    },
    utils::{get_user_agent_with_version, read_chunked_body_with_max, utcnow_ms},
};
use eyre::bail;
use futures::{
    FutureExt,
    future::{join_all, select_ok},
};
use reqwest::header::{CONTENT_TYPE, USER_AGENT};
use tracing::{Instrument, debug, error, warn};
use uuid::Uuid;

use crate::{
    constants::{MAX_SIZE_DEFAULT, REGISTER_VALIDATOR_ENDPOINT_TAG, TIMEOUT_ERROR_CODE_STR},
    metrics::{
        RELAY_LATENCY, RELAY_REGISTRATION_FALLBACK, RELAY_REGISTRATION_INFLIGHT,
        RELAY_REGISTRATION_WIRE_VERSION, RELAY_STATUS_CODE,
    },
    state::{BuilderApiState, PbsState},
};

const FALLBACK_REASON_NOT_FOUND: &str = "404";

#[derive(Debug)]
struct RegistrationBatchPayload {
    registrations: Arc<Vec<ValidatorRegistration>>,
    context: RegisterValidatorContext,
    v1: OnceLock<Bytes>,
    v2: OnceLock<Bytes>,
}

impl RegistrationBatchPayload {
    fn new(registrations: Vec<ValidatorRegistration>, context: RegisterValidatorContext) -> Self {
        Self {
            registrations: Arc::new(registrations),
            context,
            v1: OnceLock::new(),
            v2: OnceLock::new(),
        }
    }

    fn n_regs(&self) -> usize {
        self.registrations.len()
    }

    fn body_for(&self, api_version: BuilderApiVersion) -> Bytes {
        match api_version {
            BuilderApiVersion::V1 => self
                .v1
                .get_or_init(|| {
                    // SAFETY: serializing typed registration payloads should not fail.
                    Bytes::from(serde_json::to_vec(self.registrations.as_ref()).unwrap())
                })
                .clone(),
            BuilderApiVersion::V2 => self
                .v2
                .get_or_init(|| {
                    // SAFETY: serializing typed registration payloads should not fail.
                    Bytes::from(
                        serde_json::to_vec(&RegisterValidatorV2Request {
                            registrations: self.registrations.as_ref().to_vec(),
                            context: self.context.clone(),
                        })
                        .unwrap(),
                    )
                })
                .clone(),
        }
    }
}

/// Implements https://ethereum.github.io/builder-specs/#/Builder/registerValidator
/// Returns 200 if at least one relay returns 200, else 503
pub async fn register_validator<S: BuilderApiState>(
    registrations: Vec<ValidatorRegistration>,
    req_headers: HeaderMap,
    state: PbsState<S>,
    context: Option<RegisterValidatorContext>,
) -> eyre::Result<()> {
    // prepare headers
    let mut send_headers = HeaderMap::new();
    send_headers
        .insert(HEADER_START_TIME_UNIX_MS, HeaderValue::from_str(&utcnow_ms().to_string())?);
    send_headers.insert(USER_AGENT, get_user_agent_with_version(&req_headers)?);
    send_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

    let context = context.unwrap_or_else(|| RegisterValidatorContext {
        idempotency_key: Uuid::now_v7().to_string(),
        source: None,
        mode: if state.pbs_config().wait_all_registrations {
            RegistrationMode::All
        } else {
            RegistrationMode::Any
        },
    });

    let batches: Vec<_> = if let Some(batch_size) =
        state.config.pbs_config.validator_registration_batch_size
    {
        registrations
            .chunks(batch_size)
            .map(|batch| Arc::new(RegistrationBatchPayload::new(batch.to_vec(), context.clone())))
            .collect()
    } else {
        vec![Arc::new(RegistrationBatchPayload::new(registrations, context.clone()))]
    };

    let mut handles = Vec::with_capacity(state.all_relays().len() * batches.len());
    let timeout_ms = state.pbs_config().timeout_register_validator_ms;
    let retry_limit = state.pbs_config().register_validator_retry_limit;
    let probe_cache_enabled = state.pbs_config().register_validator_probe_cache;
    let semaphore = Arc::new(tokio::sync::Semaphore::new(
        state.pbs_config().register_validator_max_in_flight as usize,
    ));

    for batch in batches {
        for relay in state.all_relays().iter().cloned() {
            let semaphore = Arc::clone(&semaphore);
            let batch = Arc::clone(&batch);
            let send_headers = send_headers.clone();
            handles.push(
                tokio::spawn(
                    async move {
                        let _permit = semaphore
                            .acquire_owned()
                            .await
                            .expect("register validator semaphore should not close");
                        send_register_validator_with_timeout(
                            batch,
                            relay,
                            send_headers,
                            timeout_ms,
                            retry_limit,
                            probe_cache_enabled,
                        )
                        .await
                    }
                    .in_current_span(),
                )
                .map(|join_result| match join_result {
                    Ok(res) => res,
                    Err(err) => Err(PbsError::TokioJoinError(err)),
                }),
            );
        }
    }

    if state.pbs_config().wait_all_registrations {
        // wait for all relays registrations to complete
        let results = join_all(handles).await;
        if results.into_iter().any(|res| res.is_ok()) {
            Ok(())
        } else {
            bail!("No relay passed register_validator successfully")
        }
    } else {
        // return once first completes, others proceed in background
        let result = select_ok(handles).await;
        match result {
            Ok(_) => Ok(()),
            Err(_) => bail!("No relay passed register_validator successfully"),
        }
    }
}

/// Register validator to relay, retry connection errors until the
/// given timeout has passed
async fn send_register_validator_with_timeout(
    batch: Arc<RegistrationBatchPayload>,
    relay: RelayClient,
    headers: HeaderMap,
    timeout_ms: u64,
    retry_limit: u32,
    probe_cache_enabled: bool,
) -> Result<(), PbsError> {
    let deadline = Instant::now().checked_add(Duration::from_millis(timeout_ms));
    let mut retry = 0;
    let mut backoff = Duration::from_millis(250);
    let mut api_version = initial_registration_api_version(&relay, probe_cache_enabled);

    loop {
        let remaining_timeout = match deadline {
            Some(deadline) => deadline.saturating_duration_since(Instant::now()),
            None => Duration::from_millis(timeout_ms),
        };
        if remaining_timeout.is_zero() {
            return Err(PbsError::RelayResponse {
                error_msg: "register validator deadline exhausted before request".to_string(),
                code: reqwest::StatusCode::REQUEST_TIMEOUT.as_u16(),
            });
        }

        match send_register_validator(
            Arc::clone(&batch),
            &relay,
            headers.clone(),
            remaining_timeout,
            retry,
            api_version,
        )
        .await
        {
            Ok(_) => {
                if probe_cache_enabled
                    && matches!(relay.config.registration_api, RegistrationApi::Auto)
                    && matches!(api_version, BuilderApiVersion::V2)
                {
                    relay.set_registration_capability(RelayRegistrationCapability::V2Supported);
                }
                return Ok(());
            }

            Err(err)
                if err.is_not_found()
                    && matches!(relay.config.registration_api, RegistrationApi::Auto)
                    && matches!(api_version, BuilderApiVersion::V2) =>
            {
                if probe_cache_enabled {
                    relay.set_registration_capability(RelayRegistrationCapability::V1Only);
                }
                RELAY_REGISTRATION_FALLBACK
                    .with_label_values(&[
                        relay.id.as_ref().as_str(),
                        "v2",
                        "v1",
                        FALLBACK_REASON_NOT_FOUND,
                    ])
                    .inc();
                warn!(
                    relay_id = relay.id.as_ref(),
                    "relay does not support validator registration v2 endpoint, retrying with v1"
                );
                api_version = BuilderApiVersion::V1;
            }

            Err(err) if err.should_retry() => {
                retry += 1;
                if retry >= retry_limit {
                    error!(
                        relay_id = relay.id.as_str(),
                        retry,
                        api_version = %api_version,
                        "reached retry limit for validator registration"
                    );
                    return Err(err);
                }
                let sleep_time = match deadline {
                    Some(deadline) => {
                        deadline.saturating_duration_since(Instant::now()).min(backoff)
                    }
                    None => backoff,
                };
                if sleep_time.is_zero() {
                    return Err(err);
                }
                tokio::time::sleep(sleep_time).await;
                backoff += Duration::from_millis(250);
            }

            Err(err) => return Err(err),
        };
    }
}

async fn send_register_validator(
    batch: Arc<RegistrationBatchPayload>,
    relay: &RelayClient,
    headers: HeaderMap,
    timeout: Duration,
    retry: u32,
    api_version: BuilderApiVersion,
) -> Result<(), PbsError> {
    let url = relay.register_validator_url(api_version)?;
    let body = batch.body_for(api_version);
    let api_version_label = api_version.to_string();
    RELAY_REGISTRATION_WIRE_VERSION
        .with_label_values(&[relay.id.as_ref().as_str(), api_version_label.as_str()])
        .inc();
    RELAY_REGISTRATION_INFLIGHT.with_label_values(&[relay.id.as_ref()]).inc();

    let start_request = Instant::now();
    let res =
        match relay.client.post(url).timeout(timeout).headers(headers).body(body.0).send().await {
            Ok(res) => res,
            Err(err) => {
                RELAY_REGISTRATION_INFLIGHT.with_label_values(&[relay.id.as_ref()]).dec();
                RELAY_STATUS_CODE
                    .with_label_values(&[
                        TIMEOUT_ERROR_CODE_STR,
                        REGISTER_VALIDATOR_ENDPOINT_TAG,
                        &relay.id,
                    ])
                    .inc();
                return Err(err.into());
            }
        };
    RELAY_REGISTRATION_INFLIGHT.with_label_values(&[relay.id.as_ref()]).dec();
    let request_latency = start_request.elapsed();
    RELAY_LATENCY
        .with_label_values(&[REGISTER_VALIDATOR_ENDPOINT_TAG, &relay.id])
        .observe(request_latency.as_secs_f64());

    let code = res.status();
    RELAY_STATUS_CODE
        .with_label_values(&[code.as_str(), REGISTER_VALIDATOR_ENDPOINT_TAG, &relay.id])
        .inc();

    if !code.is_success() {
        let response_bytes = read_chunked_body_with_max(res, MAX_SIZE_DEFAULT).await?;
        let err = PbsError::RelayResponse {
            error_msg: String::from_utf8_lossy(&response_bytes).into_owned(),
            code: code.as_u16(),
        };

        // error here since we check if any success above
        error!(
            relay_id = relay.id.as_ref(),
            retry,
            api_version = %api_version,
            %err,
            "failed registration"
        );
        return Err(err);
    };

    debug!(
        relay_id = relay.id.as_ref(),
        retry,
        api_version = %api_version,
        ?code,
        latency = ?request_latency,
        num_registrations = batch.n_regs(),
        "registration successful"
    );

    Ok(())
}

fn initial_registration_api_version(
    relay: &RelayClient,
    probe_cache_enabled: bool,
) -> BuilderApiVersion {
    match relay.config.registration_api {
        RegistrationApi::V1 => BuilderApiVersion::V1,
        RegistrationApi::V2 => BuilderApiVersion::V2,
        RegistrationApi::Auto => {
            if !probe_cache_enabled {
                return BuilderApiVersion::V2;
            }

            match relay.registration_capability() {
                RelayRegistrationCapability::V1Only => BuilderApiVersion::V1,
                RelayRegistrationCapability::Unknown | RelayRegistrationCapability::V2Supported => {
                    BuilderApiVersion::V2
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy::rpc::types::beacon::relay::ValidatorRegistration;

    use super::{
        BuilderApiVersion, RegisterValidatorContext, RegistrationBatchPayload, RegistrationMode,
    };

    #[test]
    fn test_registration_batch_payload_lazy_serialization() {
        let payload = RegistrationBatchPayload::new(
            vec![test_registration()],
            RegisterValidatorContext {
                idempotency_key: "test-id".to_string(),
                source: Some("test".to_string()),
                mode: RegistrationMode::Any,
            },
        );

        assert!(payload.v1.get().is_none());
        assert!(payload.v2.get().is_none());

        let _ = payload.body_for(BuilderApiVersion::V2);
        assert!(payload.v1.get().is_none());
        assert!(payload.v2.get().is_some());

        let _ = payload.body_for(BuilderApiVersion::V1);
        assert!(payload.v1.get().is_some());
        assert!(payload.v2.get().is_some());
    }

    fn test_registration() -> ValidatorRegistration {
        serde_json::from_str(
            r#"{
            "message": {
                "fee_recipient": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "gas_limit": "100000",
                "timestamp": "1000000",
                "pubkey": "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            },
            "signature": "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
        }"#,
        )
        .unwrap()
    }
}
