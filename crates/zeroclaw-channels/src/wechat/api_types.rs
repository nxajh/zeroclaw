use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Message type constants (proto: MessageType)
// ---------------------------------------------------------------------------

/// Bot outbound message type.
pub const MESSAGE_TYPE_BOT: i64 = 2;

// ---------------------------------------------------------------------------
// MessageItemType (proto enum)
// ---------------------------------------------------------------------------

pub const ITEM_TYPE_TEXT: i64 = 1;
pub const ITEM_TYPE_IMAGE: i64 = 2;
pub const ITEM_TYPE_FILE: i64 = 4;
pub const ITEM_TYPE_VOICE: i64 = 3;
pub const ITEM_TYPE_VIDEO: i64 = 5;
pub const ITEM_TYPE_LINK: i64 = 6;

// ---------------------------------------------------------------------------
// MessageState
// ---------------------------------------------------------------------------

pub const MESSAGE_STATE_FINISH: i64 = 2;

// ---------------------------------------------------------------------------
// Typing status
// ---------------------------------------------------------------------------

pub const TYPING_STATUS_TYPING: i64 = 1;
pub const TYPING_STATUS_CANCEL: i64 = 2;

// ---------------------------------------------------------------------------
// BaseInfo — attached to every outgoing request
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct BaseInfo {
    pub channel_version: String,
}

