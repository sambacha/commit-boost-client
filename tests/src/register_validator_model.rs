use std::collections::{BTreeMap, HashMap, VecDeque};

use cb_common::{config::RegistrationApi, pbs::BuilderApiVersion};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    Unknown,
    V1Only,
    V2Supported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Outcome {
    Ok200,
    NotFound404,
    RateLimited429,
    ServerError500,
    ServerError503,
    DelayMs(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WireScript {
    pub v1: Vec<Outcome>,
    pub v2: Vec<Outcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SetRelayMode {
        relay: String,
        mode: RegistrationApi,
    },
    SetProbeCache {
        enabled: bool,
    },
    RestartPbs,
    SubmitRegistration {
        wait_all: bool,
        explicit_context: bool,
        scripts: HashMap<String, WireScript>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptLog {
    pub relay: String,
    pub version: BuilderApiVersion,
    pub status_code: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitTransition {
    pub overall_success: bool,
    pub relay_success: HashMap<String, bool>,
    pub attempts: Vec<AttemptLog>,
}

impl SubmitTransition {
    pub fn attempts_for(&self, relay: &str, version: BuilderApiVersion) -> usize {
        self.attempts
            .iter()
            .filter(|attempt| attempt.relay == relay && attempt.version == version)
            .count()
    }

    pub fn status_counts_for(&self, relay: &str) -> BTreeMap<u16, usize> {
        let mut counts = BTreeMap::new();
        for attempt in self.attempts.iter().filter(|attempt| attempt.relay == relay) {
            *counts.entry(attempt.status_code).or_default() += 1;
        }
        counts
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelState {
    pub relay_capability: HashMap<String, Capability>,
    pub relay_mode: HashMap<String, RegistrationApi>,
    pub probe_cache_enabled: bool,
    pub retry_limit: u32,
    pub timeout_budget_ms: u64,
}

impl ModelState {
    pub fn new(relays: impl IntoIterator<Item = (String, RegistrationApi)>) -> Self {
        let relay_mode: HashMap<_, _> = relays.into_iter().collect();
        let relay_capability = relay_mode
            .keys()
            .map(|relay| (relay.clone(), Capability::Unknown))
            .collect::<HashMap<_, _>>();

        Self {
            relay_capability,
            relay_mode,
            probe_cache_enabled: true,
            retry_limit: 3,
            timeout_budget_ms: 3_000,
        }
    }

    pub fn apply(&mut self, action: Action) -> Option<SubmitTransition> {
        match action {
            Action::SetRelayMode { relay, mode } => {
                self.relay_mode.insert(relay, mode);
                None
            }
            Action::SetProbeCache { enabled } => {
                self.probe_cache_enabled = enabled;
                None
            }
            Action::RestartPbs => {
                for capability in self.relay_capability.values_mut() {
                    *capability = Capability::Unknown;
                }
                None
            }
            Action::SubmitRegistration { wait_all, explicit_context: _, scripts } => {
                Some(self.apply_submit(wait_all, scripts))
            }
        }
    }

    fn apply_submit(
        &mut self,
        wait_all: bool,
        scripts: HashMap<String, WireScript>,
    ) -> SubmitTransition {
        let mut attempts = Vec::new();
        let mut relay_success = HashMap::new();

        let mut relay_ids = self.relay_mode.keys().cloned().collect::<Vec<_>>();
        relay_ids.sort();

        for relay_id in relay_ids {
            let mode = self.relay_mode.get(&relay_id).copied().unwrap_or(RegistrationApi::Auto);
            let capability =
                self.relay_capability.get(&relay_id).copied().unwrap_or(Capability::Unknown);

            let mut selected_version =
                initial_registration_api_version(mode, capability, self.probe_cache_enabled);
            let mut retry = 0u32;
            let mut backoff_ms = 250u64;
            let mut elapsed_ms = 0u64;
            let mut relay_script = scripts
                .get(&relay_id)
                .cloned()
                .map(|script| ScriptQueues {
                    v1: script.v1.into_iter().collect(),
                    v2: script.v2.into_iter().collect(),
                })
                .unwrap_or_default();
            let mut success = false;

            loop {
                let (delay_ms, status_code) = relay_script.next_for(selected_version);
                elapsed_ms = elapsed_ms.saturating_add(delay_ms);
                let observed_status =
                    if elapsed_ms >= self.timeout_budget_ms { 599 } else { status_code };

                attempts.push(AttemptLog {
                    relay: relay_id.clone(),
                    version: selected_version,
                    status_code: observed_status,
                });

                if observed_status / 100 == 2 {
                    if self.probe_cache_enabled
                        && matches!(mode, RegistrationApi::Auto)
                        && matches!(selected_version, BuilderApiVersion::V2)
                    {
                        self.relay_capability.insert(relay_id.clone(), Capability::V2Supported);
                    }
                    success = true;
                    break;
                }

                if observed_status == 404
                    && matches!(mode, RegistrationApi::Auto)
                    && matches!(selected_version, BuilderApiVersion::V2)
                {
                    if self.probe_cache_enabled {
                        self.relay_capability.insert(relay_id.clone(), Capability::V1Only);
                    }
                    selected_version = BuilderApiVersion::V1;
                    continue;
                }

                let retryable = (500..=599).contains(&observed_status);
                if retryable {
                    retry = retry.saturating_add(1);
                    if retry >= self.retry_limit {
                        break;
                    }

                    let remaining_budget = self.timeout_budget_ms.saturating_sub(elapsed_ms);
                    let sleep_ms = remaining_budget.min(backoff_ms);
                    if sleep_ms == 0 {
                        break;
                    }
                    elapsed_ms = elapsed_ms.saturating_add(sleep_ms);
                    backoff_ms = backoff_ms.saturating_add(250);
                    if elapsed_ms < self.timeout_budget_ms {
                        continue;
                    }
                }

                break;
            }

            relay_success.insert(relay_id, success);
        }

        let overall_success = if wait_all {
            relay_success.values().all(|success| *success)
        } else {
            relay_success.values().any(|success| *success)
        };

        SubmitTransition { overall_success, relay_success, attempts }
    }
}

#[derive(Debug, Default)]
struct ScriptQueues {
    v1: VecDeque<Outcome>,
    v2: VecDeque<Outcome>,
}

impl ScriptQueues {
    fn next_for(&mut self, version: BuilderApiVersion) -> (u64, u16) {
        let queue = match version {
            BuilderApiVersion::V1 => &mut self.v1,
            BuilderApiVersion::V2 => &mut self.v2,
        };

        let mut accumulated_delay_ms = 0u64;
        loop {
            match queue.pop_front() {
                Some(Outcome::DelayMs(delay_ms)) => {
                    accumulated_delay_ms = accumulated_delay_ms.saturating_add(delay_ms);
                }
                Some(outcome) => {
                    return (accumulated_delay_ms, status_code_of(outcome));
                }
                None => {
                    return (accumulated_delay_ms, 200);
                }
            }
        }
    }
}

pub fn initial_registration_api_version(
    mode: RegistrationApi,
    capability: Capability,
    probe_cache_enabled: bool,
) -> BuilderApiVersion {
    match mode {
        RegistrationApi::V1 => BuilderApiVersion::V1,
        RegistrationApi::V2 => BuilderApiVersion::V2,
        RegistrationApi::Auto => {
            if !probe_cache_enabled {
                return BuilderApiVersion::V2;
            }

            match capability {
                Capability::V1Only => BuilderApiVersion::V1,
                Capability::Unknown | Capability::V2Supported => BuilderApiVersion::V2,
            }
        }
    }
}

pub fn status_code_of(outcome: Outcome) -> u16 {
    match outcome {
        Outcome::Ok200 => 200,
        Outcome::NotFound404 => 404,
        Outcome::RateLimited429 => 429,
        Outcome::ServerError500 => 500,
        Outcome::ServerError503 => 503,
        Outcome::DelayMs(_) => 200,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use proptest::prelude::*;

    use super::{Action, Capability, ModelState, Outcome, WireScript};
    use cb_common::config::RegistrationApi;

    fn state() -> ModelState {
        ModelState::new(vec![("relay_a".to_string(), RegistrationApi::Auto)])
    }

    #[test]
    fn test_restart_resets_capability_cache() {
        let mut model = state();
        model.relay_capability.insert("relay_a".to_string(), Capability::V1Only);
        model.apply(Action::RestartPbs);
        assert_eq!(model.relay_capability["relay_a"], Capability::Unknown);
    }

    #[test]
    fn test_auto_404_fallback_sets_v1_only_capability() {
        let mut model = state();

        let mut scripts = HashMap::new();
        scripts.insert(
            "relay_a".to_string(),
            WireScript { v1: vec![Outcome::Ok200], v2: vec![Outcome::NotFound404] },
        );

        let transition = model
            .apply(Action::SubmitRegistration { wait_all: false, explicit_context: false, scripts })
            .expect("submit should produce transition");

        assert!(transition.overall_success);
        assert_eq!(transition.attempts.len(), 2);
        assert_eq!(model.relay_capability["relay_a"], Capability::V1Only);
    }

    #[test]
    fn test_non_404_does_not_fallback() {
        let mut model = state();

        let mut scripts = HashMap::new();
        scripts.insert(
            "relay_a".to_string(),
            WireScript { v1: vec![Outcome::Ok200], v2: vec![Outcome::RateLimited429] },
        );

        let transition = model
            .apply(Action::SubmitRegistration { wait_all: false, explicit_context: false, scripts })
            .expect("submit should produce transition");

        assert!(!transition.overall_success);
        assert_eq!(transition.attempts.len(), 1);
        assert_eq!(model.relay_capability["relay_a"], Capability::Unknown);
    }

    proptest! {
        #[test]
        fn prop_transition_is_deterministic(
            probe_cache_enabled in any::<bool>(),
            retry_limit in 1u32..5,
            timeout_budget_ms in 1u64..10_000,
            wait_all in any::<bool>(),
            v1_script in proptest::collection::vec(
                prop_oneof![
                    Just(Outcome::Ok200),
                    Just(Outcome::NotFound404),
                    Just(Outcome::RateLimited429),
                    Just(Outcome::ServerError500),
                    Just(Outcome::ServerError503),
                    (1u64..300u64).prop_map(Outcome::DelayMs),
                ],
                0..4,
            ),
            v2_script in proptest::collection::vec(
                prop_oneof![
                    Just(Outcome::Ok200),
                    Just(Outcome::NotFound404),
                    Just(Outcome::RateLimited429),
                    Just(Outcome::ServerError500),
                    Just(Outcome::ServerError503),
                    (1u64..300u64).prop_map(Outcome::DelayMs),
                ],
                0..4,
            ),
        ) {
            let mut left = ModelState::new(vec![("relay_a".to_string(), RegistrationApi::Auto)]);
            left.probe_cache_enabled = probe_cache_enabled;
            left.retry_limit = retry_limit;
            left.timeout_budget_ms = timeout_budget_ms;

            let mut right = left.clone();

            let mut scripts = HashMap::new();
            scripts.insert(
                "relay_a".to_string(),
                WireScript { v1: v1_script.clone(), v2: v2_script.clone() },
            );

            let left_transition = left.apply(Action::SubmitRegistration {
                wait_all,
                explicit_context: false,
                scripts: scripts.clone(),
            });
            let right_transition = right.apply(Action::SubmitRegistration {
                wait_all,
                explicit_context: false,
                scripts,
            });

            prop_assert_eq!(left_transition, right_transition);
            prop_assert_eq!(left, right);
        }
    }
}
