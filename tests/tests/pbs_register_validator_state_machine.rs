use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
    time::Duration,
};

use alloy::rpc::types::beacon::relay::ValidatorRegistration;
use cb_common::{
    config::RegistrationApi,
    pbs::RegisterValidatorContext,
    signer::random_secret,
    types::{BlsPublicKey, Chain},
};
use cb_pbs::{DefaultBuilderApi, PbsService, PbsState};
use cb_tests::{
    mock_relay::{MockRelayState, RegistrationOutcome, start_mock_relay_service},
    mock_validator::{MockValidator, ValidatorRegistrationApiMode, ValidatorRegistrationRequest},
    register_validator_model::{Action, ModelState, Outcome, WireScript},
    utils::{
        generate_mock_relay_with_registration_api, get_pbs_static_config, setup_test_env,
        to_pbs_config,
    },
};
use eyre::Result;
use proptest::prelude::*;

static NEXT_BASE_PORT: AtomicU16 = AtomicU16::new(6500);

#[derive(Clone)]
struct RelayNode {
    id: String,
    state: Arc<MockRelayState>,
}

#[derive(Debug, Clone)]
struct RelaySnapshot {
    v1_count: u64,
    v2_count: u64,
    status_counts: BTreeMap<String, u64>,
    idempotency_keys: Vec<String>,
}

#[derive(Debug, Clone)]
struct StepSpec {
    explicit_context: bool,
    scripts: HashMap<String, WireScript>,
}

fn next_base_port() -> u16 {
    NEXT_BASE_PORT.fetch_add(20, Ordering::Relaxed)
}

