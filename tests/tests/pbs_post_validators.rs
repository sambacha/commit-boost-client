use std::{path::PathBuf, sync::Arc, time::Duration};

use alloy::rpc::types::beacon::relay::ValidatorRegistration;
use cb_common::{
    config::RegistrationApi,
    signer::random_secret,
    types::{BlsPublicKey, Chain},
};
use cb_pbs::{DefaultBuilderApi, PbsService, PbsState};
use cb_tests::{
    mock_relay::{MockRelayState, start_mock_relay_service},
    mock_validator::MockValidator,
    utils::{
        generate_mock_relay, generate_mock_relay_with_registration_api, get_pbs_static_config,
        setup_test_env, to_pbs_config,
    },
};
use eyre::Result;
use reqwest::StatusCode;
use tracing::info;

#[tokio::test]
async fn test_register_validators() -> Result<()> {
    setup_test_env();
    let signer = random_secret();
    let pubkey: BlsPublicKey = signer.public_key();

    let chain = Chain::Holesky;
    let pbs_port = 4000;

    // Run a mock relay
    let relays = vec![generate_mock_relay(pbs_port + 1, pubkey)?];
    let mock_state = Arc::new(MockRelayState::new(chain, signer));
    tokio::spawn(start_mock_relay_service(mock_state.clone(), pbs_port + 1));

    // Run the PBS service
    let config = to_pbs_config(chain, get_pbs_static_config(pbs_port), relays);
    let state = PbsState::new(config, PathBuf::new());
    tokio::spawn(PbsService::run::<(), DefaultBuilderApi>(state));

    // leave some time to start servers
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mock_validator = MockValidator::new(pbs_port)?;
    info!("Sending register validator");

    let registrations = vec![test_registration()?];
    let res = mock_validator.do_register_custom_validators(registrations).await?;

    assert_eq!(mock_state.received_register_validator(), 1);
    assert_eq!(mock_state.received_register_validator_v1(), 0);
    assert_eq!(mock_state.received_register_validator_v2(), 1);
    assert_eq!(res.status(), StatusCode::OK);

    Ok(())
}

#[tokio::test]
async fn test_register_validators_does_not_retry_on_429() -> Result<()> {
    setup_test_env();
    let signer = random_secret();
    let pubkey: BlsPublicKey = signer.public_key();

    let chain = Chain::Holesky;
    let pbs_port = 4200;

    // Set up mock relay state and override response to 429
    let mock_state = Arc::new(MockRelayState::new(chain, signer));
    mock_state.set_response_override(StatusCode::TOO_MANY_REQUESTS);

    // Run a mock relay
    let relays = vec![generate_mock_relay(pbs_port + 1, pubkey)?];
    tokio::spawn(start_mock_relay_service(mock_state.clone(), pbs_port + 1));

    // Run the PBS service
    let config = to_pbs_config(chain, get_pbs_static_config(pbs_port), relays);
    let state = PbsState::new(config, PathBuf::new());
    tokio::spawn(PbsService::run::<(), DefaultBuilderApi>(state.clone()));

    // Leave some time to start servers
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mock_validator = MockValidator::new(pbs_port)?;
    info!("Sending register validator to test 429 response");

    let registrations = vec![test_registration()?];
    let res = mock_validator.do_register_custom_validators(registrations).await?;

    // Should only be called once (no retry)
    assert_eq!(mock_state.received_register_validator(), 1);
    // Expected to return 429 status code
    // But it returns `No relay passed register_validator successfully` with 502
    // status code
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(mock_state.received_register_validator_v1(), 0);
    assert_eq!(mock_state.received_register_validator_v2(), 1);

    Ok(())
}

