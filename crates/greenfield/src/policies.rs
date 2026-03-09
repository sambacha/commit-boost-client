use std::time::{Duration, Instant};

use thiserror::Error;

use crate::types::{
    BuilderApiVersion, RegistrationApiMode, RelayCapability, ValidatorRegistration,
};

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ValidationError {
    #[error("empty block hash")]
    EmptyBlockHash,
    #[error("parent hash mismatch")]
    ParentHashMismatch,
    #[error("empty transactions root")]
    EmptyTxRoot,
    #[error("bid below minimum")]
    BidTooLow,
    #[error("timestamp mismatch")]
    TimestampMismatch,
    #[error("empty validator pubkey")]
    EmptyValidatorPubkey,
    #[error("empty validator signature")]
    EmptyValidatorSignature,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayError {
    Timeout,
    Connect,
    HttpStatus(u16),
    Decode,
    Internal(String),
}

impl RelayError {
    pub fn should_retry(&self) -> bool {
        match self {
            Self::Timeout | Self::Connect => true,
            Self::HttpStatus(code) => (500..=599).contains(code),
            Self::Decode | Self::Internal(_) => false,
        }
    }

    pub fn status_code_label(&self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Connect => "connect",
            Self::HttpStatus(code) => {
                if *code == 404 {
                    "404"
                } else if *code == 429 {
                    "429"
                } else if (500..=599).contains(code) {
                    "5xx"
                } else {
                    "error"
                }
            }
            Self::Decode => "decode",
            Self::Internal(_) => "internal",
        }
    }
}

pub trait RegistrationPolicy: Send + Sync {
    fn select_version(
        &self,
        mode: RegistrationApiMode,
        capability: RelayCapability,
        probe_cache_enabled: bool,
    ) -> BuilderApiVersion;
}

#[derive(Debug, Default, Clone)]
pub struct DefaultRegistrationPolicy;

