use std::{sync::Arc, time::Duration};

use alloy::rpc::types::beacon::relay::ValidatorRegistration;
use cb_common::{
    pbs::{RegisterValidatorContext, RegistrationMode},
    signer::random_secret,
    types::Chain,
};
use cb_tests::{
    mock_relay::{MockRelayState, start_mock_relay_service},
    mock_validator::{MockValidator, ValidatorRegistrationApiMode, ValidatorRegistrationRequest},
    utils::setup_test_env,
};
use eyre::Result;
use reqwest::StatusCode;
use uuid::Uuid;

const RELAY_STARTUP_DELAY: Duration = Duration::from_millis(50);

#[tokio::test]
async fn test_auto_mode_prefers_v2_and_caches_success() -> Result<()> {
    setup_test_env();
    let chain = Chain::Holesky;
    let relay_port = 6201;

    let relay_state = Arc::new(MockRelayState::new(chain, random_secret()));
    start_relay(relay_state.clone(), relay_port).await;

    let validator = MockValidator::new(relay_port)?;
    for _ in 0..2 {
        let response = validator
            .do_register_validators(ValidatorRegistrationRequest {
                registrations: vec![test_registration()?],
                api_mode: ValidatorRegistrationApiMode::Auto,
                context: None,
            })
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
    }

    assert_eq!(relay_state.received_register_validator_v2(), 2);
    assert_eq!(relay_state.received_register_validator_v1(), 0);

    Ok(())
}

#[tokio::test]
async fn test_auto_mode_404_fallback_then_cache_v1_only() -> Result<()> {
    setup_test_env();
    let chain = Chain::Holesky;
    let relay_port = 6202;

    let relay_state = Arc::new(
        MockRelayState::new(chain, random_secret()).with_not_found_for_register_validator_v2(),
    );
    start_relay(relay_state.clone(), relay_port).await;

    let validator = MockValidator::new(relay_port)?;

    let response = validator
        .do_register_validators(ValidatorRegistrationRequest {
            registrations: vec![test_registration()?],
            api_mode: ValidatorRegistrationApiMode::Auto,
            context: None,
        })
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(relay_state.received_register_validator_v2(), 1);
    assert_eq!(relay_state.received_register_validator_v1(), 1);

    let response = validator
        .do_register_validators(ValidatorRegistrationRequest {
            registrations: vec![test_registration()?],
            api_mode: ValidatorRegistrationApiMode::Auto,
            context: None,
        })
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(relay_state.received_register_validator_v2(), 1);
    assert_eq!(relay_state.received_register_validator_v1(), 2);

    Ok(())
}

#[tokio::test]
async fn test_auto_mode_no_fallback_on_non_404() -> Result<()> {
    setup_test_env();
    let chain = Chain::Holesky;
    let relay_port = 6203;

    let relay_state = Arc::new(MockRelayState::new(chain, random_secret()));
    relay_state.set_response_override(StatusCode::INTERNAL_SERVER_ERROR);
    start_relay(relay_state.clone(), relay_port).await;

    let validator = MockValidator::new(relay_port)?;
    let response = validator
        .do_register_validators(ValidatorRegistrationRequest {
            registrations: vec![test_registration()?],
            api_mode: ValidatorRegistrationApiMode::Auto,
            context: None,
        })
        .await?;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(relay_state.received_register_validator_v2(), 1);
    assert_eq!(relay_state.received_register_validator_v1(), 0);

    Ok(())
}

#[tokio::test]
async fn test_v2_mode_no_fallback() -> Result<()> {
    setup_test_env();
    let chain = Chain::Holesky;
    let relay_port = 6204;

    let relay_state = Arc::new(
        MockRelayState::new(chain, random_secret()).with_not_found_for_register_validator_v2(),
    );
    start_relay(relay_state.clone(), relay_port).await;

    let validator = MockValidator::new(relay_port)?;
    let response = validator
        .do_register_validators(ValidatorRegistrationRequest {
            registrations: vec![test_registration()?],
            api_mode: ValidatorRegistrationApiMode::V2,
            context: None,
        })
        .await?;

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(relay_state.received_register_validator_v2(), 1);
    assert_eq!(relay_state.received_register_validator_v1(), 0);

    Ok(())
}

