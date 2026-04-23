//! WeChat iLink Bot channel implementation.
//!
//! Connects to the WeChat iLink Bot API for sending/receiving messages,
//! QR login, typing indicators, and CDN media upload/download.

use aes::Aes128;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use ecb::cipher::{BlockDecryptMut, BlockEncryptMut, KeyInit};
use ecb::{Decryptor, Encryptor};
use md5::Digest as _;
use reqwest::Client;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use zeroclaw_api::channel::{Channel, ChannelMessage, SendMessage};
use zeroclaw_config::schema::WechatConfig;

// ═══════════════════════════════════════════════════════════════════════════
// Constants
// ═══════════════════════════════════════════════════════════════════════════

/// Channel version — must match openclaw-weixin reference.
const CHANNEL_VERSION: &str = "2.1.7";
const ILINK_APP_ID: &str = "bot";
const QR_POLL_INTERVAL_SECS: u64 = 3;
const QR_MAX_ATTEMPTS: u64 = 60;
const RATE_LIMIT_PAUSE_SECS: u64 = 3600;
#[allow(dead_code)]
const TYPING_KEEPALIVE_SECS: u64 = 4;
/// Default backoff on consecutive errors.
#[allow(dead_code)]
const ERROR_BACKOFF_SECS: u64 = 5;
/// Max consecutive errors before re-login.
const MAX_CONSECUTIVE_ERRORS: u32 = 10;

// iLink message type constants
const MESSAGE_TYPE_BOT: i64 = 2;
const MESSAGE_STATE_FINISH: i64 = 2;
const ITEM_TYPE_TEXT: i64 = 1;
const ITEM_TYPE_IMAGE: i64 = 2;
const ITEM_TYPE_VOICE: i64 = 3;
const ITEM_TYPE_FILE: i64 = 4;
const ITEM_TYPE_VIDEO: i64 = 5;
const ITEM_TYPE_LINK: i64 = 6;
const TYPING_STATUS_TYPING: i64 = 1;
const TYPING_STATUS_CANCEL: i64 = 2;

// ═══════════════════════════════════════════════════════════════════════════
// Crypto helpers (AES-128-ECB)
// ═══════════════════════════════════════════════════════════════════════════

fn encrypt_ecb(plaintext: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let padded = pkcs7_pad(plaintext, 16);
    let mut enc = Encryptor::<Aes128>::new(key.into());
    padded
        .chunks(16)
        .flat_map(|chunk| {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            enc.encrypt_block_mut(&mut block.into());
            block.to_vec()
        })
        .collect()
}

#[allow(dead_code)]
fn decrypt_ecb(ciphertext: &[u8], key: &[u8; 16]) -> Result<Vec<u8>, String> {
    if ciphertext.len() % 16 != 0 {
        return Err("Ciphertext length is not a multiple of 16".into());
    }
    let mut dec = Decryptor::<Aes128>::new(key.into());
    let decrypted: Vec<u8> = ciphertext
        .chunks(16)
        .flat_map(|chunk| {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            dec.decrypt_block_mut(&mut block.into());
            block.to_vec()
        })
        .collect();
    pkcs7_unpad(&decrypted)
}

fn pkcs7_pad(data: &[u8], block_size: usize) -> Vec<u8> {
    let padding = block_size - (data.len() % block_size);
    let mut padded = data.to_vec();
    padded.extend(vec![padding as u8; padding]);
    padded
}

fn pkcs7_unpad(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        return Err("Empty data".into());
    }
    let pad_len = *data.last().unwrap() as usize;
    if pad_len == 0 || pad_len > data.len() {
        return Err("Invalid padding".into());
    }
    if data[data.len() - pad_len..].iter().any(|&b| b != pad_len as u8) {
        return Err("Invalid PKCS7 padding".into());
    }
    Ok(data[..data.len() - pad_len].to_vec())
}

/// Compute AES-ECB padded ciphertext size.
fn aes_ecb_padded_size(raw_size: usize) -> usize {
    (raw_size / 16 + 1) * 16
}

// ═══════════════════════════════════════════════════════════════════════════
// API types
// ═══════════════════════════════════════════════════════════════════════════

/// Build the `base_info` payload included in every API request.
fn build_base_info() -> BaseInfo {
    BaseInfo {
        channel_version: CHANNEL_VERSION.to_string(),
    }
}

/// Encode version string as iLink ClientVersion uint32.
/// Format: 0x00MMNNPP = major<<16 | minor<<8 | patch
fn build_client_version() -> u32 {
    let parts: Vec<u32> = CHANNEL_VERSION
        .split('.')
        .filter_map(|p| p.parse().ok())
        .collect();
    let major = parts.first().copied().unwrap_or(0);
    let minor = parts.get(1).copied().unwrap_or(0);
    let patch = parts.get(2).copied().unwrap_or(0);
    ((major & 0xff) << 16) | ((minor & 0xff) << 8) | (patch & 0xff)
}

#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)]
struct BaseInfo {
    channel_version: String,
}

/// Default CDN base URL for media upload.
const DEFAULT_CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";

// ── Inbound message types ──────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct IlinkMessage {
    #[serde(default)]
    seq: i64,
    #[serde(default)]
    message_id: i64,
    #[serde(default)]
    from_user_id: String,
    #[serde(default)]
    to_user_id: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    create_time_ms: i64,
    #[serde(default)]
    update_time_ms: i64,
    #[serde(default)]
    delete_time_ms: i64,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    group_id: String,
    #[serde(default)]
    message_type: i64,
    #[serde(default)]
    message_state: i64,
    #[serde(default)]
    item_list: Vec<MessageItem>,
    #[serde(default)]
    context_token: String,
}

impl IlinkMessage {
    fn chat_id(&self) -> &str {
        if self.group_id.is_empty() {
            &self.from_user_id
        } else {
            &self.group_id
        }
    }

