use alloy::rpc::types::beacon::relay::ValidatorRegistration;
use axum::{Json, extract::State, http::HeaderMap, response::IntoResponse};
use cb_common::{
    pbs::{RegisterValidatorContext, RegisterValidatorV2Request, RegistrationMode},
    utils::get_user_agent,
};
use reqwest::StatusCode;
use tracing::{error, info, trace};
use uuid::Uuid;

use crate::{
    api::BuilderApi,
    constants::REGISTER_VALIDATOR_ENDPOINT_TAG,
    error::PbsClientError,
    metrics::BEACON_NODE_STATUS,
    state::{BuilderApiState, PbsStateGuard},
};

pub async fn handle_register_validator_v1<S: BuilderApiState, A: BuilderApi<S>>(
    State(state): State<PbsStateGuard<S>>,
    req_headers: HeaderMap,
    Json(registrations): Json<Vec<ValidatorRegistration>>,
) -> Result<impl IntoResponse, PbsClientError> {
    let mode = if state.read().config.pbs_config.wait_all_registrations {
        RegistrationMode::All
    } else {
        RegistrationMode::Any
    };

    let ua = get_user_agent(&req_headers);
    let context = RegisterValidatorContext {
        idempotency_key: Uuid::now_v7().to_string(),
        source: (!ua.is_empty()).then_some(ua.clone()),
        mode,
    };

    handle_register_validator::<S, A>(state, req_headers, registrations, context, ua).await
}

pub async fn handle_register_validator_v2<S: BuilderApiState, A: BuilderApi<S>>(
    State(state): State<PbsStateGuard<S>>,
    req_headers: HeaderMap,
    Json(request): Json<RegisterValidatorV2Request>,
) -> Result<impl IntoResponse, PbsClientError> {
    let ua = get_user_agent(&req_headers);
    let RegisterValidatorV2Request { registrations, context } = request;

    handle_register_validator::<S, A>(state, req_headers, registrations, context, ua).await
}

async fn handle_register_validator<S: BuilderApiState, A: BuilderApi<S>>(
    state: PbsStateGuard<S>,
    req_headers: HeaderMap,
    registrations: Vec<ValidatorRegistration>,
    context: RegisterValidatorContext,
    ua: String,
) -> Result<impl IntoResponse, PbsClientError> {
    let state = state.read().clone();

    trace!(?context, ?registrations);

    info!(
        ua,
        num_registrations = registrations.len(),
        idempotency_key = %context.idempotency_key,
        mode = ?context.mode,
        "new request"
    );

    if let Err(err) = A::register_validator(registrations, req_headers, state, Some(context)).await
    {
        error!(%err, "all relays failed registration");

        let err = PbsClientError::NoResponse;
        BEACON_NODE_STATUS
            .with_label_values(&[err.status_code().as_str(), REGISTER_VALIDATOR_ENDPOINT_TAG])
            .inc();
        Err(err)
    } else {
        info!("register validator successful");

        BEACON_NODE_STATUS.with_label_values(&["200", REGISTER_VALIDATOR_ENDPOINT_TAG]).inc();
        Ok(StatusCode::OK)
    }
}
