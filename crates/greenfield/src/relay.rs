use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use reqwest::{Client, Method, StatusCode};
use serde::de::DeserializeOwned;

use crate::{
    policies::RelayError,
    types::{BuilderApiVersion, EndpointTag, RelayDescriptor},
};

#[derive(Debug, Clone)]
pub struct RelayRequest {
    pub endpoint: EndpointTag,
    pub method: Method,
    pub version: BuilderApiVersion,
    pub path_suffix: String,
    pub body: Option<Bytes>,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct RelayResponse {
    pub status: u16,
    pub body: Bytes,
}

impl RelayResponse {
    pub fn is_success(&self) -> bool {
        (200..=299).contains(&self.status)
    }

    pub fn json<T: DeserializeOwned>(&self) -> Result<T, RelayError> {
        serde_json::from_slice(&self.body).map_err(|_| RelayError::Decode)
    }
}

#[async_trait]
pub trait RelayTransport: Send + Sync {
    async fn send(
        &self,
        relay: &RelayDescriptor,
        request: RelayRequest,
        timeout: Duration,
    ) -> Result<RelayResponse, RelayError>;
}

#[derive(Debug, Clone)]
pub struct ReqwestRelayTransport {
    client: Client,
}

impl ReqwestRelayTransport {
    pub fn new() -> Self {
        Self { client: Client::new() }
    }

    fn build_url(&self, relay: &RelayDescriptor, request: &RelayRequest) -> String {
        format!(
            "{}{}{}",
            relay.base_url.trim_end_matches('/'),
            request.version.as_path(),
            request.path_suffix
        )
    }
}

#[async_trait]
impl RelayTransport for ReqwestRelayTransport {
    async fn send(
        &self,
        relay: &RelayDescriptor,
        request: RelayRequest,
        timeout: Duration,
    ) -> Result<RelayResponse, RelayError> {
        let url = self.build_url(relay, &request);
        let mut request_builder = self
            .client
            .request(request.method, &url)
            .timeout(timeout)
            .header("content-type", "application/json");

        for (name, value) in &request.headers {
            request_builder = request_builder.header(name, value);
        }

        if let Some(body) = request.body {
            request_builder = request_builder.body(body);
        }

        let response = request_builder.send().await.map_err(|err| {
            if err.is_timeout() {
                RelayError::Timeout
            } else if err.is_connect() {
                RelayError::Connect
            } else {
                RelayError::Internal(err.to_string())
            }
        })?;

        let status = response.status();
        let body = response.bytes().await.map_err(|err| RelayError::Internal(err.to_string()))?;

        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
            return Err(RelayError::HttpStatus(status.as_u16()));
        }

        Ok(RelayResponse { status: status.as_u16(), body })
    }
}
