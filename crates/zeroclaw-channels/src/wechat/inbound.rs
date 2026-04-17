use crate::wechat::api_types::{
    IlinkMessage, ITEM_TYPE_TEXT, ITEM_TYPE_IMAGE, ITEM_TYPE_VOICE,
    ITEM_TYPE_FILE, ITEM_TYPE_VIDEO, ITEM_TYPE_LINK,
};

/// Parsed inbound message ready for ZeroClaw processing.
#[derive(Debug, Clone)]
pub struct InboundEvent {
    /// Unique message ID (client_id or constructed).
    pub msg_id: String,

    /// Sender user ID (from_user_id).
    pub sender_wxid: String,

    /// Sender display name.
    pub sender_name: String,

    /// Chat ID (room_wxid for groups, from_user_id for DMs).
    pub chat_id: String,

    /// Whether this is a group message.
    pub is_group: bool,

    /// Whether the bot was @mentioned.
    pub is_mentioned: bool,

    /// Parsed content.
    pub content: InboundContent,

    /// Context token (must be echoed in reply).
    pub context_token: String,

    /// Original raw message (for reference).
    pub raw: IlinkMessage,
}

/// Parsed content of an inbound message.
#[derive(Debug, Clone)]
pub enum InboundContent {
    /// Plain text message.
    Text(String),

    /// Image (encrypted CDN params).
    Image { aes_key: String, encrypt_query_param: String },

    /// Voice message.
    Voice {
        aes_key: String,
        encrypt_query_param: String,
        duration_secs: i64,
    },

    /// Video message.
    Video {
        aes_key: String,
        encrypt_query_param: String,
        duration_secs: i64,
    },

    /// File message.
    File {
        filename: String,
        size_bytes: i64,
        aes_key: String,
        encrypt_query_param: String,
    },

    /// Link message.
    Link {
        title: String,
        url: String,
        description: String,
    },

    /// System notification message.
    System(String),

    /// Unknown / unhandled message type.
    Unknown(i64),
}

/// Parse a raw `IlinkMessage` into a structured `InboundEvent`.
pub fn parse_inbound(msg: &IlinkMessage, _bot_wxid: &str) -> InboundEvent {
    let is_group = msg.is_group();
    let chat_id = msg.chat_id().to_string();

    // Determine if the bot was @mentioned.
    // TODO: Parse actual @mention data from group message content when available.
    let is_mentioned = false;

    // Extract context token
    let context_token = msg.context_token.clone();

    // Parse content from item_list
    let content = parse_content(msg);

    InboundEvent {
        msg_id: if msg.client_id.is_empty() {
            format!("{}_{}", msg.from_user_id, msg.timestamp)
        } else {
            msg.client_id.clone()
        },
        sender_wxid: msg.from_user_id.clone(),
        sender_name: msg.nickname.clone(),
        chat_id,
        is_group,
        is_mentioned,
        content,
        context_token,
        raw: msg.clone(),
    }
}

/// Extract typed content from the message's item_list.
fn parse_content(msg: &IlinkMessage) -> InboundContent {
    let item = match msg.item_list.first() {
        Some(it) => it,
        None => return InboundContent::Text(String::new()),
    };

    match item.item_type {
        ITEM_TYPE_TEXT => {
            let text = item
                .text_item
                .as_ref()
                .map(|t| t.text.clone())
                .unwrap_or_default();
            InboundContent::Text(text)
        }
        ITEM_TYPE_IMAGE => {
            let (aes_key, eqp) = extract_media_params(item);
            InboundContent::Image { aes_key, encrypt_query_param: eqp }
        }
        ITEM_TYPE_VOICE => {
            let (aes_key, eqp) = extract_media_params(item);
            let duration_secs = item
                .voice_item
                .as_ref()
                .map(|v| v.duration)
                .unwrap_or(0);
            InboundContent::Voice { aes_key, encrypt_query_param: eqp, duration_secs }
        }
        ITEM_TYPE_VIDEO => {
            let (aes_key, eqp) = extract_media_params(item);
            let duration_secs = item
                .video_item
                .as_ref()
                .map(|v| v.duration)
                .unwrap_or(0);
            InboundContent::Video { aes_key, encrypt_query_param: eqp, duration_secs }
        }
        ITEM_TYPE_FILE => {
            let (aes_key, eqp) = extract_media_params(item);
            let filename = item
                .file_item
                .as_ref()
                .map(|f| f.file_name.clone())
                .unwrap_or_default();
            let size_bytes = item
                .file_item
                .as_ref()
                .map(|f| f.len.parse::<i64>().unwrap_or(0))
                .unwrap_or(0);
            InboundContent::File { filename, size_bytes, aes_key, encrypt_query_param: eqp }
        }
        ITEM_TYPE_LINK => {
            let link = item.link_item.as_ref();
            let title = link.map(|l| l.title.clone()).unwrap_or_default();
            let url = link.map(|l| l.url.clone()).unwrap_or_default();
            let description = link.map(|l| l.description.clone()).unwrap_or_default();
            InboundContent::Link { title, url, description }
        }
        // System notification: text content without explicit item type
        _ => {
            // Try to extract text for system messages
            if let Some(text_item) = &item.text_item {
                InboundContent::System(text_item.text.clone())
            } else {
                InboundContent::Unknown(item.item_type)
            }
        }
    }
}

/// Extract AES key and encrypt_query_param from a message item's media field.
fn extract_media_params(item: &crate::wechat::api_types::MessageItem) -> (String, String) {
    // Try image_item -> media
    if let Some(img) = &item.image_item {
        if let Some(media) = &img.media {
            return (media.aes_key.clone(), media.encrypt_query_param.clone());
        }
    }
    // Try file_item -> media
    if let Some(file) = &item.file_item {
        if let Some(media) = &file.media {
            return (media.aes_key.clone(), media.encrypt_query_param.clone());
        }
    }
    // Try voice_item -> media
    if let Some(voice) = &item.voice_item {
        if let Some(media) = &voice.media {
            return (media.aes_key.clone(), media.encrypt_query_param.clone());
        }
    }
    // Try video_item -> media
    if let Some(video) = &item.video_item {
        if let Some(media) = &video.media {
            return (media.aes_key.clone(), media.encrypt_query_param.clone());
        }
    }
    (String::new(), String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wechat::api_types::*;

    #[test]
    fn test_parse_text_dm() {
        let msg = IlinkMessage {
            from_user_id: "wxid_user1".to_string(),
            to_user_id: "wxid_bot".to_string(),
            room_wxid: String::new(),
            context_token: "ctx_tok_123".to_string(),
            item_list: vec![MessageItem {
                item_type: ITEM_TYPE_TEXT,
                text_item: Some(TextItem { text: "hello".to_string() }),
                image_item: None,
                file_item: None,
                voice_item: None,
                video_item: None,
                link_item: None,
            }],
            client_id: "msg_001".to_string(),
            timestamp: 1000,
            source: "wx".to_string(),
            nickname: "Alice".to_string(),
        };

        let event = parse_inbound(&msg, "wxid_bot");
        assert_eq!(event.sender_wxid, "wxid_user1");
        assert_eq!(event.chat_id, "wxid_user1");
        assert!(!event.is_group);
        assert_eq!(event.context_token, "ctx_tok_123");

        match event.content {
            InboundContent::Text(t) => assert_eq!(t, "hello"),
            _ => panic!("Expected Text content"),
        }
    }
}
