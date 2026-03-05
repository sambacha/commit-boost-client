use alloy::{primitives::B256, rpc::types::beacon::relay::ValidatorRegistration};
use cb_common::{
    pbs::{
        BuilderApiVersion, RegisterValidatorContext, RegisterValidatorV2Request, RegistrationMode,
        RelayClient, SignedBlindedBeaconBlock,
    },
    types::BlsPublicKey,
    utils::bls_pubkey_from_hex,
};
use reqwest::{Response, StatusCode};
use std::sync::atomic::{AtomicU8, Ordering};
use uuid::Uuid;

use crate::utils::generate_mock_relay;

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ValidatorRegistrationApiMode {
    V1,
    V2,
    Auto,
}

#[derive(Debug, Clone)]
pub struct ValidatorRegistrationRequest {
    pub registrations: Vec<ValidatorRegistration>,
    pub api_mode: ValidatorRegistrationApiMode,
    pub context: Option<RegisterValidatorContext>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum CachedRelayRegistrationCapability {
    Unknown = 0,
    V1Only = 1,
    V2Supported = 2,
}

impl CachedRelayRegistrationCapability {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::V1Only,
            2 => Self::V2Supported,
            _ => Self::Unknown,
        }
    }
}

pub struct MockValidator {
    pub comm_boost: RelayClient,
    registration_capability: AtomicU8,
}

impl MockValidator {
    pub fn new(port: u16) -> eyre::Result<Self> {
        let pubkey = bls_pubkey_from_hex(
            "0xac6e77dfe25ecd6110b8e780608cce0dab71fdd5ebea22a16c0205200f2f8e2e3ad3b71d3499c54ad14d6c21b41a37ae",
        )?;
        Ok(Self {
            comm_boost: generate_mock_relay(port, pubkey)?,
            registration_capability: AtomicU8::new(
                CachedRelayRegistrationCapability::Unknown as u8,
            ),
        })
    }

    pub async fn do_get_header(&self, pubkey: Option<BlsPublicKey>) -> eyre::Result<Response> {
        let default_pubkey = bls_pubkey_from_hex(
            "0xac6e77dfe25ecd6110b8e780608cce0dab71fdd5ebea22a16c0205200f2f8e2e3ad3b71d3499c54ad14d6c21b41a37ae",
        )?;
        let url =
            self.comm_boost.get_header_url(0, &B256::ZERO, &pubkey.unwrap_or(default_pubkey))?;
        Ok(self.comm_boost.client.get(url).send().await?)
    }

    pub async fn do_get_status(&self) -> eyre::Result<Response> {
        let url = self.comm_boost.get_status_url()?;
        Ok(self.comm_boost.client.get(url).send().await?)
    }

    pub async fn do_register_validator(&self) -> eyre::Result<Response> {
        self.do_register_custom_validators(vec![]).await
    }

    pub async fn do_register_validators(
        &self,
        request: ValidatorRegistrationRequest,
    ) -> eyre::Result<Response> {
        let ValidatorRegistrationRequest { registrations, api_mode, context } = request;
        let initial_version = self.initial_registration_api_version(api_mode);
        if matches!(initial_version, BuilderApiVersion::V1) {
            return self.send_register_validator_v1(registrations).await;
        }

        let v2_context = context.unwrap_or_else(Self::default_v2_context);
        let v2_response =
            self.send_register_validator_v2(registrations.clone(), v2_context).await?;
        if v2_response.status().is_success() {
            self.set_registration_capability(CachedRelayRegistrationCapability::V2Supported);
            return Ok(v2_response);
        }

        // 404 is the only downgrade signal: relay lacks v2 endpoint support.
        if matches!(api_mode, ValidatorRegistrationApiMode::Auto)
            && v2_response.status() == StatusCode::NOT_FOUND
        {
            self.set_registration_capability(CachedRelayRegistrationCapability::V1Only);
            return self.send_register_validator_v1(registrations).await;
        }

        Ok(v2_response)
    }

    pub async fn do_register_custom_validators(
        &self,
        registrations: Vec<ValidatorRegistration>,
    ) -> eyre::Result<Response> {
        self.do_register_validators(ValidatorRegistrationRequest {
            registrations,
            api_mode: ValidatorRegistrationApiMode::V1,
            context: None,
        })
        .await
    }