impl RegistrationPolicy for DefaultRegistrationPolicy {
    fn select_version(
        &self,
        mode: RegistrationApiMode,
        capability: RelayCapability,
        probe_cache_enabled: bool,
    ) -> BuilderApiVersion {
        match mode {
            RegistrationApiMode::V1 => BuilderApiVersion::V1,
            RegistrationApiMode::V2 => BuilderApiVersion::V2,
            RegistrationApiMode::Auto => {
                if !probe_cache_enabled {
                    return BuilderApiVersion::V2;
                }
                match capability {
                    RelayCapability::V1Only => BuilderApiVersion::V1,
                    RelayCapability::Unknown | RelayCapability::V2Supported => {
                        BuilderApiVersion::V2
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    retry_limit: u32,
    base_backoff: Duration,
}

impl RetryPolicy {
    pub fn new(retry_limit: u32, base_backoff: Duration) -> Self {
        Self { retry_limit, base_backoff }
    }

    pub fn should_retry(
        &self,
        attempt: u32,
        error: &RelayError,
        now: Instant,
        deadline: Instant,
    ) -> Option<Duration> {
        if attempt >= self.retry_limit {
            return None;
        }
        if !error.should_retry() {
            return None;
        }
        if now >= deadline {
            return None;
        }

        let next_backoff = self.base_backoff.saturating_mul(attempt.saturating_add(1));
        Some(next_backoff.min(deadline.saturating_duration_since(now)))
    }
}

pub trait ValidationEngine: Send + Sync {
    fn validate_header(
        &self,
        block_hash: &str,
        parent_hash: &str,
        expected_parent_hash: &str,
        tx_root: &str,
        bid_wei: u64,
        min_bid_wei: u64,
        timestamp: u64,
        expected_timestamp: u64,
    ) -> Result<(), ValidationError>;

    fn validate_registrations(
        &self,
        registrations: &[ValidatorRegistration],
    ) -> Result<(), ValidationError>;

    fn validate_submit_block(&self, block_hash: &str) -> Result<(), ValidationError>;
}

#[derive(Debug, Clone, Default)]
pub struct DefaultValidationEngine;

impl ValidationEngine for DefaultValidationEngine {
    fn validate_header(
        &self,
        block_hash: &str,
        parent_hash: &str,
        expected_parent_hash: &str,
        tx_root: &str,
        bid_wei: u64,
        min_bid_wei: u64,
        timestamp: u64,
        expected_timestamp: u64,
    ) -> Result<(), ValidationError> {
        if block_hash.is_empty() || block_hash == "0x0" {
            return Err(ValidationError::EmptyBlockHash);
        }
        if parent_hash != expected_parent_hash {
            return Err(ValidationError::ParentHashMismatch);
        }
        if tx_root.is_empty() || tx_root == "0x0" {
            return Err(ValidationError::EmptyTxRoot);
        }
        if bid_wei < min_bid_wei {
            return Err(ValidationError::BidTooLow);
        }
        if timestamp != expected_timestamp {
            return Err(ValidationError::TimestampMismatch);
        }
        Ok(())
    }

    fn validate_registrations(
        &self,
        registrations: &[ValidatorRegistration],
    ) -> Result<(), ValidationError> {
        for registration in registrations {
            if registration.pubkey.trim().is_empty() {
                return Err(ValidationError::EmptyValidatorPubkey);
            }
            if registration.signature.trim().is_empty() {
                return Err(ValidationError::EmptyValidatorSignature);
            }
        }
        Ok(())
    }

    fn validate_submit_block(&self, block_hash: &str) -> Result<(), ValidationError> {
        if block_hash.is_empty() || block_hash == "0x0" {
            return Err(ValidationError::EmptyBlockHash);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::{DefaultRegistrationPolicy, RegistrationPolicy};
    use crate::types::{BuilderApiVersion, RegistrationApiMode, RelayCapability};

    fn capability_strategy() -> impl Strategy<Value = RelayCapability> {
        prop_oneof![
            Just(RelayCapability::Unknown),
            Just(RelayCapability::V1Only),
            Just(RelayCapability::V2Supported),
        ]
    }

    fn mode_strategy() -> impl Strategy<Value = RegistrationApiMode> {
        prop_oneof![
            Just(RegistrationApiMode::Auto),
            Just(RegistrationApiMode::V1),
            Just(RegistrationApiMode::V2),
        ]
    }

    proptest! {
        #[test]
        fn prop_pinned_modes_are_absolute(
            capability in capability_strategy(),
            probe_cache_enabled in any::<bool>(),
        ) {
            let policy = DefaultRegistrationPolicy;
            prop_assert_eq!(
                policy.select_version(RegistrationApiMode::V1, capability, probe_cache_enabled),
                BuilderApiVersion::V1
            );
            prop_assert_eq!(
                policy.select_version(RegistrationApiMode::V2, capability, probe_cache_enabled),
                BuilderApiVersion::V2
            );
        }

        #[test]
        fn prop_auto_without_probe_cache_always_v2(capability in capability_strategy()) {
            let policy = DefaultRegistrationPolicy;
            prop_assert_eq!(
                policy.select_version(RegistrationApiMode::Auto, capability, false),
                BuilderApiVersion::V2
            );
        }

        #[test]
        fn prop_auto_with_probe_cache_uses_capability(capability in capability_strategy()) {
            let policy = DefaultRegistrationPolicy;
            let expected = if matches!(capability, RelayCapability::V1Only) {
                BuilderApiVersion::V1
            } else {
                BuilderApiVersion::V2
            };
            prop_assert_eq!(
                policy.select_version(RegistrationApiMode::Auto, capability, true),
                expected
            );
        }

        #[test]
        fn prop_mapping_is_deterministic(
            mode in mode_strategy(),
            capability in capability_strategy(),
            probe_cache_enabled in any::<bool>(),
        ) {
            let policy = DefaultRegistrationPolicy;
            let first = policy.select_version(mode, capability, probe_cache_enabled);
            let second = policy.select_version(mode, capability, probe_cache_enabled);
            prop_assert_eq!(first, second);
        }
    }
}
