use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuilderApiVersion {
    V1,
    V2,
}

impl BuilderApiVersion {
    pub fn as_path(self) -> &'static str {
        match self {
            Self::V1 => "/eth/v1/builder",
            Self::V2 => "/eth/v2/builder",
        }
    }

    pub fn as_label(self) -> &'static str {
        match self {
            Self::V1 => "v1",
            Self::V2 => "v2",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointTag {
    GetHeader,
    SubmitBlindedBlock,
    RegisterValidator,
    Status,
    SignerGetPubkeys,
    SignerRequestSignature,
    SignerGenerateProxyKey,
    ManagementStatus,
}

impl EndpointTag {
    pub fn as_label(self) -> &'static str {
        match self {
            Self::GetHeader => "get_header",
            Self::SubmitBlindedBlock => "submit_blinded_block",
            Self::RegisterValidator => "register_validator",
            Self::Status => "status",
            Self::SignerGetPubkeys => "signer_get_pubkeys",
            Self::SignerRequestSignature => "signer_request_signature",
            Self::SignerGenerateProxyKey => "signer_generate_proxy_key",
            Self::ManagementStatus => "management_status",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RegistrationApiMode {
    #[default]
    Auto,
    V1,
    V2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RegistrationMode {
    Any,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FanoutMode {
    AnySuccess,
    AllMustSucceed,
}

impl FanoutMode {
    pub fn as_registration_mode(self) -> RegistrationMode {
        match self {
            Self::AnySuccess => RegistrationMode::Any,
            Self::AllMustSucceed => RegistrationMode::All,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelayCapability {
    Unknown,
    V1Only,
    V2Supported,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayDescriptor {
    pub id: String,
    pub base_url: String,
    #[serde(default)]
    pub registration_api: RegistrationApiMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatorRegistration {
    pub pubkey: String,
    pub fee_recipient: String,
    pub gas_limit: u64,
    pub timestamp: u64,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterValidatorContext {
    pub idempotency_key: String,
    pub source: Option<String>,
    pub mode: RegistrationMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterValidatorV2Request {
    pub registrations: Vec<ValidatorRegistration>,
    pub context: RegisterValidatorContext,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeaderBid {
    pub block_hash: String,
    pub parent_hash: String,
    pub tx_root: String,
    pub value: u64,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureRequest {
    pub pubkey: String,
    pub payload_root: String,
    pub mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyKeyRequest {
    pub pubkey: String,
    pub delegatee: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyKeyResponse {
    pub delegation: String,
    pub signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub status: &'static str,
}