fn registration() -> Result<ValidatorRegistration> {
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

async fn start_sut(
    base_port: u16,
    relay_modes: &[RegistrationApi],
    wait_all_registrations: bool,
    probe_cache_enabled: bool,
    retry_limit: u32,
) -> Result<(MockValidator, Vec<RelayNode>)> {
    let chain = Chain::Holesky;
    let pbs_port = base_port;

    let mut relays = Vec::with_capacity(relay_modes.len());
    let mut nodes = Vec::with_capacity(relay_modes.len());

    for (index, mode) in relay_modes.iter().copied().enumerate() {
        let relay_port = base_port + 1 + index as u16;
        let signer = random_secret();
        let pubkey: BlsPublicKey = signer.public_key();
        let state = Arc::new(MockRelayState::new(chain, signer));
        tokio::spawn(start_mock_relay_service(Arc::clone(&state), relay_port));

        let relay = generate_mock_relay_with_registration_api(relay_port, pubkey, mode)?;
        let relay_id = relay.id.to_string();
        relays.push(relay);
        nodes.push(RelayNode { id: relay_id, state });
    }

    let mut pbs_config = get_pbs_static_config(pbs_port);
    pbs_config.wait_all_registrations = wait_all_registrations;
    pbs_config.register_validator_probe_cache = probe_cache_enabled;
    pbs_config.register_validator_retry_limit = retry_limit;
    pbs_config.timeout_register_validator_ms = 5_000;

    let config = to_pbs_config(chain, pbs_config, relays);
    let state = PbsState::new(config, PathBuf::new());
    tokio::spawn(PbsService::run::<(), DefaultBuilderApi>(state));

    tokio::time::sleep(Duration::from_millis(120)).await;

    let validator = MockValidator::new(pbs_port)?;
    Ok((validator, nodes))
}

fn snapshot_relays(nodes: &[RelayNode]) -> HashMap<String, RelaySnapshot> {
    nodes
        .iter()
        .map(|node| {
            (
                node.id.clone(),
                RelaySnapshot {
                    v1_count: node.state.received_register_validator_v1(),
                    v2_count: node.state.received_register_validator_v2(),
                    status_counts: node.state.registration_status_counts(),
                    idempotency_keys: node.state.register_validator_v2_idempotency_keys(),
                },
            )
        })
        .collect()
}

fn expected_status_delta_keys(
    step: &cb_tests::register_validator_model::SubmitTransition,
    relay: &str,
) -> BTreeMap<String, u64> {
    let mut counts = BTreeMap::new();
    for attempt in step.attempts.iter().filter(|attempt| attempt.relay == relay) {
        let wire = match attempt.version {
            cb_common::pbs::BuilderApiVersion::V1 => "v1",
            cb_common::pbs::BuilderApiVersion::V2 => "v2",
        };
        *counts.entry(format!("{wire}:{}", attempt.status_code)).or_default() += 1;
    }
    counts
}

fn observed_status_delta(
    after: &BTreeMap<String, u64>,
    before: &BTreeMap<String, u64>,
) -> BTreeMap<String, u64> {
    let mut all_keys = after.keys().cloned().collect::<Vec<_>>();
    for key in before.keys() {
        if !all_keys.iter().any(|existing| existing == key) {
            all_keys.push(key.clone());
        }
    }
    all_keys.sort();
    all_keys.dedup();

    all_keys
        .into_iter()
        .filter_map(|key| {
            let after_value = after.get(&key).copied().unwrap_or_default();
            let before_value = before.get(&key).copied().unwrap_or_default();
            let delta = after_value.saturating_sub(before_value);
            if delta > 0 { Some((key, delta)) } else { None }
        })
        .collect()
}

fn map_script_to_mock(script: &[Outcome]) -> Vec<RegistrationOutcome> {
    script
        .iter()
        .map(|outcome| match outcome {
            Outcome::Ok200 => RegistrationOutcome::Ok200,
            Outcome::NotFound404 => RegistrationOutcome::NotFound404,
            Outcome::RateLimited429 => RegistrationOutcome::RateLimited429,
            Outcome::ServerError500 => RegistrationOutcome::ServerError500,
            Outcome::ServerError503 => RegistrationOutcome::ServerError503,
            Outcome::DelayMs(delay_ms) => RegistrationOutcome::DelayMs(*delay_ms),
        })
        .collect()
}

fn relay_step_matches_expected(
    relay_id: &str,
    before_snapshot: &RelaySnapshot,
    after_snapshot: &RelaySnapshot,
    expected: &cb_tests::register_validator_model::SubmitTransition,
) -> bool {
    let expected_v1 = expected.attempts_for(relay_id, cb_common::pbs::BuilderApiVersion::V1) as u64;
    let expected_v2 = expected.attempts_for(relay_id, cb_common::pbs::BuilderApiVersion::V2) as u64;
    let observed_v1 = after_snapshot.v1_count.saturating_sub(before_snapshot.v1_count);
    let observed_v2 = after_snapshot.v2_count.saturating_sub(before_snapshot.v2_count);
    if observed_v1 != expected_v1 || observed_v2 != expected_v2 {
        return false;
    }

    let expected_status = expected_status_delta_keys(expected, relay_id);
    let observed_status =
        observed_status_delta(&after_snapshot.status_counts, &before_snapshot.status_counts);
    if observed_status != expected_status {
        return false;
    }

    let new_key_count = after_snapshot
        .idempotency_keys
        .len()
        .saturating_sub(before_snapshot.idempotency_keys.len()) as u64;
    new_key_count == expected_v2
}

async fn wait_for_relays_idle(nodes: &[RelayNode], timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let all_idle =
            nodes.iter().all(|node| node.state.metrics_snapshot().register_validator_inflight == 0);
        if all_idle {
            return;
        }

        if tokio::time::Instant::now() >= deadline {
            return;
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_expected_settle(
    nodes: &[RelayNode],
    before: &HashMap<String, RelaySnapshot>,
    expected: &cb_tests::register_validator_model::SubmitTransition,
    timeout: Duration,
) -> HashMap<String, RelaySnapshot> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut latest = snapshot_relays(nodes);

    loop {
        let all_match = nodes.iter().all(|node| {
            let before_snapshot = before.get(&node.id).expect("missing before snapshot");
            let after_snapshot = latest.get(&node.id).expect("missing after snapshot");
            let no_inflight = node.state.metrics_snapshot().register_validator_inflight == 0;
            no_inflight
                && relay_step_matches_expected(&node.id, before_snapshot, after_snapshot, expected)
        });

        if all_match || tokio::time::Instant::now() >= deadline {
            return latest;
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
        latest = snapshot_relays(nodes);
    }
}

async fn run_step_and_assert(
    model: &mut ModelState,
    validator: &MockValidator,
    nodes: &[RelayNode],
    step_index: usize,
    step_spec: StepSpec,
    last_auto_idempotency: &mut HashMap<String, String>,
) -> Result<()> {
    wait_for_relays_idle(nodes, Duration::from_millis(1500)).await;
    let before = snapshot_relays(nodes);

    for node in nodes {
        node.state.clear_registration_scripts();
        if let Some(script) = step_spec.scripts.get(&node.id) {
            node.state.push_registration_script_v1(map_script_to_mock(&script.v1));
            node.state.push_registration_script_v2(map_script_to_mock(&script.v2));
        }
    }

    let expected = model
        .apply(Action::SubmitRegistration {
            wait_all: false,
            explicit_context: step_spec.explicit_context,
            scripts: step_spec.scripts.clone(),
        })
        .expect("submit action should produce a transition");

    let response = if step_spec.explicit_context {
        validator
            .do_register_validators(ValidatorRegistrationRequest {
                registrations: vec![registration()?],
                api_mode: ValidatorRegistrationApiMode::V2,
                context: Some(RegisterValidatorContext {
                    idempotency_key: format!("explicit-step-{step_index}"),
                    source: Some("state-machine-test".to_string()),
                    mode: cb_common::pbs::RegistrationMode::All,
                }),
            })
            .await?
    } else {
        validator.do_register_custom_validators(vec![registration()?]).await?
    };

    assert_eq!(response.status().is_success(), expected.overall_success);

    let after =
        wait_for_expected_settle(nodes, &before, &expected, Duration::from_millis(2500)).await;
    for node in nodes {
        let relay_id = &node.id;
        let before_snapshot = before.get(relay_id).expect("missing before snapshot");
        let after_snapshot = after.get(relay_id).expect("missing after snapshot");

        let expected_v1 =
            expected.attempts_for(relay_id, cb_common::pbs::BuilderApiVersion::V1) as u64;
        let expected_v2 =
            expected.attempts_for(relay_id, cb_common::pbs::BuilderApiVersion::V2) as u64;

        let observed_v1 = after_snapshot.v1_count.saturating_sub(before_snapshot.v1_count);
        let observed_v2 = after_snapshot.v2_count.saturating_sub(before_snapshot.v2_count);
        assert_eq!(observed_v1, expected_v1, "unexpected v1 attempt count for relay={relay_id}");
        assert_eq!(observed_v2, expected_v2, "unexpected v2 attempt count for relay={relay_id}");

        let expected_status = expected_status_delta_keys(&expected, relay_id);
        let observed_status =
            observed_status_delta(&after_snapshot.status_counts, &before_snapshot.status_counts);
        assert_eq!(
            observed_status, expected_status,
            "unexpected status deltas for relay={relay_id}"
        );

        let new_keys = &after_snapshot.idempotency_keys[before_snapshot.idempotency_keys.len()..];
        assert_eq!(
            new_keys.len() as u64,
            expected_v2,
            "unexpected idempotency key count for relay={relay_id}"
        );

        if expected_v2 > 1 {
            assert!(new_keys.windows(2).all(|window| window[0] == window[1]));
        }

        if expected_v2 > 0 && !step_spec.explicit_context {
            let first = new_keys[0].clone();
            if let Some(previous) = last_auto_idempotency.get(relay_id) {
                assert_ne!(
                    first, *previous,
                    "auto-generated idempotency key should change per request"
                );
            }
            last_auto_idempotency.insert(relay_id.clone(), first);
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_state_machine_trace_mixed_relays_deterministic() -> Result<()> {
    setup_test_env();

    let base_port = next_base_port();
    let relay_modes = vec![RegistrationApi::Auto, RegistrationApi::Auto, RegistrationApi::V2];
    let (validator, nodes) = start_sut(base_port, &relay_modes, false, true, 3).await?;

    let mut model = ModelState::new(
        nodes.iter().zip(relay_modes.iter().copied()).map(|(node, mode)| (node.id.clone(), mode)),
    );
    model.probe_cache_enabled = true;
    model.retry_limit = 3;

    let mut last_auto_idempotency = HashMap::new();

    let mut step1_scripts = HashMap::new();
    step1_scripts.insert(
        nodes[0].id.clone(),
        WireScript { v1: vec![Outcome::Ok200], v2: vec![Outcome::NotFound404] },
    );
    step1_scripts.insert(
        nodes[1].id.clone(),
        WireScript { v1: vec![], v2: vec![Outcome::ServerError500, Outcome::Ok200] },
    );
    step1_scripts
        .insert(nodes[2].id.clone(), WireScript { v1: vec![], v2: vec![Outcome::RateLimited429] });
    run_step_and_assert(
        &mut model,
        &validator,
        &nodes,
        1,
        StepSpec { explicit_context: false, scripts: step1_scripts },
        &mut last_auto_idempotency,
    )
    .await?;

    let mut step2_scripts = HashMap::new();
    step2_scripts.insert(nodes[2].id.clone(), WireScript { v1: vec![], v2: vec![Outcome::Ok200] });
    run_step_and_assert(
        &mut model,
        &validator,
        &nodes,
        2,
        StepSpec { explicit_context: false, scripts: step2_scripts },
        &mut last_auto_idempotency,
    )
    .await?;

    Ok(())
}

#[tokio::test]
async fn test_state_machine_restart_resets_capability_and_reprobes() -> Result<()> {
    setup_test_env();

    let base_port = next_base_port();
    let relay_modes = vec![RegistrationApi::Auto];
    let (validator_a, nodes_a) = start_sut(base_port, &relay_modes, false, true, 3).await?;

    let mut model = ModelState::new(vec![(nodes_a[0].id.clone(), RegistrationApi::Auto)]);
    model.probe_cache_enabled = true;
    model.retry_limit = 3;

    let mut last_auto_idempotency = HashMap::new();

    let mut step1_scripts = HashMap::new();
    step1_scripts.insert(
        nodes_a[0].id.clone(),
        WireScript { v1: vec![Outcome::Ok200], v2: vec![Outcome::NotFound404] },
    );
    run_step_and_assert(
        &mut model,
        &validator_a,
        &nodes_a,
        1,
        StepSpec { explicit_context: false, scripts: step1_scripts },
        &mut last_auto_idempotency,
    )
    .await?;

    run_step_and_assert(
        &mut model,
        &validator_a,
        &nodes_a,
        2,
        StepSpec { explicit_context: false, scripts: HashMap::new() },
        &mut last_auto_idempotency,
    )
    .await?;

    model.apply(Action::RestartPbs);
    last_auto_idempotency.clear();

    let (validator_b, nodes_b) = start_sut(base_port + 10, &relay_modes, false, true, 3).await?;
    model = ModelState::new(vec![(nodes_b[0].id.clone(), RegistrationApi::Auto)]);
    model.probe_cache_enabled = true;
    model.retry_limit = 3;

    run_step_and_assert(
        &mut model,
        &validator_b,
        &nodes_b,
        3,
        StepSpec { explicit_context: false, scripts: HashMap::new() },
        &mut last_auto_idempotency,
    )
    .await?;

    Ok(())
}

#[derive(Debug, Clone)]
struct GeneratedStep {
    explicit_context: bool,
    relay_0_v1: Vec<Outcome>,
    relay_0_v2: Vec<Outcome>,
    relay_1_v1: Vec<Outcome>,
    relay_1_v2: Vec<Outcome>,
}

fn generated_trace_strategy() -> impl Strategy<Value = Vec<GeneratedStep>> {
    let outcome = prop_oneof![
        Just(Outcome::Ok200),
        Just(Outcome::NotFound404),
        Just(Outcome::RateLimited429),
        Just(Outcome::ServerError500),
        Just(Outcome::ServerError503),
    ];

    let script = proptest::collection::vec(outcome, 0..3);
    let step = (any::<bool>(), script.clone(), script.clone(), script.clone(), script).prop_map(
        |(explicit_context, relay_0_v1, relay_0_v2, relay_1_v1, relay_1_v2)| GeneratedStep {
            explicit_context,
            relay_0_v1,
            relay_0_v2,
            relay_1_v1,
            relay_1_v2,
        },
    );

    proptest::collection::vec(step, 1..6)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 8,
        max_shrink_iters: 256,
        .. ProptestConfig::default()
    })]

    #[test]
    fn prop_model_matches_sut_for_generated_traces(trace in generated_trace_strategy()) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime build");

        runtime.block_on(async move {
            setup_test_env();
            let base_port = next_base_port();
            let relay_modes = vec![RegistrationApi::Auto, RegistrationApi::V2];
            let (validator, nodes) = start_sut(base_port, &relay_modes, false, true, 3).await.expect("start sut");

            let mut model = ModelState::new(
                nodes
                    .iter()
                    .zip(relay_modes.iter().copied())
                    .map(|(node, mode)| (node.id.clone(), mode)),
            );
            model.probe_cache_enabled = true;
            model.retry_limit = 3;

            let mut last_auto_idempotency = HashMap::new();
            for (index, generated_step) in trace.into_iter().enumerate() {
                let mut scripts = HashMap::new();
                scripts.insert(
                    nodes[0].id.clone(),
                    WireScript {
                        v1: generated_step.relay_0_v1,
                        v2: generated_step.relay_0_v2,
                    },
                );
                scripts.insert(
                    nodes[1].id.clone(),
                    WireScript {
                        v1: generated_step.relay_1_v1,
                        v2: generated_step.relay_1_v2,
                    },
                );

                run_step_and_assert(
                    &mut model,
                    &validator,
                    &nodes,
                    index,
                    StepSpec {
                        explicit_context: generated_step.explicit_context,
                        scripts,
                    },
                    &mut last_auto_idempotency,
                )
                .await
                .expect("step assertion");
            }
        });
    }
}
