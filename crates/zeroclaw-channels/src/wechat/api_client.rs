use crate::wechat::api_types::*;
use crate::wechat::config::WechatConfig;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// Shared mutable state for the API client.
#[derive(Debug, Clone, Default)]
pub struct ApiState {
    /// Bot token obtained from QR login.
    pub token: Option<String>,
    /// Bot's own wxid (filled after login/getconfig).
    pub bot_wxid: Option<String>,
    /// Bot's display name.
    pub bot_nickname: Option<String>,
    /// AES key for message decryption (may come from getconfig).
    pub aes_key: Option<String>,
    /// Polling cursor.
    pub get_updates_buf: Option<String>,
    /// Typing ticket from getconfig.
    pub typing_ticket: Option<String>,
    /// Per-user context tokens (keyed by ilink_user_id).
    /// Updated on each inbound message; used when sending replies.
    pub context_tokens: HashMap<String, String>,
}

/// HTTP client for the iLink Bot API.
#[derive(Clone)]
pub struct ApiClient {
    config: WechatConfig,
    http: Client,
    state: Arc<RwLock<ApiState>>,
}

// ---------------------------------------------------------------------------
// ApiResponse — unified response wrapper
// ---------------------------------------------------------------------------

/// Unified API response wrapper that holds the raw JSON and provides typed access.
pub struct ApiResponse {
    raw: Value,
}

impl ApiResponse {
    pub fn raw(&self) -> &Value {
        &self.raw
    }

    /// Try to extract a typed value from the "data" field of the response.
    fn get<T: DeserializeOwned>(&self) -> Result<T, ApiError> {
        let data = self.raw.get("data").ok_or_else(|| {
            ApiError::Parse("response missing `data` field".to_string())
        })?;
        serde_json::from_value(data.clone())
            .map_err(|e| ApiError::Parse(format!("deserialize data: {e}")))
    }

    /// Extract the `ret` code (0 = success).
    fn ret_code(&self) -> i64 {
        self.raw.get("ret").and_then(|v| v.as_i64()).unwrap_or(0)
    }

    /// Extract the `errmsg` field.
    fn errmsg(&self) -> String {
        self.raw
            .get("errmsg")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }
}

// ---------------------------------------------------------------------------
// ApiClient implementation
// ---------------------------------------------------------------------------

impl ApiClient {
    pub fn new(config: WechatConfig) -> Self {
        let poll_timeout = config.poll_timeout as u64;
        let http = Client::builder()
            .user_agent(&config.user_agent)
            .timeout(Duration::from_secs(poll_timeout + 15))
            .build()
            .expect("failed to build HTTP client");

        let mut state = ApiState::default();
        if let Some(ref token) = config.bot_token {
            tracing::info!("WeChat: loading bot_token from config");
            state.token = Some(token.clone());
        }

        Self {
            config,
            http,
            state: Arc::new(RwLock::new(state)),
        }
    }

    /// Get a reference to the underlying HTTP client (for CDN etc.).
    pub fn http(&self) -> &Client {
        &self.http
    }

    /// Access the shared API state.
    pub fn state(&self) -> Arc<RwLock<ApiState>> {
        self.state.clone()
    }

    /// Generate X-WECHAT-UIN header value: base64(decimal string of random u32).
    fn random_uin_header() -> String {
        let uin: u32 = rand::random();
        BASE64.encode(uin.to_string())
    }

    /// Build a full URL from a relative endpoint path.
    fn url(&self, endpoint: &str) -> String {
        format!(
            "{}/{}",
            self.config.api_base.trim_end_matches('/'),
            endpoint
        )
    }

    // -----------------------------------------------------------------------
    // Auth helpers
    // -----------------------------------------------------------------------

    /// Set the auth token (after QR login).
    pub async fn set_token(&self, token: String) {
        self.state.write().await.token = Some(token);
    }

    /// Get a clone of the current token.
    pub async fn token(&self) -> Option<String> {
        self.state.read().await.token.clone()
    }