#[tokio::test]
async fn test_register_validators_retries_on_500() -> Result<()> {
    setup_test_env();
    let signer = random_secret();
    let pubkey: BlsPublicKey = signer.public_key();

    let chain = Chain::Holesky;
    let pbs_port = 4300;

    // Set up internal mock relay with 500 response override
    let mock_state = Arc::new(MockRelayState::new(chain, signer));
    mock_state.set_response_override(StatusCode::INTERNAL_SERVER_ERROR); // 500

    let relays = vec![generate_mock_relay(pbs_port + 1, pubkey)?];
    tokio::spawn(start_mock_relay_service(mock_state.clone(), pbs_port + 1));

    // Set retry limit to 3
    let mut pbs_config = get_pbs_static_config(pbs_port);
    pbs_config.register_validator_retry_limit = 3;

    let config = to_pbs_config(chain, pbs_config, relays);
    let state = PbsState::new(config, PathBuf::new());
    tokio::spawn(PbsService::run::<(), DefaultBuilderApi>(state.clone()));

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mock_validator = MockValidator::new(pbs_port)?;
    info!("Sending register validator to test retry on 500");

    let registrations = vec![test_registration()?];
    let _ = mock_validator.do_register_custom_validators(registrations).await;

    // Should retry 3 times (0, 1, 2) → total 3 calls
    assert_eq!(mock_state.received_register_validator(), 3);
    assert_eq!(mock_state.received_register_validator_v1(), 0);
    assert_eq!(mock_state.received_register_validator_v2(), 3);
    let idempotency_keys = mock_state.register_validator_v2_idempotency_keys();
    assert_eq!(idempotency_keys.len(), 3);
    assert!(idempotency_keys.windows(2).all(|w| w[0] == w[1]));

    Ok(())
}

#[tokio::test]
async fn test_register_validators_falls_back_to_v1_on_v2_404() -> Result<()> {
    setup_test_env();
    let signer = random_secret();
    let pubkey: BlsPublicKey = signer.public_key();

    let chain = Chain::Holesky;
    let pbs_port = 4400;

    let mock_state =
        Arc::new(MockRelayState::new(chain, signer).with_not_found_for_register_validator_v2());
    let relays = vec![generate_mock_relay(pbs_port + 1, pubkey)?];
    tokio::spawn(start_mock_relay_service(mock_state.clone(), pbs_port + 1));

    let config = to_pbs_config(chain, get_pbs_static_config(pbs_port), relays);
    let state = PbsState::new(config, PathBuf::new());
    tokio::spawn(PbsService::run::<(), DefaultBuilderApi>(state));

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mock_validator = MockValidator::new(pbs_port)?;
    let res = mock_validator.do_register_custom_validators(vec![test_registration()?]).await?;

    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(mock_state.received_register_validator_v2(), 1);
    assert_eq!(mock_state.received_register_validator_v1(), 1);
    assert_eq!(mock_state.received_register_validator(), 2);

    Ok(())
}

#[tokio::test]
async fn test_register_validators_v2_pin_does_not_fallback_on_404() -> Result<()> {
    setup_test_env();
    let signer = random_secret();
    let pubkey: BlsPublicKey = signer.public_key();

    let chain = Chain::Holesky;
    let pbs_port = 4500;

    let mock_state =
        Arc::new(MockRelayState::new(chain, signer).with_not_found_for_register_validator_v2());
    let relays =
        vec![generate_mock_relay_with_registration_api(pbs_port + 1, pubkey, RegistrationApi::V2)?];
    tokio::spawn(start_mock_relay_service(mock_state.clone(), pbs_port + 1));

    let config = to_pbs_config(chain, get_pbs_static_config(pbs_port), relays);
    let state = PbsState::new(config, PathBuf::new());
    tokio::spawn(PbsService::run::<(), DefaultBuilderApi>(state));

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mock_validator = MockValidator::new(pbs_port)?;
    let res = mock_validator.do_register_custom_validators(vec![test_registration()?]).await?;

    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(mock_state.received_register_validator_v2(), 1);
    assert_eq!(mock_state.received_register_validator_v1(), 0);
    assert_eq!(mock_state.received_register_validator(), 1);

    Ok(())
}

#[tokio::test]
async fn test_register_validators_v2_inbound_route() -> Result<()> {
    setup_test_env();
    let signer = random_secret();
    let pubkey: BlsPublicKey = signer.public_key();

    let chain = Chain::Holesky;
    let pbs_port = 4600;

    let mock_state = Arc::new(MockRelayState::new(chain, signer));
    let relays = vec![generate_mock_relay(pbs_port + 1, pubkey)?];
    tokio::spawn(start_mock_relay_service(mock_state.clone(), pbs_port + 1));

    let config = to_pbs_config(chain, get_pbs_static_config(pbs_port), relays);
    let state = PbsState::new(config, PathBuf::new());
    tokio::spawn(PbsService::run::<(), DefaultBuilderApi>(state));

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mock_validator = MockValidator::new(pbs_port)?;
    let res = mock_validator.do_register_custom_validators_v2(vec![test_registration()?]).await?;

    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(mock_state.received_register_validator_v2(), 1);
    assert_eq!(mock_state.received_register_validator_v1(), 0);

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
