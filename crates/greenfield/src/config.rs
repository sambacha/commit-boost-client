use std::{fs, path::Path, time::Duration};

use eyre::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::types::{FanoutMode, RelayDescriptor};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformConfig {
    pub pbs: PbsConfig,
    pub signer: SignerConfig,
    pub relays: Vec<RelayDescriptor>,
}

impl PlatformConfig {
    pub fn validate_local_invariants(&self) -> Result<()> {
        self.pbs.validate_local_invariants()?;
        self.signer.validate_local_invariants()?;

        ensure!(!self.relays.is_empty(), "at least one relay must be configured");
        for relay in &self.relays {
            ensure!(!relay.id.trim().is_empty(), "relay id cannot be empty");
            ensure!(
                relay.base_url.starts_with("http://") || relay.base_url.starts_with("https://"),
                "relay {} must use http(s)",
                relay.id
            );
        }

        Ok(())
    }

    pub fn from_toml_file(path: impl AsRef<Path>) -> Result<Self> {
        let raw = fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&raw)?;
        cfg.validate_local_invariants()?;
        Ok(cfg)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PbsConfig {
    pub bind_address: String,
    pub timeout_get_header_ms: u64,
    pub timeout_submit_block_ms: u64,
    pub timeout_register_validator_ms: u64,
    pub late_in_slot_time_ms: u64,
    pub register_validator_retry_limit: u32,
    pub register_validator_probe_cache: bool,
    pub register_validator_max_in_flight: u32,
    pub min_bid_wei: u64,
    pub relay_check: bool,
    pub registration_fanout_mode: FanoutModeConfig,
    pub max_request_body_bytes: usize,
    pub extra_validation_enabled: bool,
    pub rpc_url: Option<String>,
}

impl PbsConfig {
    pub fn validate_local_invariants(&self) -> Result<()> {
        ensure!(self.timeout_get_header_ms > 0, "timeout_get_header_ms must be > 0");
        ensure!(self.timeout_submit_block_ms > 0, "timeout_submit_block_ms must be > 0");
        ensure!(
            self.timeout_register_validator_ms > 0,
            "timeout_register_validator_ms must be > 0"
        );
        ensure!(self.late_in_slot_time_ms > 0, "late_in_slot_time_ms must be > 0");
        ensure!(
            self.timeout_get_header_ms < self.late_in_slot_time_ms,
            "timeout_get_header_ms must be less than late_in_slot_time_ms"
        );
        ensure!(
            self.register_validator_retry_limit > 0,
            "register_validator_retry_limit must be > 0"
        );
        ensure!(
            self.register_validator_max_in_flight > 0,
            "register_validator_max_in_flight must be > 0"
        );
        ensure!(self.max_request_body_bytes > 0, "max_request_body_bytes must be > 0");

        if self.extra_validation_enabled {
            ensure!(self.rpc_url.is_some(), "rpc_url is required when extra_validation_enabled");
        }

        Ok(())
    }

    pub fn register_timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_register_validator_ms)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FanoutModeConfig {
    AnySuccess,
    AllMustSucceed,
}

impl From<FanoutModeConfig> for FanoutMode {
    fn from(value: FanoutModeConfig) -> Self {
        match value {
            FanoutModeConfig::AnySuccess => FanoutMode::AnySuccess,
            FanoutModeConfig::AllMustSucceed => FanoutMode::AllMustSucceed,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignerConfig {
    pub bind_address: String,
    pub api_key: Option<String>,
    pub max_request_body_bytes: usize,
}

impl SignerConfig {
    pub fn validate_local_invariants(&self) -> Result<()> {
        ensure!(self.max_request_body_bytes > 0, "signer max_request_body_bytes must be > 0");
        Ok(())
    }
}

impl Default for PlatformConfig {
    fn default() -> Self {
        Self {
            pbs: PbsConfig {
                bind_address: "127.0.0.1:18550".to_string(),
                timeout_get_header_ms: 950,
                timeout_submit_block_ms: 4_000,
                timeout_register_validator_ms: 2_000,
                late_in_slot_time_ms: 3_000,
                register_validator_retry_limit: 3,
                register_validator_probe_cache: true,
                register_validator_max_in_flight: 8,
                min_bid_wei: 0,
                relay_check: true,
                registration_fanout_mode: FanoutModeConfig::AllMustSucceed,
                max_request_body_bytes: 2 * 1024 * 1024,
                extra_validation_enabled: false,
                rpc_url: None,
            },
            signer: SignerConfig {
                bind_address: "127.0.0.1:18551".to_string(),
                api_key: None,
                max_request_body_bytes: 512 * 1024,
            },
            relays: vec![RelayDescriptor {
                id: "relay-1".to_string(),
                base_url: "http://127.0.0.1:28550".to_string(),
                registration_api: Default::default(),
            }],
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::PlatformConfig;

    fn valid_cfg() -> PlatformConfig {
        PlatformConfig::default()
    }

    proptest! {
        #[test]
        fn prop_timeout_get_header_must_be_less_than_late_slot(late in 1u64..10_000, delta in 0u64..10_000) {
            let mut cfg = valid_cfg();
            cfg.pbs.late_in_slot_time_ms = late;
            cfg.pbs.timeout_get_header_ms = late.saturating_add(delta);
            prop_assert!(cfg.validate_local_invariants().is_err());
        }

        #[test]
        fn prop_retry_limit_zero_fails(_marker in Just(())) {
            let mut cfg = valid_cfg();
            cfg.pbs.register_validator_retry_limit = 0;
            prop_assert!(cfg.validate_local_invariants().is_err());
        }

        #[test]
        fn prop_max_in_flight_zero_fails(_marker in Just(())) {
            let mut cfg = valid_cfg();
            cfg.pbs.register_validator_max_in_flight = 0;
            prop_assert!(cfg.validate_local_invariants().is_err());
        }

        #[test]
        fn prop_extra_validation_requires_rpc_url(_marker in Just(())) {
            let mut cfg = valid_cfg();
            cfg.pbs.extra_validation_enabled = true;
            cfg.pbs.rpc_url = None;
            prop_assert!(cfg.validate_local_invariants().is_err());
        }

        #[test]
        fn prop_valid_constraints_pass(
            timeout_get_header_ms in 1u64..5_000,
            timeout_submit_block_ms in 1u64..5_000,
            timeout_register_validator_ms in 1u64..5_000,
            late_delta in 1u64..10_000,
            retry_limit in 1u32..20,
            max_in_flight in 1u32..64,
            max_body in 1usize..2_000_000,
        ) {
            let mut cfg = valid_cfg();
            cfg.pbs.timeout_get_header_ms = timeout_get_header_ms;
            cfg.pbs.timeout_submit_block_ms = timeout_submit_block_ms;
            cfg.pbs.timeout_register_validator_ms = timeout_register_validator_ms;
            cfg.pbs.late_in_slot_time_ms = timeout_get_header_ms.saturating_add(late_delta);
            cfg.pbs.register_validator_retry_limit = retry_limit;
            cfg.pbs.register_validator_max_in_flight = max_in_flight;
            cfg.pbs.max_request_body_bytes = max_body;

            prop_assert!(cfg.validate_local_invariants().is_ok());
        }
    }
}
