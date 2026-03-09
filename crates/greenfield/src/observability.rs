use std::{collections::HashMap, sync::Arc};

use parking_lot::Mutex;
use serde::Serialize;

use crate::types::{BuilderApiVersion, EndpointTag};

pub const TIMEOUT_ERROR_CODE_LABEL: &str = "timeout";
pub const CONNECT_ERROR_CODE_LABEL: &str = "connect";
pub const INTERNAL_ERROR_CODE_LABEL: &str = "internal";

#[derive(Debug, Clone, Default)]
pub struct Observability {
    inner: Arc<Mutex<ObservabilityState>>,
}

#[derive(Debug, Default)]
struct ObservabilityState {
    relay_status_code_total: HashMap<(String, String, String), u64>,
    relay_registration_wire_version_total: HashMap<(String, String), u64>,
    relay_registration_fallback_total: HashMap<(String, String, String, String), u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ObservabilitySnapshot {
    pub relay_status_code_total: HashMap<(String, String, String), u64>,
    pub relay_registration_wire_version_total: HashMap<(String, String), u64>,
    pub relay_registration_fallback_total: HashMap<(String, String, String, String), u64>,
}

impl Observability {
    pub fn record_status_code(&self, relay_id: &str, endpoint: EndpointTag, status_code: &str) {
        let mut guard = self.inner.lock();
        *guard
            .relay_status_code_total
            .entry((relay_id.to_string(), endpoint.as_label().to_string(), status_code.to_string()))
            .or_default() += 1;
    }

    pub fn record_registration_wire_version(&self, relay_id: &str, version: BuilderApiVersion) {
        let mut guard = self.inner.lock();
        *guard
            .relay_registration_wire_version_total
            .entry((relay_id.to_string(), version.as_label().to_string()))
            .or_default() += 1;
    }

    pub fn record_registration_fallback(
        &self,
        relay_id: &str,
        from: BuilderApiVersion,
        to: BuilderApiVersion,
        reason: &str,
    ) {
        let mut guard = self.inner.lock();
        *guard
            .relay_registration_fallback_total
            .entry((
                relay_id.to_string(),
                from.as_label().to_string(),
                to.as_label().to_string(),
                reason.to_string(),
            ))
            .or_default() += 1;
    }

    pub fn snapshot(&self) -> ObservabilitySnapshot {
        let guard = self.inner.lock();
        ObservabilitySnapshot {
            relay_status_code_total: guard.relay_status_code_total.clone(),
            relay_registration_wire_version_total: guard
                .relay_registration_wire_version_total
                .clone(),
            relay_registration_fallback_total: guard.relay_registration_fallback_total.clone(),
        }
    }
}
