use std::time::{Duration, Instant};

use alloy::{primitives::Bytes, rpc::types::beacon::relay::ValidatorRegistration};
use axum::http::{HeaderMap, HeaderValue};
use cb_common::{
    config::RegistrationApi,
    pbs::{
        BuilderApiVersion, HEADER_START_TIME_UNIX_MS, RegisterValidatorContext,
        RegisterValidatorV2Request, RegistrationMode, RelayClient, error::PbsError,
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
    metrics::{RELAY_LATENCY, RELAY_STATUS_CODE},
    state::{BuilderApiState, PbsState},
};

#[derive(Clone)]
struct RegistrationBodies {
    v1: Bytes,
    v2: Bytes,
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

    let total_regs = registrations.len();

    // prepare both v1 and v2 payloads in advance
    let bodies: Box<dyn Iterator<Item = (usize, RegistrationBodies)>> =
        if let Some(batch_size) = state.config.pbs_config.validator_registration_batch_size {
            Box::new(registrations.chunks(batch_size).map(|batch| {
                // SAFETY: serializing typed registration payloads should not fail.
                let v1 = serde_json::to_vec(batch).unwrap();
                let v2 = serde_json::to_vec(&RegisterValidatorV2Request {
                    registrations: batch.to_vec(),
                    context: context.clone(),
                })
                .unwrap();
                (batch.len(), RegistrationBodies { v1: Bytes::from(v1), v2: Bytes::from(v2) })
            }))
        } else {
            let v1 = serde_json::to_vec(&registrations).unwrap();
            let v2 = serde_json::to_vec(&RegisterValidatorV2Request {
                registrations,
                context: context.clone(),
            })
            .unwrap();
            Box::new(std::iter::once((
                total_regs,
                RegistrationBodies { v1: Bytes::from(v1), v2: Bytes::from(v2) },
            )))
        };

    let mut handles = Vec::with_capacity(state.all_relays().len());
    let timeout_ms = state.pbs_config().timeout_register_validator_ms;
    let retry_limit = state.pbs_config().register_validator_retry_limit;

    for (n_regs, body) in bodies {
        for relay in state.all_relays().iter().cloned() {
            handles.push(
                tokio::spawn(
                    send_register_validator_with_timeout(
                        n_regs,
                        body.clone(),
                        relay,
                        send_headers.clone(),
                        timeout_ms,
                        retry_limit,
                    )
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
    n_regs: usize,
    body: RegistrationBodies,
    relay: RelayClient,
    headers: HeaderMap,
    timeout_ms: u64,
    retry_limit: u32,
) -> Result<(), PbsError> {
    let mut remaining_timeout_ms = timeout_ms;
    let mut retry = 0;
    let mut backoff = Duration::from_millis(250);
    let mut api_version = match relay.config.registration_api {
        RegistrationApi::V1 => BuilderApiVersion::V1,
        RegistrationApi::V2 | RegistrationApi::Auto => BuilderApiVersion::V2,
    };

    loop {
        let start_request = Instant::now();
        match send_register_validator(
            n_regs,
            body.clone(),
            &relay,
            headers.clone(),
            remaining_timeout_ms,
            retry,
            api_version,
        )
        .await
        {
            Ok(_) => return Ok(()),

            Err(err)
                if err.is_not_found()
                    && matches!(relay.config.registration_api, RegistrationApi::Auto)
                    && matches!(api_version, BuilderApiVersion::V2) =>
            {
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
                tokio::time::sleep(backoff).await;
                backoff += Duration::from_millis(250);

                remaining_timeout_ms =
                    timeout_ms.saturating_sub(start_request.elapsed().as_millis() as u64);

                if remaining_timeout_ms == 0 {
                    return Err(err);
                }
            }

            Err(err) => return Err(err),
        };
    }
}

async fn send_register_validator(
    n_regs: usize,
    body: RegistrationBodies,
    relay: &RelayClient,
    headers: HeaderMap,
    timeout_ms: u64,
    retry: u32,
    api_version: BuilderApiVersion,
) -> Result<(), PbsError> {
    let url = relay.register_validator_url(api_version)?;
    let body = match api_version {
        BuilderApiVersion::V1 => body.v1,
        BuilderApiVersion::V2 => body.v2,
    };

    let start_request = Instant::now();
    let res = match relay
        .client
        .post(url)
        .timeout(Duration::from_millis(timeout_ms))
        .headers(headers)
        .body(body.0)
        .send()
        .await
    {
        Ok(res) => res,
        Err(err) => {
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
        num_registrations = n_regs,
        "registration successful"
    );

    Ok(())
}
