use std::{sync::Arc, time::Duration};

use alloy::rpc::types::beacon::relay::ValidatorRegistration;
use cb_common::{
    pbs::{RegisterValidatorContext, RegisterValidatorV2Request, RegistrationMode},
    signer::random_secret,
    types::Chain,
};
use cb_tests::{
    mock_relay::{
        MockRelayMetricsSnapshot, MockRelayState, RegistrationWireVersion,
        StoredValidatorRegistration, start_mock_relay_service,
    },
    utils::setup_test_env,
};
use eyre::Result;
use reqwest::StatusCode;

#[tokio::test]
async fn test_mock_relay_persists_v2_registrations_and_exposes_debug_api() -> Result<()> {
    setup_test_env();

    let chain = Chain::Holesky;
    let relay_port = 6101;
    let state = Arc::new(MockRelayState::new(chain, random_secret()));
    tokio::spawn(start_mock_relay_service(state, relay_port));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = reqwest::Client::new();
    let registration = test_registration()?;
    let pubkey = registration.message.pubkey.to_string().to_ascii_lowercase();
    let request = RegisterValidatorV2Request {
        registrations: vec![registration],
        context: RegisterValidatorContext {
            idempotency_key: "mock-id-1".to_string(),
            source: Some("integration-test".to_string()),
            mode: RegistrationMode::All,
        },
    };

    let response = client
        .post(format!("http://0.0.0.0:{relay_port}/eth/v2/builder/validators"))
        .json(&request)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    let all = client
        .get(format!("http://0.0.0.0:{relay_port}/debug/registrations"))
        .send()
        .await?
        .json::<Vec<StoredValidatorRegistration>>()
        .await?;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].pubkey, pubkey);
    assert_eq!(all[0].wire_version, RegistrationWireVersion::V2);
    assert_eq!(all[0].idempotency_key.as_deref(), Some("mock-id-1"));

    let one = client
        .get(format!("http://0.0.0.0:{relay_port}/debug/registrations/{pubkey}"))
        .send()
        .await?
        .json::<StoredValidatorRegistration>()
        .await?;
    assert_eq!(one.registration.message.gas_limit, 100_000);

    let metrics = client
        .get(format!("http://0.0.0.0:{relay_port}/debug/metrics"))
        .send()
        .await?
        .json::<MockRelayMetricsSnapshot>()
        .await?;
    assert_eq!(metrics.received_register_validator_v2, 1);
    assert_eq!(metrics.stored_registrations, 1);
    assert_eq!(metrics.stored_registration_upserts, 1);

    Ok(())
}

#[tokio::test]
async fn test_mock_relay_rejects_invalid_registration_and_tracks_failures() -> Result<()> {
    setup_test_env();

    let chain = Chain::Holesky;
    let relay_port = 6102;
    let state = Arc::new(MockRelayState::new(chain, random_secret()));
    tokio::spawn(start_mock_relay_service(state, relay_port));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = reqwest::Client::new();
    let mut invalid = test_registration()?;
    invalid.message.gas_limit = 0;

    let response = client
        .post(format!("http://0.0.0.0:{relay_port}/eth/v1/builder/validators"))
        .json(&vec![invalid])
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let metrics = client
        .get(format!("http://0.0.0.0:{relay_port}/debug/metrics"))
        .send()
        .await?
        .json::<MockRelayMetricsSnapshot>()
        .await?;
    assert_eq!(metrics.registration_validation_failures, 1);
    assert_eq!(metrics.stored_registrations, 0);

    Ok(())
}

#[tokio::test]
async fn test_mock_relay_upserts_registration_by_pubkey() -> Result<()> {
    setup_test_env();

    let chain = Chain::Holesky;
    let relay_port = 6103;
    let state = Arc::new(MockRelayState::new(chain, random_secret()));
    tokio::spawn(start_mock_relay_service(state, relay_port));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let client = reqwest::Client::new();
    let first = test_registration()?;
    let pubkey = first.message.pubkey.to_string().to_ascii_lowercase();

    let response = client
        .post(format!("http://0.0.0.0:{relay_port}/eth/v1/builder/validators"))
        .json(&vec![first])
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    let mut second = test_registration()?;
    second.message.gas_limit = 200_000;

    let response = client
        .post(format!("http://0.0.0.0:{relay_port}/eth/v2/builder/validators"))
        .json(&RegisterValidatorV2Request {
            registrations: vec![second],
            context: RegisterValidatorContext {
                idempotency_key: "mock-id-2".to_string(),
                source: None,
                mode: RegistrationMode::Any,
            },
        })
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    let one = client
        .get(format!("http://0.0.0.0:{relay_port}/debug/registrations/{pubkey}"))
        .send()
        .await?
        .json::<StoredValidatorRegistration>()
        .await?;
    assert_eq!(one.registration.message.gas_limit, 200_000);
    assert_eq!(one.wire_version, RegistrationWireVersion::V2);
    assert_eq!(one.idempotency_key.as_deref(), Some("mock-id-2"));

    let metrics = client
        .get(format!("http://0.0.0.0:{relay_port}/debug/metrics"))
        .send()
        .await?
        .json::<MockRelayMetricsSnapshot>()
        .await?;
    assert_eq!(metrics.stored_registration_upserts, 2);
    assert_eq!(metrics.stored_registrations, 1);

    Ok(())
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
