//! Metrics for PBS module
//! We collect two types of metrics within the PBS module:
//! - what PBS receives from relays
//! - what PBS returns to the beacon node

use lazy_static::lazy_static;
use prometheus::{
    HistogramVec, IntCounterVec, IntGaugeVec, Registry, register_histogram_vec_with_registry,
    register_int_counter_vec_with_registry, register_int_gauge_vec_with_registry,
};

lazy_static! {
    pub static ref PBS_METRICS_REGISTRY: Registry =
        Registry::new_custom(Some("cb_pbs".to_string()), None).unwrap();

    // FROM RELAYS
    /// Status code received by relay by endpoint
    pub static ref RELAY_STATUS_CODE: IntCounterVec = register_int_counter_vec_with_registry!(
        "relay_status_code_total",
        "HTTP status code received by relay",
        &["http_status_code", "endpoint", "relay_id"],
        PBS_METRICS_REGISTRY
    )
    .unwrap();

    /// Latency by relay by endpoint
    pub static ref RELAY_LATENCY: HistogramVec = register_histogram_vec_with_registry!(
        "relay_latency",
        "HTTP latency by relay",
        &["endpoint", "relay_id"],
        PBS_METRICS_REGISTRY
    )
    .unwrap();

    /// Registration wire version used per relay
    pub static ref RELAY_REGISTRATION_WIRE_VERSION: IntCounterVec =
        register_int_counter_vec_with_registry!(
            "relay_registration_wire_version_total",
            "Registration wire version attempts by relay",
            &["relay_id", "version"],
            PBS_METRICS_REGISTRY
        )
        .unwrap();

    /// Registration fallback events by relay
    pub static ref RELAY_REGISTRATION_FALLBACK: IntCounterVec =
        register_int_counter_vec_with_registry!(
            "relay_registration_fallback_total",
            "Registration wire version fallback events by relay",
            &["relay_id", "from", "to", "reason"],
            PBS_METRICS_REGISTRY
        )
        .unwrap();

    /// Number of active outbound registration requests per relay
    pub static ref RELAY_REGISTRATION_INFLIGHT: IntGaugeVec =
        register_int_gauge_vec_with_registry!(
            "relay_registration_inflight",
            "Active outbound validator registration requests by relay",
            &["relay_id"],
            PBS_METRICS_REGISTRY
        )
        .unwrap();

    /// Latest slot for which relay delivered a header
    pub static ref RELAY_LAST_SLOT: IntGaugeVec = register_int_gauge_vec_with_registry!(
        "relay_last_slot",
        "Latest slot for which relay delivered a header",
        &["relay_id"],
        PBS_METRICS_REGISTRY
    )
    .unwrap();

    /// Latest slot for which relay delivered a header
    // Don't store slot number to avoid creating high cardinality, if needed can just aggregate for 12sec
    pub static ref RELAY_HEADER_VALUE: IntGaugeVec = register_int_gauge_vec_with_registry!(
        "relay_header_value",
        "Header value in gwei delivered by relay",
        &["relay_id"],
        PBS_METRICS_REGISTRY
    )
    .unwrap();


    // TO BEACON NODE
    /// Status code returned to beacon node by endpoint
    pub static ref BEACON_NODE_STATUS: IntCounterVec = register_int_counter_vec_with_registry!(
        "beacon_node_status_code_total",
        "HTTP status code returned to beacon node",
        &["http_status_code", "endpoint"],
        PBS_METRICS_REGISTRY
    ).unwrap();
}
