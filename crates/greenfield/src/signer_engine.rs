use std::sync::Arc;

use parking_lot::RwLock;

use crate::types::{ProxyKeyRequest, ProxyKeyResponse, SignatureRequest};

#[derive(Debug, Clone, Default)]
pub struct SignerEngine {
    keys: Arc<RwLock<Vec<String>>>,
}

impl SignerEngine {
    pub fn new(initial_keys: Vec<String>) -> Self {
        Self { keys: Arc::new(RwLock::new(initial_keys)) }
    }

    pub fn get_pubkeys(&self) -> Vec<String> {
        self.keys.read().clone()
    }

    pub fn request_signature(&self, request: SignatureRequest) -> String {
        format!("sig:{}:{}", request.pubkey, request.payload_root)
    }

    pub fn generate_proxy_key(&self, request: ProxyKeyRequest) -> ProxyKeyResponse {
        ProxyKeyResponse {
            delegation: format!("delegate:{}->{}", request.pubkey, request.delegatee),
            signature: "proxy-signature".to_string(),
        }
    }
}