    pub async fn do_register_custom_validators_auto(
        &self,
        registrations: Vec<ValidatorRegistration>,
    ) -> eyre::Result<Response> {
        self.do_register_validators(ValidatorRegistrationRequest {
            registrations,
            api_mode: ValidatorRegistrationApiMode::Auto,
            context: None,
        })
        .await
    }

    pub async fn do_register_custom_validators_v2(
        &self,
        registrations: Vec<ValidatorRegistration>,
    ) -> eyre::Result<Response> {
        self.do_register_validators(ValidatorRegistrationRequest {
            registrations,
            api_mode: ValidatorRegistrationApiMode::V2,
            context: None,
        })
        .await
    }

    pub async fn do_submit_block_v1(
        &self,
        signed_blinded_block: Option<SignedBlindedBeaconBlock>,
    ) -> eyre::Result<Response> {
        self.do_submit_block_impl(signed_blinded_block, BuilderApiVersion::V1).await
    }

    pub async fn do_submit_block_v2(
        &self,
        signed_blinded_block: Option<SignedBlindedBeaconBlock>,
    ) -> eyre::Result<Response> {
        self.do_submit_block_impl(signed_blinded_block, BuilderApiVersion::V2).await
    }

    async fn do_submit_block_impl(
        &self,
        signed_blinded_block: Option<SignedBlindedBeaconBlock>,
        api_version: BuilderApiVersion,
    ) -> eyre::Result<Response> {
        let url = self.comm_boost.submit_block_url(api_version)?;

        let signed_blinded_block =
            signed_blinded_block.unwrap_or_else(load_test_signed_blinded_block);

        Ok(self.comm_boost.client.post(url).json(&signed_blinded_block).send().await?)
    }

    async fn send_register_validator_v1(
        &self,
        registrations: Vec<ValidatorRegistration>,
    ) -> eyre::Result<Response> {
        let url = self.comm_boost.register_validator_url(BuilderApiVersion::V1)?;
        self.comm_boost.client.post(url).json(&registrations).send().await.map_err(Into::into)
    }

    async fn send_register_validator_v2(
        &self,
        registrations: Vec<ValidatorRegistration>,
        context: RegisterValidatorContext,
    ) -> eyre::Result<Response> {
        let url = self.comm_boost.register_validator_url(BuilderApiVersion::V2)?;
        let body = RegisterValidatorV2Request { registrations, context };
        self.comm_boost.client.post(url).json(&body).send().await.map_err(Into::into)
    }

    // This cache is runtime-only and scoped to a MockValidator instance.
    fn initial_registration_api_version(
        &self,
        api_mode: ValidatorRegistrationApiMode,
    ) -> BuilderApiVersion {
        match api_mode {
            ValidatorRegistrationApiMode::V1 => BuilderApiVersion::V1,
            ValidatorRegistrationApiMode::V2 => BuilderApiVersion::V2,
            ValidatorRegistrationApiMode::Auto => match self.registration_capability() {
                CachedRelayRegistrationCapability::V1Only => BuilderApiVersion::V1,
                CachedRelayRegistrationCapability::Unknown
                | CachedRelayRegistrationCapability::V2Supported => BuilderApiVersion::V2,
            },
        }
    }

    fn registration_capability(&self) -> CachedRelayRegistrationCapability {
        CachedRelayRegistrationCapability::from_u8(
            self.registration_capability.load(Ordering::Relaxed),
        )
    }

    fn set_registration_capability(&self, capability: CachedRelayRegistrationCapability) {
        self.registration_capability.store(capability as u8, Ordering::Relaxed);
    }

    fn default_v2_context() -> RegisterValidatorContext {
        RegisterValidatorContext {
            idempotency_key: Uuid::now_v7().to_string(),
            source: Some("mock-validator".to_string()),
            mode: RegistrationMode::All,
        }
    }
}

pub fn load_test_signed_blinded_block() -> SignedBlindedBeaconBlock {
    let data_json = include_str!(
        "../../crates/common/src/pbs/types/testdata/signed-blinded-beacon-block-electra-2.json"
    );
    serde_json::from_str(data_json).unwrap()
}