impl Default for BaseInfo {
    fn default() -> Self {
        Self {
            channel_version: "2.1.7".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Inbound message (from getupdates msgs array)
// ---------------------------------------------------------------------------

/// A single message from the `msgs` array in getupdates response.
#[derive(Debug, Clone, Deserialize)]
pub struct IlinkMessage {
    /// Sender user ID (e.g. wxid_xxx).
    #[serde(default, rename = "from_user_id")]
    pub from_user_id: String,

    /// Target user ID (the bot's ID or room ID).
    #[serde(default, rename = "to_user_id")]
    pub to_user_id: String,

    /// Room/group ID. Empty for DMs.
    #[serde(default)]
    pub room_wxid: String,

    /// Message item list (text, image, etc.).
    #[serde(default)]
    pub item_list: Vec<MessageItem>,

    /// Context token for multi-turn conversation (base64 string).
    #[serde(default)]
    pub context_token: String,

    /// Client message ID.
    #[serde(default)]
    pub client_id: String,

    /// Message timestamp.
    #[serde(default)]
    pub timestamp: i64,

    /// Source (e.g. "wx", "room").
    #[serde(default)]
    pub source: String,

    /// Sender nickname (may not always be present).
    #[serde(default)]
    pub nickname: String,

}

impl IlinkMessage {
    /// Chat ID: room_wxid for groups, from_user_id for DMs.
    pub fn chat_id(&self) -> &str {
        if self.room_wxid.is_empty() {
            &self.from_user_id
        } else {
            &self.room_wxid
        }
    }

    /// Whether this is a group message.
    pub fn is_group(&self) -> bool {
        !self.room_wxid.is_empty()
    }

    /// Extract text content from item_list.
    pub fn text_content(&self) -> Option<&str> {
        self.item_list
            .iter()
            .find(|item| item.item_type == ITEM_TYPE_TEXT)
            .and_then(|item| item.text_item.as_ref())
            .map(|ti| ti.text.as_str())
    }
}

// ---------------------------------------------------------------------------
// MessageItem (from proto: MessageItem)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct MessageItem {
    /// Item type (1=text, 2=image, 3=file, 4=voice, 5=video, 6=link).
    #[serde(default, rename = "type")]
    pub item_type: i64,

    /// Text item (present when type == 1).
    #[serde(default)]
    pub text_item: Option<TextItem>,

    /// Image item (present when type == 2).
    #[serde(default)]
    pub image_item: Option<ImageItem>,

    /// File item (present when type == 3).
    #[serde(default)]
    pub file_item: Option<FileItem>,

    /// Voice item (present when type == 4).
    #[serde(default)]
    pub voice_item: Option<VoiceItem>,

    /// Video item (present when type == 5).
    #[serde(default)]
    pub video_item: Option<VideoItem>,

    /// Link item (present when type == 6).
    #[serde(default)]
    pub link_item: Option<LinkItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TextItem {
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ImageItem {
    /// Encrypted image data (base64) or CDN URL params.
    #[serde(default)]
    pub media: Option<MediaInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileItem {
    #[serde(default)]
    pub file_name: String,
    #[serde(default)]
    pub len: String,
    #[serde(default)]
    pub media: Option<MediaInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VoiceItem {
    #[serde(default)]
    pub media: Option<MediaInfo>,
    #[serde(default)]
    pub duration: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VideoItem {
    #[serde(default)]
    pub media: Option<MediaInfo>,
    #[serde(default)]
    pub duration: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LinkItem {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub description: String,
}

/// Media download info (encrypted CDN params).
#[derive(Debug, Clone, Deserialize)]
pub struct MediaInfo {
    /// Base64-encoded AES key for this media.
    #[serde(default)]
    pub aes_key: String,

    /// Encrypted CDN query param (used to construct download URL).
    #[serde(default, rename = "encrypt_query_param")]
    pub encrypt_query_param: String,

    /// Encryption type (1 = AES-128-ECB).
    #[serde(default)]
    pub encrypt_type: i64,
}

// ---------------------------------------------------------------------------
// getupdates request / response
// ---------------------------------------------------------------------------

/// Request body for `ilink/bot/getupdates`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct GetUpdatesRequest {
    pub get_updates_buf: String,
    pub base_info: BaseInfo,
}

/// Response from `ilink/bot/getupdates`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GetUpdatesResponse {
    /// Return code (0 = success).
    #[serde(default)]
    pub ret: i64,

    /// Error message if ret != 0.
    #[serde(default)]
    pub errmsg: String,

    /// New sync buffer to use for next request.
    #[serde(default)]
    pub get_updates_buf: String,

    /// Array of new messages.
    #[serde(default)]
    pub msgs: Vec<IlinkMessage>,
}

// ---------------------------------------------------------------------------
// sendmessage request / response
// ---------------------------------------------------------------------------

/// Request body for `ilink/bot/sendmessage`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SendMessageRequest {
    pub msg: SendMessageMsg,
    pub base_info: BaseInfo,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SendMessageMsg {
    /// Empty string for outbound bot messages.
    #[serde(default)]
    pub from_user_id: String,

    /// Target user or room ID.
    pub to_user_id: String,

    /// Unique client message ID (e.g. "openclaw-weixin_<random>").
    pub client_id: String,

    /// Message type (3 = BOT).
    pub message_type: i64,

    /// Message state (2 = FINISH).
    pub message_state: i64,

    /// Message items.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_list: Option<Vec<SendMessageItem>>,

    /// Context token (echoed from inbound message).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_token: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendMessageItem {
    #[serde(rename = "type")]
    pub item_type: i64,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_item: Option<SendTextItem>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_item: Option<SendFileItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendTextItem {
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendFileItem {
    pub media: SendMediaInfo,
    pub file_name: String,
    pub len: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendMediaInfo {
    pub encrypt_query_param: String,
    pub aes_key: String,
    pub encrypt_type: i64,
}

/// Response from `ilink/bot/sendmessage` (empty on success).
#[derive(Debug, Clone, Deserialize)]
pub struct SendMessageResponse {}

// ---------------------------------------------------------------------------
// getuploadurl request / response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct GetUploadUrlRequest {
    pub filekey: String,
    pub media_type: i64,
    pub to_user_id: String,
    pub rawsize: i64,
    pub rawfilemd5: String,
    pub filesize: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumb_rawsize: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumb_rawfilemd5: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumb_filesize: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub no_need_thumb: Option<bool>,
    pub aeskey: String,
    pub base_info: BaseInfo,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GetUploadUrlResponse {
    #[serde(default)]
    pub ret: i64,
    #[serde(default)]
    pub errmsg: String,
    /// Upload parameters (encrypted).
    #[serde(default)]
    pub upload_param: String,
    /// Thumbnail upload parameters.
    #[serde(default)]
    pub thumb_upload_param: String,
    /// CDN host.
    #[serde(default)]
    pub cdn_host: String,
}

// ---------------------------------------------------------------------------
// sendtyping request / response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SendTypingRequest {
    pub ilink_user_id: String,
    pub typing_ticket: String,
    pub status: i64,
    pub base_info: BaseInfo,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SendTypingResponse {
    #[serde(default)]
    pub ret: i64,
    #[serde(default)]
    pub errmsg: String,
}

// ---------------------------------------------------------------------------
// getconfig response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GetConfigResponse {
    #[serde(default)]
    pub ret: i64,
    #[serde(default)]
    pub errmsg: String,
    /// Bot wxid.
    #[serde(default)]
    pub wxid: String,
    /// Bot display name.
    #[serde(default)]
    pub nickname: String,
    /// Base64-encoded typing ticket.
    #[serde(default)]
    pub typing_ticket: String,
    /// AES key (hex-encoded, from config).
    #[serde(default)]
    pub aeskey: String,
    /// Extra fields.
    #[serde(flatten)]
    pub custom: HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// QR login types
// ---------------------------------------------------------------------------

pub const QR_STATUS_CONFIRMED: &str = "confirmed";
pub const QR_STATUS_EXPIRED: &str = "expired";

/// Response from `get_bot_qrcode` API.
/// Fields match iLink Bot API: `qrcode` (token for polling) and `qrcode_img_content` (QR image URL).
#[derive(Debug, Clone, Deserialize)]
pub struct QrCodeResponse {
    #[serde(default)]
    pub ret: i64,
    #[serde(default)]
    pub errmsg: String,
    /// The QR code token used for polling get_qrcode_status.
    #[serde(default)]
    pub qrcode: String,
    /// URL of the QR code image for the user to scan.
    #[serde(default)]
    pub qrcode_img_content: String,
}

/// Response from `get_qrcode_status` API.
#[derive(Debug, Clone, Deserialize)]
pub struct QrStatus {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub bot_token: String,
    /// iLink bot ID (returned on confirmed).
    #[serde(default, rename = "ilink_bot_id")]
    pub wxid: String,
    /// iLink user ID who scanned the QR code.
    #[serde(default, rename = "ilink_user_id")]
    pub nickname: String,
}

impl QrStatus {
    pub fn is_confirmed(&self) -> bool {
        self.status == QR_STATUS_CONFIRMED
    }

    pub fn is_expired(&self) -> bool {
        self.status == QR_STATUS_EXPIRED
    }
}