    fn is_group(&self) -> bool {
        !self.group_id.is_empty()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct MessageItem {
    #[serde(default, rename = "type")]
    item_type: i64,
    #[serde(default)]
    text_item: Option<TextItem>,
    #[serde(default)]
    image_item: Option<ImageItem>,
    #[serde(default)]
    file_item: Option<FileItem>,
    #[serde(default)]
    voice_item: Option<VoiceItem>,
    #[serde(default)]
    video_item: Option<VideoItem>,
    #[serde(default)]
    link_item: Option<LinkItem>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct TextItem {
    #[serde(default)]
    text: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ImageItem {
    #[serde(default)]
    media: Option<MediaInfo>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct FileItem {
    #[serde(default)]
    file_name: String,
    #[serde(default)]
    len: String,
    #[serde(default)]
    media: Option<MediaInfo>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct VoiceItem {
    #[serde(default)]
    media: Option<MediaInfo>,
    #[serde(default)]
    duration: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct VideoItem {
    #[serde(default)]
    media: Option<MediaInfo>,
    #[serde(default)]
    duration: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct LinkItem {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct MediaInfo {
    #[serde(default)]
    aes_key: String,
    #[serde(default, rename = "encrypt_query_param")]
    encrypt_query_param: String,
    #[serde(default)]
    encrypt_type: i64,
}

// ── Outbound request types ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
struct GetUpdatesRequest {
    get_updates_buf: String,
    base_info: BaseInfo,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct SendMessageRequest {
    msg: SendMessageMsg,
    base_info: BaseInfo,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
struct SendMessageMsg {
    #[serde(default)]
    from_user_id: String,
    to_user_id: String,
    client_id: String,
    message_type: i64,
    message_state: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    item_list: Option<Vec<SendMessageItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)]
struct SendMessageItem {
    #[serde(rename = "type")]
    item_type: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    text_item: Option<SendTextItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_item: Option<SendImageItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    video_item: Option<SendVideoItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_item: Option<SendFileItem>,
}

#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)]
struct SendTextItem {
    text: String,
}

#[derive(Debug, Clone, Serialize)]
struct SendFileItem {
    media: SendMediaInfo,
    file_name: String,
    len: String,
}

#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)]
struct SendImageItem {
    media: SendMediaInfo,
    /// Ciphertext file size (AES-ECB padded).
    mid_size: i64,
}

#[derive(Debug, Clone, Serialize)]
struct SendVideoItem {
    media: SendMediaInfo,
    /// Ciphertext video size.
    video_size: i64,
}

#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)]
struct SendMediaInfo {
    encrypt_query_param: String,
    aes_key: String,
    encrypt_type: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct GetUploadUrlRequest {
    filekey: String,
    media_type: i64,
    to_user_id: String,
    rawsize: i64,
    rawfilemd5: String,
    filesize: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    thumb_rawsize: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thumb_rawfilemd5: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thumb_filesize: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    no_need_thumb: Option<bool>,
    aeskey: String,
    base_info: BaseInfo,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
struct SendTypingRequest {
    ilink_user_id: String,
    typing_ticket: String,
    status: i64,
    base_info: BaseInfo,
}

// ── Response types (deserialized from full body) ───────────────────────

/// getupdates response — parsed from the full response body.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
struct GetUpdatesResponse {
    #[serde(default)]
    ret: i64,
    #[serde(default)]
    errcode: i64,
    #[serde(default)]
    errmsg: String,
    #[serde(default)]
    get_updates_buf: String,
    #[serde(default)]
    msgs: Vec<IlinkMessage>,
}

/// getconfig response — parsed from the full response body.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
struct GetConfigResponse {
    #[serde(default)]
    ret: i64,
    #[serde(default)]
    errmsg: String,
    #[serde(default)]
    wxid: String,
    #[serde(default)]
    nickname: String,
    #[serde(default)]
    typing_ticket: String,
    #[serde(default)]
    aeskey: String,
    #[serde(flatten)]
    custom: HashMap<String, serde_json::Value>,
}

/// getuploadurl response — parsed from the full response body.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
struct GetUploadUrlResponse {
    #[serde(default)]
    ret: i64,
    #[serde(default)]
    errmsg: String,
    #[serde(default)]
    upload_param: String,
    #[serde(default)]
    thumb_upload_param: String,
    #[serde(default)]
    upload_full_url: String,
}

/// QR login — get_bot_qrcode response.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct QrCodeResponse {
    #[serde(default)]
    qrcode: String,
    #[serde(default)]
    qrcode_img_content: String,
}

/// QR login — get_qrcode_status response.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct QrStatus {
    #[serde(default)]
    status: String,
    #[serde(default)]
    bot_token: String,
    #[serde(default, rename = "ilink_bot_id")]
    wxid: String,
    #[serde(default, rename = "baseurl")]
    base_url: String,
    #[serde(default, rename = "ilink_user_id")]
    ilink_user_id: String,
    #[serde(default)]
    redirect_host: String,
    #[serde(default)]
    nickname: String,
}

// ── Inbound event (parsed from IlinkMessage) ───────────────────────────

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct InboundEvent {
    msg_id: String,
    sender_wxid: String,
    sender_name: String,
    chat_id: String,
    is_group: bool,
    is_mentioned: bool,
    content: InboundContent,
    context_token: String,
    raw_timestamp: i64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum InboundContent {
    Text(String),
    Image { aes_key: String, encrypt_query_param: String },
    Voice { aes_key: String, encrypt_query_param: String, duration_secs: i64 },
    Video { aes_key: String, encrypt_query_param: String, duration_secs: i64 },
    File { filename: String, size_bytes: i64, aes_key: String, encrypt_query_param: String },
    Link { title: String, url: String, description: String },
    System(String),
    Unknown(i64),
}

fn parse_inbound(msg: &IlinkMessage, _bot_wxid: &str) -> InboundEvent {
    let is_group = msg.is_group();
    let content = if let Some(first_item) = msg.item_list.first() {
        match first_item.item_type {
            ITEM_TYPE_TEXT => {
                let text = first_item
                    .text_item
                    .as_ref()
                    .map(|ti| ti.text.clone())
                    .unwrap_or_default();
                InboundContent::Text(text)
            }
            ITEM_TYPE_IMAGE => {
                let media = first_item.image_item.as_ref().and_then(|ii| ii.media.as_ref());
                InboundContent::Image {
                    aes_key: media.map(|m| m.aes_key.clone()).unwrap_or_default(),
                    encrypt_query_param: media.map(|m| m.encrypt_query_param.clone()).unwrap_or_default(),
                }
            }
            ITEM_TYPE_VOICE => {
                let media = first_item.voice_item.as_ref().and_then(|vi| vi.media.as_ref());
                let duration = first_item.voice_item.as_ref().map(|vi| vi.duration).unwrap_or(0);
                InboundContent::Voice {
                    aes_key: media.map(|m| m.aes_key.clone()).unwrap_or_default(),
                    encrypt_query_param: media.map(|m| m.encrypt_query_param.clone()).unwrap_or_default(),
                    duration_secs: duration,
                }
            }
            ITEM_TYPE_FILE => {
                let fi = first_item.file_item.as_ref();
                let media = fi.and_then(|f| f.media.as_ref());
                InboundContent::File {
                    filename: fi.map(|f| f.file_name.clone()).unwrap_or_default(),
                    size_bytes: fi.and_then(|f| f.len.parse::<i64>().ok()).unwrap_or(0),
                    aes_key: media.map(|m| m.aes_key.clone()).unwrap_or_default(),
                    encrypt_query_param: media.map(|m| m.encrypt_query_param.clone()).unwrap_or_default(),
                }
            }
            ITEM_TYPE_VIDEO => {
                let media = first_item.video_item.as_ref().and_then(|vi| vi.media.as_ref());
                let duration = first_item.video_item.as_ref().map(|vi| vi.duration).unwrap_or(0);
                InboundContent::Video {
                    aes_key: media.map(|m| m.aes_key.clone()).unwrap_or_default(),
                    encrypt_query_param: media.map(|m| m.encrypt_query_param.clone()).unwrap_or_default(),
                    duration_secs: duration,
                }
            }
            ITEM_TYPE_LINK => {
                let link = first_item.link_item.as_ref();
                InboundContent::Link {
                    title: link.map(|l| l.title.clone()).unwrap_or_default(),
                    url: link.map(|l| l.url.clone()).unwrap_or_default(),
                    description: link.map(|l| l.description.clone()).unwrap_or_default(),
                }
            }
            _ => {
                if let Some(text_item) = &first_item.text_item {
                    InboundContent::System(text_item.text.clone())
                } else {
                    InboundContent::Unknown(first_item.item_type)
                }
            }
        }
    } else {
        InboundContent::Unknown(0)
    };

    let msg_id = if msg.client_id.is_empty() {
        format!("{}_{}", msg.from_user_id, msg.create_time_ms)
    } else {
        msg.client_id.clone()
    };

    // TODO: parse actual @mention data from group message content.
    let is_mentioned = false;

    InboundEvent {
        msg_id,
        sender_wxid: msg.from_user_id.clone(),
        sender_name: String::new(), // Not available in standard WeixinMessage
        chat_id: msg.chat_id().to_string(),
        is_group,
        is_mentioned,
        content,
        context_token: msg.context_token.clone(),
        raw_timestamp: msg.create_time_ms,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// API client + error types
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
enum ApiError {
    #[error("Network error: {0}")]
    Network(String),
    #[error("HTTP {0}: {1}")]
    Http(u16, String),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("API error {0}: {1}")]
    Api(i64, String),
    #[error("Not authenticated")]
    NotAuthenticated,
}

/// Mutable state shared between the API client and the monitor.
#[derive(Debug, Clone, Default)]
struct SharedState {
    bot_token: Option<String>,
    bot_wxid: Option<String>,
    bot_nickname: Option<String>,
    get_updates_buf: String,
    typing_ticket: Option<String>,
    aes_key: Option<String>,
    /// Per-user context tokens (chat_id → context_token).
    context_tokens: HashMap<String, String>,
    /// API base URL — may change on IDC redirect or QR login confirmed.
    api_base: Option<String>,
}

/// HTTP client for the iLink Bot API.
#[derive(Clone)]
struct ApiClient {
    api_base: String,
    http: Client,
    state: Arc<RwLock<SharedState>>,
    client_version: String,
}

/// Unified API response wrapper.
struct ApiResponse {
    raw: serde_json::Value,
}

impl ApiResponse {
    fn raw(&self) -> &serde_json::Value {
        &self.raw
    }

    /// Parse the full response body as a typed struct.
    fn get<T: DeserializeOwned>(&self) -> Result<T, ApiError> {
        serde_json::from_value(self.raw.clone())
            .map_err(|e| ApiError::Parse(format!("deserialize: {e}")))
    }

    fn ret_code(&self) -> Option<i64> {
        self.raw
            .get("ret")
            .and_then(|v| v.as_i64())
            .or_else(|| self.raw.get("errcode").and_then(|v| v.as_i64()))
    }

    fn errmsg(&self) -> String {
        self.raw
            .get("errmsg")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    /// Check for API-level errors. Returns Ok for ret=0 or missing ret/errcode.
    fn check_ret(&self) -> Result<(), ApiError> {
        if self.raw.as_object().map_or(true, |o| o.is_empty()) {
            // Empty body is treated as success (e.g. sendmessage).
            return Ok(());
        }
        match self.ret_code() {
            Some(-14) => Err(ApiError::Api(-14, "rate limited".into())),
            Some(code) if code != 0 => Err(ApiError::Api(code, self.errmsg())),
            _ => Ok(()),
        }
    }
}

impl ApiClient {
    fn new(config: &WechatConfig) -> Self {
        let poll_timeout = config.poll_timeout;
        let http = zeroclaw_config::schema::build_runtime_proxy_client_with_timeouts(
            "channel.wechat",
            (poll_timeout + 15) as u64,
            5,
        );
        let mut state = SharedState::default();
        if let Some(ref token) = config.bot_token {
            tracing::info!("WeChat: loading bot_token from config");
            state.bot_token = Some(token.clone());
        }
        if let Some(ref key) = config.aes_key {
            state.aes_key = Some(key.clone());
        }
        Self {
            api_base: config.api_base.trim_end_matches('/').to_string(),
            http,
            state: Arc::new(RwLock::new(state)),
            client_version: build_client_version().to_string(),
        }
    }

    fn http_client(&self) -> &Client {
        &self.http
    }

    fn state(&self) -> Arc<RwLock<SharedState>> {
        self.state.clone()
    }

    /// Generate X-WECHAT-UIN header value: base64(decimal string of random u32).
    fn random_uin_header() -> String {
        let uin: u32 = rand::random();
        BASE64.encode(uin.to_string())
    }

    fn url(&self, endpoint: &str) -> String {
        // Use dynamic api_base from state if available (updated on IDC redirect),
        // otherwise fall back to the initial api_base.
        let base = self.state.try_read()
            .ok()
            .and_then(|s| s.api_base.clone())
            .unwrap_or_else(|| self.api_base.clone());
        format!(
            "{}/{}",
            base.trim_end_matches('/'),
            endpoint.trim_start_matches('/')
        )
    }

    /// POST JSON with iLink auth headers.
    async fn api_post(
        &self,
        endpoint: &str,
        body: &serde_json::Value,
    ) -> Result<ApiResponse, ApiError> {
        let mut req = self.http.post(self.url(endpoint));
        req = self.add_auth_headers(req).await;
        req = req.header("X-WECHAT-UIN", Self::random_uin_header());
        req = req.header("iLink-App-Id", ILINK_APP_ID);
        req = req.header("iLink-App-ClientVersion", &self.client_version);

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

        let raw: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ApiError::Parse(e.to_string()))?;

        Ok(ApiResponse { raw })
    }

    /// GET with iLink headers (no auth — used for QR login).
    async fn api_get(
        &self,
        endpoint: &str,
    ) -> Result<ApiResponse, ApiError> {
        let mut req = self.http.get(self.url(endpoint));
        // No auth for GET endpoints (login flow).
        req = req.header("X-WECHAT-UIN", Self::random_uin_header());
        req = req.header("iLink-App-Id", ILINK_APP_ID);
        req = req.header("iLink-App-ClientVersion", &self.client_version);

        let resp = req
            .send()
            .await
            .map_err(|e| ApiError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ApiError::Http(status.as_u16(), text));
        }

        let raw: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| ApiError::Parse(e.to_string()))?;

        Ok(ApiResponse { raw })
    }

    async fn add_auth_headers(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        // Always send AuthorizationType for POST requests (matches reference impl).
        req = req.header("AuthorizationType", "ilink_bot_token");
        let token = self.state.read().await.bot_token.clone();
        if let Some(t) = token {
            if !t.is_empty() {
                req = req.header("Authorization", format!("Bearer {}", t));
            }
        }
        req
    }

    
    // ── High-level API methods ─────────────────────────────────────────

    /// Long-poll for new messages.
    async fn get_updates(&self) -> Result<GetUpdatesResponse, ApiError> {
        let buf = self.state.read().await.get_updates_buf.clone();
        let req_body = GetUpdatesRequest {
            get_updates_buf: buf,
            base_info: build_base_info(),
        };
        let resp = self
            .api_post(
                "ilink/bot/getupdates",
                &serde_json::to_value(&req_body).unwrap(),
            )
            .await?;

        // Update sync buffer from response.
        let new_buf = resp.raw().get("get_updates_buf")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if !new_buf.is_empty() {
            self.state.write().await.get_updates_buf = new_buf;
        }

        resp.get()
    }

    /// Send a text message.
    async fn send_text(
        &self,
        to_user_id: &str,
        text: &str,
        context_token: Option<&str>,
    ) -> Result<serde_json::Value, ApiError> {
        let client_id = format!("openclaw-weixin_{}", uuid::Uuid::new_v4());
        let req = SendMessageRequest {
            msg: SendMessageMsg {
                from_user_id: String::new(),
                to_user_id: to_user_id.to_string(),
                client_id,
                message_type: MESSAGE_TYPE_BOT,
                message_state: MESSAGE_STATE_FINISH,
                item_list: Some(vec![SendMessageItem {
                    item_type: ITEM_TYPE_TEXT,
                    text_item: Some(SendTextItem { text: text.to_string() }),
                    image_item: None,
                    video_item: None,
                    file_item: None,
                }]),
                context_token: context_token.map(|s| s.to_string()),
            },
            base_info: build_base_info(),
        };
        let resp = self
            .api_post(
                "ilink/bot/sendmessage",
                &serde_json::to_value(&req).unwrap(),
            )
            .await?;
        Ok(resp.raw().clone())
    }

    /// Send typing indicator.
    async fn send_typing(
        &self,
        to_user_id: &str,
        typing: bool,
    ) -> Result<(), ApiError> {
        let ticket = self.state.read().await.typing_ticket.clone().unwrap_or_default();
        let req = SendTypingRequest {
            ilink_user_id: to_user_id.to_string(),
            typing_ticket: ticket,
            status: if typing { TYPING_STATUS_TYPING } else { TYPING_STATUS_CANCEL },
            base_info: build_base_info(),
        };
        let resp = self
            .api_post(
                "ilink/bot/sendtyping",
                &serde_json::to_value(&req).unwrap(),
            )
            .await?;
        resp.check_ret()?;
        Ok(())
    }

    /// Fetch bot config for a specific user (typing ticket, AES key, etc.).
    async fn get_config(
        &self,
        ilink_user_id: &str,
        context_token: Option<&str>,
    ) -> Result<GetConfigResponse, ApiError> {
        let mut body = serde_json::json!({
            "base_info": build_base_info(),
            "ilink_user_id": ilink_user_id,
        });
        if let Some(ct) = context_token {
            body["context_token"] = serde_json::Value::String(ct.to_string());
        }
        let resp = self.api_post("ilink/bot/getconfig", &body).await?;
        resp.check_ret()?;

        let cfg: GetConfigResponse = resp.get()?;

        // Store derived fields in shared state.
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

        Ok(cfg)
    }

    /// Get upload URL for CDN media.
    async fn get_upload_url(
        &self,
        req: GetUploadUrlRequest,
    ) -> Result<GetUploadUrlResponse, ApiError> {
        let resp = self
            .api_post(
                "ilink/bot/getuploadurl",
                &serde_json::to_value(&req).unwrap(),
            )
            .await?;
        resp.check_ret()?;
        resp.get()
    }

    /// QR login — get bot QR code.
    async fn get_bot_qrcode(&self) -> Result<QrCodeResponse, ApiError> {
        let resp = self
            .api_get("ilink/bot/get_bot_qrcode?bot_type=3")
            .await?;
        resp.check_ret()?;
        resp.get()
    }

    /// QR login — poll scan status.
    async fn get_qrcode_status(&self, qrcode: &str) -> Result<QrStatus, ApiError> {
        let endpoint = format!(
            "ilink/bot/get_qrcode_status?qrcode={}",
            urlencoding::encode(qrcode)
        );
        let resp = self.api_get(&endpoint).await?;
        resp.check_ret()?;
        resp.get()
    }

    /// Upload a file to CDN and send as an image message.
    async fn send_image_message(
        &self,
        to_user_id: &str,
        plaintext: &[u8],
        cdn_base_url: &str,
        context_token: Option<&str>,
    ) -> Result<(), ApiError> {
        let (_filekey, download_param, aeskey_hex, file_size_ct) =
            self.upload_media(plaintext, to_user_id, 1, cdn_base_url).await?;

        let client_id = format!("openclaw-weixin_{}", uuid::Uuid::new_v4());
        let req = SendMessageRequest {
            msg: SendMessageMsg {
                from_user_id: String::new(),
                to_user_id: to_user_id.to_string(),
                client_id,
                message_type: MESSAGE_TYPE_BOT,
                message_state: MESSAGE_STATE_FINISH,
                item_list: Some(vec![SendMessageItem {
                    item_type: ITEM_TYPE_IMAGE,
                    text_item: None,
                    image_item: Some(SendImageItem {
                        media: SendMediaInfo {
                            encrypt_query_param: download_param,
                            aes_key: BASE64.encode(hex::decode(&aeskey_hex).unwrap_or_default()),
                            encrypt_type: 1,
                        },
                        mid_size: file_size_ct as i64,
                    }),
                    video_item: None,
                    file_item: None,
                }]),
                context_token: context_token.map(|s| s.to_string()),
            },
            base_info: build_base_info(),
        };
        self.api_post(
            "ilink/bot/sendmessage",
            &serde_json::to_value(&req).unwrap(),
        )
        .await?;
        Ok(())
    }

    /// Upload a file to CDN and send as a file attachment message.
    async fn send_file_message(
        &self,
        to_user_id: &str,
        file_name: &str,
        plaintext: &[u8],
        cdn_base_url: &str,
        context_token: Option<&str>,
    ) -> Result<(), ApiError> {
        let (_filekey, download_param, aeskey_hex, _file_size_ct) =
            self.upload_media(plaintext, to_user_id, 3, cdn_base_url).await?;

        let client_id = format!("openclaw-weixin_{}", uuid::Uuid::new_v4());
        let req = SendMessageRequest {
            msg: SendMessageMsg {
                from_user_id: String::new(),
                to_user_id: to_user_id.to_string(),
                client_id,
                message_type: MESSAGE_TYPE_BOT,
                message_state: MESSAGE_STATE_FINISH,
                item_list: Some(vec![SendMessageItem {
                    item_type: ITEM_TYPE_FILE,
                    text_item: None,
                    image_item: None,
                    video_item: None,
                    file_item: Some(SendFileItem {
                        media: SendMediaInfo {
                            encrypt_query_param: download_param,
                            aes_key: BASE64.encode(hex::decode(&aeskey_hex).unwrap_or_default()),
                            encrypt_type: 1,
                        },
                        file_name: file_name.to_string(),
                        len: plaintext.len().to_string(),
                    }),
                }]),
                context_token: context_token.map(|s| s.to_string()),
            },
            base_info: build_base_info(),
        };
        self.api_post(
            "ilink/bot/sendmessage",
            &serde_json::to_value(&req).unwrap(),
        )
        .await?;
        Ok(())
    }

    /// Full upload pipeline: hash -> getUploadUrl -> encrypt -> CDN upload -> return info.
    /// Returns (filekey, download_encrypted_query_param, aeskey_hex, ciphertext_size).
    async fn upload_media(
        &self,
        plaintext: &[u8],
        to_user_id: &str,
        media_type: i64,
        cdn_base_url: &str,
    ) -> Result<(String, String, String, usize), ApiError> {
        use rand::RngExt;
        let rawsize = plaintext.len() as i64;
        let rawfilemd5 = format!("{:x}", md5::Md5::digest(plaintext));
        let filesize = aes_ecb_padded_size(rawsize as usize) as i64;

        // Generate random filekey and aeskey
        let mut filekey_bytes = [0u8; 16];
        rand::rng().fill(&mut filekey_bytes);
        let filekey = hex::encode(filekey_bytes);

        let mut aeskey_bytes = [0u8; 16];
        rand::rng().fill(&mut aeskey_bytes);
        let aeskey_hex = hex::encode(aeskey_bytes);

        // Step 1: getUploadUrl
        let upload_req = GetUploadUrlRequest {
            filekey: filekey.clone(),
            media_type,
            to_user_id: to_user_id.to_string(),
            rawsize,
            rawfilemd5: rawfilemd5.clone(),
            filesize,
            thumb_rawsize: None,
            thumb_rawfilemd5: None,
            thumb_filesize: None,
            no_need_thumb: Some(true),
            aeskey: aeskey_hex.clone(),
            base_info: build_base_info(),
        };
        let upload_resp = self.get_upload_url(upload_req).await?;

        // Step 2: determine CDN upload URL
        let cdn_url = if !upload_resp.upload_full_url.trim().is_empty() {
            upload_resp.upload_full_url.trim().to_string()
        } else if !upload_resp.upload_param.is_empty() {
            format!(
                "{}/upload?encrypted_query_param={}&filekey={}",
                cdn_base_url.trim_end_matches('/'),
                urlencoding::encode(&upload_resp.upload_param),
                urlencoding::encode(&filekey),
            )
        } else {
            return Err(ApiError::Api(-1, "getUploadUrl returned no upload URL".into()));
        };

        // Step 3: encrypt and upload to CDN
        let aeskey_arr: [u8; 16] = aeskey_bytes;
        let ciphertext = encrypt_ecb(plaintext, &aeskey_arr);

        let cdn = CdnHandler::new(&self.http);
        let download_param: String = cdn.upload_encrypted(&cdn_url, &ciphertext).await
            .map_err(|e: CdnError| ApiError::Network(e.to_string()))?;

        Ok((filekey, download_param, aeskey_hex, ciphertext.len()))
    }

    /// Store a context token for a user (called on each inbound message).
    async fn store_context_token(&self, user_id: &str, token: &str) {
        if !token.is_empty() {
            tracing::debug!(user_id, context_token = token, "wechat: stored context_token");
            self.state
                .write()
                .await
                .context_tokens
                .insert(user_id.to_string(), token.to_string());
        }
    }

    async fn get_context_token(&self, user_id: &str) -> Option<String> {
        self.state.read().await.context_tokens.get(user_id).cloned()
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// CDN handler
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Debug, thiserror::Error)]
enum CdnError {
    #[error("Network error: {0}")]
    Network(String),
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("Crypto error: {0}")]
    Crypto(String),
    #[error("Upload error: {0}")]
    Upload(String),
}

struct CdnHandler {
    client: Client,
}

impl CdnHandler {
    fn new(client: &Client) -> Self {
        Self { client: client.clone() }
    }

    /// Download and decrypt media from CDN.
    #[allow(dead_code)]
    async fn download_and_decrypt(&self, url: &str, aes_key: &[u8; 16]) -> Result<Vec<u8>, CdnError> {
        let resp = self.client
            .get(url)
            .send()
            .await
            .map_err(|e| CdnError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(CdnError::Http(resp.status().to_string()));
        }
        let encrypted = resp.bytes().await.map_err(|e| CdnError::Network(e.to_string()))?;
        // Try base64 decode first, fall back to raw bytes.
        let ciphertext = match BASE64.decode(&encrypted) {
            Ok(decoded) => decoded,
            Err(_) => encrypted.to_vec(),
        };
        decrypt_ecb(&ciphertext, aes_key).map_err(CdnError::Crypto)
    }

    /// Upload encrypted data to CDN via POST application/octet-stream.
    /// Returns the `x-encrypted-param` header value (download param).
    async fn upload_encrypted(
        &self,
        url: &str,
        ciphertext: &[u8],
    ) -> Result<String, CdnError> {
        let resp = self.client
            .post(url)
            .header("Content-Type", "application/octet-stream")
            .body(ciphertext.to_vec())
            .send()
            .await
            .map_err(|e| CdnError::Network(e.to_string()))?;

        if resp.status().as_u16() >= 400 && resp.status().as_u16() < 500 {
            let msg = resp.headers().get("x-error-message")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("client error")
                .to_string();
            return Err(CdnError::Upload(format!("{}: {}", resp.status(), msg)));
        }
        if !resp.status().is_success() {
            return Err(CdnError::Http(resp.status().to_string()));
        }

        let download_param = resp.headers()
            .get("x-encrypted-param")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if download_param.is_empty() {
            return Err(CdnError::Upload("CDN response missing x-encrypted-param header".into()));
        }

        Ok(download_param)
    }

    /// Build CDN upload URL from upload_param fallback.
    #[allow(dead_code)]
    fn build_upload_url(cdn_base: &str, upload_param: &str, filekey: &str) -> String {
        format!(
            "{}/upload?encrypted_query_param={}&filekey={}",
            cdn_base.trim_end_matches('/'),
            urlencoding::encode(upload_param),
            urlencoding::encode(filekey),
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Deduplication state
// ═══════════════════════════════════════════════════════════════════════════

struct DedupState {
    seen_ids: HashMap<String, i64>,
    cap: usize,
    ttl_secs: i64,
}

impl DedupState {
    fn new() -> Self {
        Self {
            seen_ids: HashMap::new(),
            cap: 10_000,
            ttl_secs: 600,
        }
    }

    /// Returns `true` if duplicate.
    fn check_and_record(&mut self, msg_id: &str) -> bool {
        if self.seen_ids.contains_key(msg_id) {
            return true;
        }
        let now = epoch_secs();
        self.seen_ids.insert(msg_id.to_string(), now);

        if self.seen_ids.len() > self.cap {
            let deadline = now - self.ttl_secs;
            let cap = self.cap;
            let initial_len = self.seen_ids.len();
            let mut removed = 0usize;
            self.seen_ids.retain(|_, ts| {
                if initial_len - removed <= cap {
                    return true;
                }
                let keep = *ts > deadline;
                if !keep { removed += 1; }
                keep
            });
            if self.seen_ids.len() > cap {
                let mut entries: Vec<(String, i64)> = self.seen_ids.iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .collect();
                entries.sort_unstable_by_key(|(_, ts)| *ts);
                for (id, _) in entries.iter().take(entries.len() - cap) {
                    self.seen_ids.remove(id);
                }
            }
        }
        false
    }
}

fn epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ═══════════════════════════════════════════════════════════════════════════
// Channel implementation
// ═══════════════════════════════════════════════════════════════════════════

/// WeChat iLink Bot channel implementation.
pub struct WechatChannel {
    config: WechatConfig,
    api: ApiClient,
    dedup: Arc<RwLock<DedupState>>,
}

impl WechatChannel {
    pub fn new(config: WechatConfig) -> Self {
        let api = ApiClient::new(&config);
        Self {
            config,
            api,
            dedup: Arc::new(RwLock::new(DedupState::new())),
        }
    }
}

#[async_trait]
impl Channel for WechatChannel {
    fn name(&self) -> &str {
        "wechat"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        // Look up context token for this recipient.
        let context_token = self.api.get_context_token(&message.recipient).await;
        let ctx_ref = context_token.as_deref();

        // Send text content if present.
        if !message.content.is_empty() {
            self.api
                .send_text(&message.recipient, &message.content, ctx_ref)
                .await
                .map_err(|e| anyhow::anyhow!("WeChat send error: {e}"))?;
        }

        // Send attachments via CDN upload.
        for attachment in &message.attachments {
            let kind = attachment.kind();
            match kind {
                zeroclaw_api::media::MediaKind::Image => {
                    self.api
                        .send_image_message(
                            &message.recipient,
                            &attachment.data,
                            DEFAULT_CDN_BASE_URL,
                            ctx_ref,
                        )
                        .await
                        .map_err(|e| anyhow::anyhow!("WeChat image send error: {e}"))?;
                }
                zeroclaw_api::media::MediaKind::Audio
                | zeroclaw_api::media::MediaKind::Video
                | zeroclaw_api::media::MediaKind::Unknown => {
                    // Treat as file attachment.
                    self.api
                        .send_file_message(
                            &message.recipient,
                            &attachment.file_name,
                            &attachment.data,
                            DEFAULT_CDN_BASE_URL,
                            ctx_ref,
                        )
                        .await
                        .map_err(|e| anyhow::anyhow!("WeChat file send error: {e}"))?;
                }
            }
        }

        Ok(())
    }

    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        tracing::info!("WeChat channel starting (iLink Bot API)");

        // Step 1: Login if needed.
        self.login().await?;

        // Step 2: Main poll loop.
        let mut consecutive_errors: u32 = 0;
        loop {
            match self.api.get_updates().await {
                Ok(resp) => {
                    // Rate limit check.
                    if resp.ret == -14 {
                        tracing::warn!("WeChat: rate limited (ret -14), pausing {}s", RATE_LIMIT_PAUSE_SECS);
                        tokio::time::sleep(Duration::from_secs(RATE_LIMIT_PAUSE_SECS)).await;
                        continue;
                    }

                    consecutive_errors = 0;
                    let bot_wxid = self.api.state().read().await.bot_wxid.clone().unwrap_or_default();

                    for msg in &resp.msgs {
                        // Dedup.
                        if self.dedup.write().await.check_and_record(&msg.client_id) {
                            continue;
                        }
                        let event = parse_inbound(msg, &bot_wxid);

                        // Store context token.
                        if !event.context_token.is_empty() {
                            self.api.store_context_token(&event.chat_id, &event.context_token).await;
                        }

                        // Fetch typing ticket per-user (lazy, on first message from user).
                        let _ = self.api.get_config(&event.sender_wxid, Some(&event.context_token)).await;

                        // Allowed users filter.
                        if !self.is_user_allowed(&event.sender_wxid) {
                            tracing::debug!("WeChat: ignoring message from unauthorized user {}", event.sender_wxid);
                            continue;
                        }

                        // Dispatch to orchestrator.
                        let channel_msg = ChannelMessage {
                            id: uuid::Uuid::new_v4().to_string(),
                            sender: event.sender_wxid.clone(),
                            reply_target: event.chat_id.clone(),
                            content: match &event.content {
                                InboundContent::Text(t) => t.clone(),
                                _ => String::new(),
                            },
                            channel: "wechat".to_string(),
                            timestamp: event.raw_timestamp as u64,
                            thread_ts: None,
                            interruption_scope_id: None,
                            attachments: self.build_attachments(&event).await,
                        };
                        if let Err(e) = tx.send(channel_msg).await {
                            tracing::error!("WeChat dispatch error: {e}");
                        }
                    }
                }
                Err(ApiError::Api(-14, _)) => {
                    tracing::warn!("WeChat: rate limited (-14), pausing {}s", RATE_LIMIT_PAUSE_SECS);
                    tokio::time::sleep(Duration::from_secs(RATE_LIMIT_PAUSE_SECS)).await;
                }
                Err(e) => {
                    consecutive_errors += 1;
                    let backoff = Self::classify_and_backoff(&e, consecutive_errors);

                    match Self::error_class(&e) {
                        ErrorClass::Auth => {
                            tracing::error!("WeChat: auth error ({consecutive_errors}): {e}");
                            self.api.state().write().await.bot_token = None;
                            match self.login().await {
                                Ok(()) => {
                                    tracing::info!("WeChat: re-login successful, resuming poll");
                                    consecutive_errors = 0;
                                    continue;
                                }
                                Err(login_err) => {
                                    tracing::error!("WeChat: re-login failed: {login_err}");
                                }
                            }
                        }
                        ErrorClass::Network => {
                            tracing::warn!("WeChat: network error, retrying in {backoff}s: {e}");
                        }
                        ErrorClass::Server => {
                            tracing::error!("WeChat: server error ({consecutive_errors}): {e}, retrying in {backoff}s");
                            if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                                tracing::error!("WeChat: {consecutive_errors} consecutive server errors, attempting re-login");
                                self.api.state().write().await.bot_token = None;
                                match self.login().await {
                                    Ok(()) => {
                                        tracing::info!("WeChat: re-login successful, resuming poll");
                                        consecutive_errors = 0;
                                        continue;
                                    }
                                    Err(login_err) => {
                                        tracing::error!("WeChat: re-login failed: {login_err}");
                                    }
                                }
                            }
                        }
                        ErrorClass::Parse => {
                            tracing::warn!("WeChat: parse error, retrying in {backoff}s: {e}");
                        }
                    }

                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                }
            }
        }
    }

    async fn start_typing(&self, recipient: &str) -> anyhow::Result<()> {
        self.api.send_typing(recipient, true).await?;
        Ok(())
    }

    async fn stop_typing(&self, recipient: &str) -> anyhow::Result<()> {
        self.api.send_typing(recipient, false).await?;
        Ok(())
    }

    async fn health_check(&self) -> bool {
        self.api.state().read().await.bot_token.is_some()
    }
}

// ── User filtering ──

impl WechatChannel {
    fn is_user_allowed(&self, user_id: &str) -> bool {
        let allowed = &self.config.allowed_users;
        if allowed.is_empty() {
            return false;
        }
        if allowed.iter().any(|u| u == "*") {
            return true;
        }
        allowed.iter().any(|u| u == user_id)
    }

    /// Download and decrypt media from CDN, return as MediaAttachment list.
    async fn build_attachments(&self, event: &InboundEvent) -> Vec<zeroclaw_api::media::MediaAttachment> {
        match &event.content {
            InboundContent::Image { aes_key, encrypt_query_param } => {
                self.download_media_attachment(aes_key, encrypt_query_param, "image.jpg", "image/jpeg").await
            }
            InboundContent::Voice { aes_key, encrypt_query_param, .. } => {
                self.download_media_attachment(aes_key, encrypt_query_param, "voice.mp3", "audio/mpeg").await
            }
            InboundContent::Video { aes_key, encrypt_query_param, .. } => {
                self.download_media_attachment(aes_key, encrypt_query_param, "video.mp4", "video/mp4").await
            }
            InboundContent::File { filename, aes_key, encrypt_query_param, .. } => {
                self.download_media_attachment(aes_key, encrypt_query_param, filename, "application/octet-stream").await
            }
            _ => vec![],
        }
    }

    async fn download_media_attachment(
        &self,
        aes_key_b64: &str,
        encrypt_query_param: &str,
        file_name: &str,
        mime_type: &str,
    ) -> Vec<zeroclaw_api::media::MediaAttachment> {
        if aes_key_b64.is_empty() || encrypt_query_param.is_empty() {
            return vec![];
        }
        let key_bytes = match BASE64.decode(aes_key_b64) {
            Ok(k) if k.len() == 16 => {
                let mut arr = [0u8; 16];
                arr.copy_from_slice(&k);
                arr
            }
            _ => {
                tracing::warn!("WeChat: invalid AES key for media download");
                return vec![];
            }
        };

        let cdn_base = DEFAULT_CDN_BASE_URL;
        let url = format!(
            "{}/download?encrypted_query_param={}",
            cdn_base.trim_end_matches('/'),
            urlencoding::encode(encrypt_query_param),
        );

        let cdn = CdnHandler::new(self.api.http_client());
        match cdn.download_and_decrypt(&url, &key_bytes).await {
            Ok(data) => vec![zeroclaw_api::media::MediaAttachment {
                file_name: file_name.to_string(),
                data,
                mime_type: Some(mime_type.to_string()),
            }],
            Err(e) => {
                tracing::warn!("WeChat: failed to download media: {e}");
                vec![]
            }
        }
    }
}

// ── Login logic ────────────────────────────────────────────────────────

impl WechatChannel {
    async fn login(&self) -> anyhow::Result<()> {
        if self.api.state().read().await.bot_token.is_some() {
            tracing::info!("WeChat: using saved bot_token");
            return Ok(());
        }

        tracing::info!("WeChat: starting QR login flow");
        let qr_resp = self.api.get_bot_qrcode().await?;

        if !qr_resp.qrcode_img_content.is_empty() {
            tracing::info!("WeChat QR code image available (base64, {} bytes)", qr_resp.qrcode_img_content.len());
            // TODO: push QR image/URL to user via sender channel.
        }

        let mut qrcode = qr_resp.qrcode;
        for _attempt in 0..QR_MAX_ATTEMPTS {
            tokio::time::sleep(Duration::from_secs(QR_POLL_INTERVAL_SECS)).await;

            let status = self.api.get_qrcode_status(&qrcode).await?;

            match status.status.as_str() {
                "confirmed" => {
                    tracing::info!(
                        "WeChat QR login confirmed: {} ({})",
                        status.nickname,
                        status.wxid
                    );
                    let state = self.api.state();
                    let mut st = state.write().await;
                    st.bot_token = Some(status.bot_token);
                    st.bot_wxid = Some(status.wxid.clone());
                    st.bot_nickname = Some(status.nickname.clone());
                    if !status.base_url.is_empty() {
                        tracing::info!("WeChat: API base updated to {}", status.base_url);
                        st.api_base = Some(status.base_url.clone());
                    }
                    return Ok(());
                }
                "expired" => {
                    tracing::warn!("WeChat: QR code expired, refreshing");
                    // Re-fetch QR code and continue polling.
                    match self.api.get_bot_qrcode().await {
                        Ok(new_qr) => {
                            qrcode = new_qr.qrcode;
                            if !new_qr.qrcode_img_content.is_empty() {
                                tracing::info!("WeChat: new QR code available (base64, {} bytes)", new_qr.qrcode_img_content.len());
                            }
                            continue;
                        }
                        Err(e) => return Err(anyhow::anyhow!("Failed to refresh QR code: {e}")),
                    }
                }
                "scaned_but_redirect" => {
                    if !status.redirect_host.is_empty() {
                        tracing::info!("WeChat: IDC redirect to {}", status.redirect_host);
                        let state = self.api.state();
                        let mut st = state.write().await;
                        st.api_base = Some(status.redirect_host.clone());
                    }
                }
                _ => {
                    // "wait" or "scaned" — keep polling.
                }
            }
        }

        Err(anyhow::anyhow!("QR login timed out after {} attempts", QR_MAX_ATTEMPTS))
    }
}

// ── Error classification ───────────────────────────────────────────────

#[derive(Debug)]
enum ErrorClass {
    Auth,
    Network,
    Server,
    Parse,
}

impl WechatChannel {
    fn error_class(err: &ApiError) -> ErrorClass {
        match err {
            ApiError::Http(code, _) => match *code {
                401 | 403 => ErrorClass::Auth,
                _ => ErrorClass::Server,
            },
            ApiError::Api(code, msg) => {
                let msg_lower = msg.to_lowercase();
                if *code == -1
                    || msg_lower.contains("token")
                    || msg_lower.contains("expired")
                    || msg_lower.contains("unauthorized")
                    || msg_lower.contains("not login")
                    || msg_lower.contains("请先登录")
                    || msg_lower.contains("未登录")
                {
                    ErrorClass::Auth
                } else {
                    ErrorClass::Server
                }
            }
            ApiError::Network(_) => ErrorClass::Network,
            ApiError::Parse(_) => ErrorClass::Parse,
            ApiError::NotAuthenticated => ErrorClass::Auth,
        }
    }

    fn classify_and_backoff(err: &ApiError, count: u32) -> u64 {
        match err {
            ApiError::Network(_) => std::cmp::min(5 + 2 * count as u64, 30),
            ApiError::Parse(_) => 3,
            ApiError::Http(401, _) | ApiError::Http(403, _) => 5,
            _ => std::cmp::min(2u64.pow(std::cmp::min(count, 6)), 60),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_version_encoding() {
        // "2.1.7" → (2<<16)|(1<<8)|7 = 131072+256+7 = 131335
        assert_eq!(build_client_version(), 131335);
    }

    #[test]
    fn test_dedup() {
        let mut st = DedupState::new();
        assert!(!st.check_and_record("msg1"));
        assert!(st.check_and_record("msg1"));
        assert!(!st.check_and_record("msg2"));
    }

    #[test]
    fn test_pkcs7_roundtrip() {
        let data = b"hello world";
        let padded = pkcs7_pad(data, 16);
        assert_eq!(padded.len() % 16, 0);
        let unpadded = pkcs7_unpad(&padded).unwrap();
        assert_eq!(data.to_vec(), unpadded);
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let key = b"0123456789abcdef";
        let plaintext = b"hello world test";
        let encrypted = encrypt_ecb(plaintext, key);
        let decrypted = decrypt_ecb(&encrypted, key).unwrap();
        assert_eq!(plaintext.to_vec(), decrypted);
    }

    #[test]
    fn test_aes_ecb_padded_size() {
        assert_eq!(aes_ecb_padded_size(0), 16);
        assert_eq!(aes_ecb_padded_size(1), 16);
        assert_eq!(aes_ecb_padded_size(15), 16);
        assert_eq!(aes_ecb_padded_size(16), 32);
        assert_eq!(aes_ecb_padded_size(100), 112);
    }

    #[test]
    fn test_parse_text_message() {
        let msg = IlinkMessage {
            seq: 0,
            message_id: 0,
            from_user_id: "user1".into(),
            to_user_id: "bot1".into(),
            client_id: "cid_123".into(),
            create_time_ms: 1000,
            update_time_ms: 0,
            delete_time_ms: 0,
            session_id: String::new(),
            group_id: String::new(),
            message_type: 1,
            message_state: 2,
            item_list: vec![MessageItem {
                item_type: ITEM_TYPE_TEXT,
                text_item: Some(TextItem { text: "hello".into() }),
                image_item: None,
                file_item: None,
                voice_item: None,
                video_item: None,
                link_item: None,
            }],
            context_token: "ctx_tok".into(),
        };
        let event = parse_inbound(&msg, "bot1");
        assert_eq!(event.sender_wxid, "user1");
        assert_eq!(event.chat_id, "user1");
        assert!(!event.is_group);
        match event.content {
            InboundContent::Text(ref t) => assert_eq!(t, "hello"),
            _ => panic!("expected text content"),
        }
    }

    #[test]
    fn test_base_info_serialization() {
        let bi = build_base_info();
        let json = serde_json::to_value(&bi).unwrap();
        assert_eq!(json["channel_version"], CHANNEL_VERSION);
    }
}