#[tokio::test]
async fn test_v1_mode_always_v1() -> Result<()> {
    setup_test_env();
    let chain = Chain::Holesky;
    let relay_port = 6205;

    let relay_state = Arc::new(MockRelayState::new(chain, random_secret()));
    start_relay(relay_state.clone(), relay_port).await;

    let validator = MockValidator::new(relay_port)?;

    let auto_response = validator
        .do_register_validators(ValidatorRegistrationRequest {
            registrations: vec![test_registration()?],
            api_mode: ValidatorRegistrationApiMode::Auto,
            context: None,
        })
        .await?;
    assert_eq!(auto_response.status(), StatusCode::OK);
    assert_eq!(relay_state.received_register_validator_v2(), 1);

    let v1_response = validator
        .do_register_validators(ValidatorRegistrationRequest {
            registrations: vec![test_registration()?],
            api_mode: ValidatorRegistrationApiMode::V1,
            context: None,
        })
        .await?;
    assert_eq!(v1_response.status(), StatusCode::OK);
    assert_eq!(relay_state.received_register_validator_v2(), 1);
    assert_eq!(relay_state.received_register_validator_v1(), 1);

    Ok(())
}

#[tokio::test]
async fn test_context_passthrough_when_provided() -> Result<()> {
    setup_test_env();
    let chain = Chain::Holesky;
    let relay_port = 6206;

    let relay_state = Arc::new(MockRelayState::new(chain, random_secret()));
    start_relay(relay_state.clone(), relay_port).await;

    let validator = MockValidator::new(relay_port)?;
    let context = RegisterValidatorContext {
        idempotency_key: "manual-idempotency-key".to_string(),
        source: Some("manual-source".to_string()),
        mode: RegistrationMode::Any,
    };

    let response = validator
        .do_register_validators(ValidatorRegistrationRequest {
            registrations: vec![test_registration()?],
            api_mode: ValidatorRegistrationApiMode::V2,
            context: Some(context.clone()),
        })
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    let stored = relay_state.stored_registrations();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].idempotency_key.as_deref(), Some(context.idempotency_key.as_str()));
    assert_eq!(stored[0].source.as_deref(), context.source.as_deref());
    assert_eq!(stored[0].mode.as_deref(), Some("any"));

    Ok(())
}

#[tokio::test]
async fn test_context_autogenerated_for_v2() -> Result<()> {
    setup_test_env();
    let chain = Chain::Holesky;
    let relay_port = 6207;

    let relay_state = Arc::new(MockRelayState::new(chain, random_secret()));
    start_relay(relay_state.clone(), relay_port).await;

    let validator = MockValidator::new(relay_port)?;
    let response = validator
        .do_register_validators(ValidatorRegistrationRequest {
            registrations: vec![test_registration()?],
            api_mode: ValidatorRegistrationApiMode::V2,
            context: None,
        })
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    let idempotency_keys = relay_state.register_validator_v2_idempotency_keys();
    assert_eq!(idempotency_keys.len(), 1);
    assert!(!idempotency_keys[0].is_empty());
    assert!(Uuid::parse_str(&idempotency_keys[0]).is_ok());

    let stored = relay_state.stored_registrations();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].source.as_deref(), Some("mock-validator"));
    assert_eq!(stored[0].mode.as_deref(), Some("all"));

    Ok(())
}

async fn start_relay(state: Arc<MockRelayState>, relay_port: u16) {
    tokio::spawn(start_mock_relay_service(state, relay_port));
    tokio::time::sleep(RELAY_STARTUP_DELAY).await;
}

fn test_registration() -> Result<ValidatorRegistration> {
    Ok(serde_json::from_str(
        r#"{
        "message": {
            "fee_recipient": "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "gas_limit": "100000",
            "timestamp": "1000000",
            "pubkey": "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        },
        "signature": "0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
    }"#,
    )?)
}
