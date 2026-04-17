use reqwest::Client;
use crate::wechat::crypto;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

/// CDN media handler for downloading/uploading encrypted media.
///
/// The AES key is NOT stored globally — it is provided per-operation
/// because the iLink API may return different keys per message or per session.
pub struct CdnHandler {
    client: Client,
}

impl CdnHandler {
    /// Create a new CdnHandler. No AES key is needed at construction time.
    pub fn new(client: &Client) -> Self {
        Self {
            client: client.clone(),
        }
    }

    /// Download encrypted media from CDN URL and decrypt it with the given key.
    pub async fn download_and_decrypt(&self, url: &str, aes_key: &[u8; 16]) -> Result<Vec<u8>, CdnError> {
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| CdnError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(CdnError::Http(resp.status().to_string()));
        }

        let encrypted = resp
            .bytes()
            .await
            .map_err(|e| CdnError::Network(e.to_string()))?;

        crypto::decrypt_ecb(&encrypted, aes_key).map_err(CdnError::Crypto)
    }

    /// Download encrypted media from a base64-encoded ciphertext string.
    pub async fn download_and_decrypt_b64(
        &self,
        b64_ciphertext: &str,
        aes_key: &[u8; 16],
    ) -> Result<Vec<u8>, CdnError> {
        let ciphertext = BASE64
            .decode(b64_ciphertext)
            .map_err(|e| CdnError::Crypto(format!("base64 decode: {}", e)))?;
        crypto::decrypt_ecb(&ciphertext, aes_key).map_err(CdnError::Crypto)
    }

    /// Upload raw bytes to a CDN URL.
    pub async fn upload(&self, url: &str, data: &[u8]) -> Result<(), CdnError> {
        let resp = self
            .client
            .put(url)
            .body(data.to_vec())
            .send()
            .await
            .map_err(|e| CdnError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(CdnError::Upload(format!("HTTP {}", resp.status())));
        }

        Ok(())
    }

    /// Get a reference to the underlying HTTP client.
    pub fn client(&self) -> &Client {
        &self.client
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CdnError {
    #[error("Network error: {0}")]
    Network(String),
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("Crypto error: {0}")]
    Crypto(String),
    #[error("Upload error: {0}")]
    Upload(String),
}
