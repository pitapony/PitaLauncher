//! HTTP transport, kept behind a trait so the fetch loop is testable offline.

use std::future::Future;

use crate::{Error, Result};

/// How the metadata client talks to a store.
///
/// The only production implementation is [`ReqwestTransport`]; tests inject a
/// fake that returns fixture bodies and records the requested URLs.
pub trait Transport: Send + Sync {
    /// Fetch a URL and return the response body as text.
    fn get_text(&self, url: &str) -> impl Future<Output = Result<String>> + Send;
}

/// `reqwest` with rustls and transparent gzip.
///
/// Steam serves gzip even when the client did not ask for it, so the `gzip`
/// feature is not optional: without it the parser would see compressed bytes.
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("pitagames/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(30))
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(Error::Client)?;
        Ok(Self { client })
    }
}

impl Transport for ReqwestTransport {
    async fn get_text(&self, url: &str) -> Result<String> {
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|source| Error::Http {
                url: url.to_string(),
                source,
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(Error::Status {
                status: status.as_u16(),
                url: url.to_string(),
            });
        }

        response.text().await.map_err(|source| Error::Http {
            url: url.to_string(),
            source,
        })
    }
}