    // -----------------------------------------------------------------------
    // Low-level HTTP helpers with iLink headers
    // -----------------------------------------------------------------------

    /// POST JSON request with iLink auth headers.
    /// Auth headers are only sent when `bot_token` is `Some`.
    async fn api_post(
        &self,
        endpoint: &str,
        body: &Value,
        bot_token: Option<&str>,
    ) -> Result<ApiResponse, ApiError> {
        let mut req = self.http.post(self.url(endpoint));

        // Auth headers only when token is provided
        if let Some(t) = bot_token {
            if !t.is_empty() {
                req = req.header("Authorization", format!("Bearer {}", t));
                req = req.header("AuthorizationType", "ilink_bot_token");
            }
        }
        req = req.header("X-WECHAT-UIN", Self::random_uin_header());
        req = req.header("iLink-App-Id", "bot");
        req = req.header("iLink-App-ClientVersion", "65547");

        let resp = req
            .json(body)
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ApiError::Http(status.as_u16(), text));
        }

        let raw: Value = resp
            .json()
            .await
            .map_err(|e| ApiError::Parse(e.to_string()))?;

        Ok(ApiResponse { raw })
    }

    /// GET request with optional auth token and required iLink headers.
    /// Uses ClientVersion `65547` (unified with `api_post`).
    async fn api_get(
        &self,
        endpoint: &str,
        bot_token: Option<&str>,
    ) -> Result<ApiResponse, ApiError> {
        let mut req = self.http.get(self.url(endpoint));

        // Auth headers only when token is provided
        if let Some(t) = bot_token {
            if !t.is_empty() {
                req = req.header("Authorization", format!("Bearer {}", t));
                req = req.header("AuthorizationType", "ilink_bot_token");
            }
        }
        req = req.header("X-WECHAT-UIN", Self::random_uin_header());
        req = req.header("iLink-App-Id", "bot");
        req = req.header("iLink-App-ClientVersion", "65547");

        let resp = req
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ApiError::Http(status.as_u16(), text));
        }

        let raw: Value = resp
            .json()
            .await
            .map_err(|e| ApiError::Parse(e.to_string()))?;

        Ok(ApiResponse { raw })
    }

    // -----------------------------------------------------------------------
    // check_ret — unified API error checking
    // -----------------------------------------------------------------------

    /// Check the `ret` code in a response. Returns `Err(ApiError::Api)` if non-zero.
    fn check_ret(&self, resp: &ApiResponse) -> Result<(), ApiError> {
        let ret = resp.ret_code();
        if ret != 0 {
            let msg = resp.errmsg();
            return Err(ApiError::Api(ret, msg));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // High-level API methods
    // -----------------------------------------------------------------------

    /// Fetch bot configuration (wxid, nickname, typing_ticket, aeskey).
    /// Must be called per-user with the message sender's `ilink_user_id` and `context_token`
    /// from the inbound message.
    pub async fn get_config(
        &self,
        ilink_user_id: &str,
        context_token: Option<&str>,
    ) -> Result<GetConfigResponse, ApiError> {
        let token = self.token().await;
        let token_str = token.as_deref();

        let mut body = serde_json::json!({
            "base_info": BaseInfo::default(),
            "ilink_user_id": ilink_user_id,
        });
        if let Some(ct) = context_token {
            body["context_token"] = serde_json::Value::String(ct.to_string());
        }

        let resp = self
            .api_post("ilink/bot/getconfig", &body, token_str)
            .await?;
        self.check_ret(&resp)?;

        let cfg: GetConfigResponse = resp.get()?;

        // Store derived fields in state
        {
            let mut st = self.state.write().await;
            if !cfg.wxid.is_empty() {
                st.bot_wxid = Some(cfg.wxid.clone());
            }
            if !cfg.nickname.is_empty() {
                st.bot_nickname = Some(cfg.nickname.clone());
            }
            if !cfg.aeskey.is_empty() {
                st.aes_key = Some(cfg.aeskey.clone());
            }
            if !cfg.typing_ticket.is_empty() {
                st.typing_ticket = Some(cfg.typing_ticket.clone());
            }
        }

        Ok(cfg)
    }

    /// Long-poll for new messages.
    pub async fn get_updates(&self) -> Result<GetUpdatesResponse, ApiError> {
        let token = self.token().await;
        let token_str = token.as_deref();

        let st = self.state.read().await;
        let buf = st.get_updates_buf.clone().unwrap_or_default();
        drop(st);

        let req_body = GetUpdatesRequest {
            get_updates_buf: buf,
            base_info: BaseInfo::default(),
        };
        let resp = self
            .api_post("ilink/bot/getupdates", &serde_json::to_value(&req_body).unwrap(), token_str)
            .await?;

        // Update sync buffer from response
        if let Some(new_buf) = resp.raw().get("get_updates_buf").and_then(|v| v.as_str()) {
            self.state.write().await.get_updates_buf = Some(new_buf.to_string());
        }

        resp.get()
    }

    /// Send a text message.
    pub async fn send_text(
        &self,
        to_user_id: &str,
        text: &str,
        context_token: Option<&str>,
        reply_client_id: Option<&str>,
    ) -> Result<serde_json::Value, ApiError> {
        let token = self.token().await;
        let token_str = token.as_deref();

        let client_id = reply_client_id
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("openclaw-weixin_{}", uuid::Uuid::new_v4()));

        let req = SendMessageRequest {
            base_info: BaseInfo::default(),
            msg: SendMessageMsg {
                from_user_id: String::new(),
                to_user_id: to_user_id.to_string(),
                client_id,
                message_type: MESSAGE_TYPE_BOT,
                message_state: MESSAGE_STATE_FINISH,
                item_list: Some(vec![SendMessageItem {
                    item_type: ITEM_TYPE_TEXT,
                    text_item: Some(SendTextItem {
                        text: text.to_string(),
                    }),
                    file_item: None,
                }]),
                context_token: context_token.map(|s| s.to_string()),
            },
        };
        let resp = self
            .api_post(
                "ilink/bot/sendmessage",
                &serde_json::to_value(&req).unwrap(),
                token_str,
            )
            .await?;
        Ok(resp.raw().clone())
    }

    /// Send typing indicator.
    pub async fn send_typing(
        &self,
        to_user_id: &str,
        typing: bool,
    ) -> Result<serde_json::Value, ApiError> {
        let token = self.token().await;
        let token_str = token.as_deref();

        let st = self.state.read().await;
        let typing_ticket = st.typing_ticket.clone().unwrap_or_default();
        drop(st);

        let req = SendTypingRequest {
            ilink_user_id: to_user_id.to_string(),
            typing_ticket,
            status: if typing {
                TYPING_STATUS_TYPING
            } else {
                TYPING_STATUS_CANCEL
            },
            base_info: BaseInfo::default(),
        };
        let resp = self
            .api_post(
                "ilink/bot/sendtyping",
                &serde_json::to_value(&req).unwrap(),
                token_str,
            )
            .await?;
        Ok(resp.raw().clone())
    }

    /// Get upload URL for media.
    pub async fn get_upload_url(
        &self,
        filekey: &str,
        media_type: i64,
        to_user_id: &str,
        rawsize: i64,
        rawfilemd5: &str,
        filesize: i64,
        aeskey: &str,
    ) -> Result<GetUploadUrlResponse, ApiError> {
        let token = self.token().await;
        let token_str = token.as_deref();

        let req = GetUploadUrlRequest {
            filekey: filekey.to_string(),
            media_type,
            to_user_id: to_user_id.to_string(),
            rawsize,
            rawfilemd5: rawfilemd5.to_string(),
            filesize,
            thumb_rawsize: None,
            thumb_rawfilemd5: None,
            thumb_filesize: None,
            no_need_thumb: None,
            aeskey: aeskey.to_string(),
            base_info: BaseInfo::default(),
        };
        let resp = self
            .api_post(
                "ilink/bot/getuploadurl",
                &serde_json::to_value(&req).unwrap(),
                token_str,
            )
            .await?;
        self.check_ret(&resp)?;
        resp.get()
    }

    /// Store a context token for a user (called on each inbound message).
    pub async fn store_context_token(&self, user_id: &str, token: &str) {
        if !token.is_empty() {
            self.state
                .write()
                .await
                .context_tokens
                .insert(user_id.to_string(), token.to_string());
        }
    }

    /// Retrieve the stored context token for a user.
    pub async fn get_context_token(&self, user_id: &str) -> Option<String> {
        self.state.read().await.context_tokens.get(user_id).cloned()
    }

    /// Send a file message via iLink Bot API.
    pub async fn send_file_message(
        &self,
        to_user_id: &str,
        filename: &str,
        filesize: i64,
        upload_param: &str,
        aes_key_b64: &str,
        context_token: Option<&str>,
    ) -> Result<serde_json::Value, ApiError> {
        let token = self.token().await;
        let token_str = token.as_deref();

        let client_id = format!("openclaw-weixin_{}", uuid::Uuid::new_v4());

        let req = SendMessageRequest {
            base_info: BaseInfo::default(),
            msg: SendMessageMsg {
                from_user_id: String::new(),
                to_user_id: to_user_id.to_string(),
                client_id,
                message_type: MESSAGE_TYPE_BOT,
                message_state: MESSAGE_STATE_FINISH,
                item_list: Some(vec![SendMessageItem {
                    item_type: ITEM_TYPE_FILE,
                    text_item: None,
                    file_item: Some(SendFileItem {
                        media: SendMediaInfo {
                            encrypt_query_param: upload_param.to_string(),
                            aes_key: aes_key_b64.to_string(),
                            encrypt_type: 1,
                        },
                        file_name: filename.to_string(),
                        len: filesize.to_string(),
                    }),
                }]),
                context_token: context_token.map(|s| s.to_string()),
            },
        };

        let resp = self
            .api_post(
                "ilink/bot/sendmessage",
                &serde_json::to_value(&req).unwrap(),
                token_str,
            )
            .await?;
        Ok(resp.raw().clone())
    }

    /// Start QR login flow — get QR code token.
    /// Uses GET request matching openclaw-weixin behavior.
    pub async fn get_bot_qrcode(&self) -> Result<QrCodeResponse, ApiError> {
        let resp = self
            .api_get("ilink/bot/get_bot_qrcode?bot_type=3", None)
            .await?;

        if let Some(code) = resp.raw().get("ret").and_then(|v| v.as_i64()) {
            if code != 0 {
                let msg = resp.raw().get("errmsg").and_then(|v| v.as_str()).unwrap_or("unknown");
                return Err(ApiError::Api(code, msg.to_string()));
            }
        }

        resp.get()
    }

    /// Poll QR login status.
    /// Uses GET request matching openclaw-weixin behavior.
    pub async fn get_qrcode_status(&self, qrcode: &str) -> Result<QrStatus, ApiError> {
        let endpoint = format!(
            "ilink/bot/get_qrcode_status?qrcode={}",
            urlencoding::encode(qrcode)
        );
        let resp = self
            .api_get(&endpoint, None)
            .await?;

        if let Some(code) = resp.raw().get("ret").and_then(|v| v.as_i64()) {
            if code != 0 {
                let msg = resp.raw().get("errmsg").and_then(|v| v.as_str()).unwrap_or("unknown");
                return Err(ApiError::Api(code, msg.to_string()));
            }
        }

        resp.get()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("Network error: {0}")]
    Network(String),
    #[error("HTTP {0}: {1}")]
    Http(u16, String),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("API error {0}: {1}")]
    Api(i64, String),
}
