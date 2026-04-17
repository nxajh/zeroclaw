use crate::wechat::api_client::ApiClient;
use crate::wechat::inbound::InboundEvent;

/// Send a text reply to the chat where an inbound event came from.
pub async fn send_text_reply(
    api: &ApiClient,
    event: &InboundEvent,
    text: &str,
) -> Result<serde_json::Value, crate::wechat::api_client::ApiError> {
    api.send_text(
        &event.chat_id,
        text,
        Some(&event.context_token),
        None,
    )
    .await
}

/// Send a text message to an arbitrary user/chat.
pub async fn send_text_to(
    api: &ApiClient,
    to_user_id: &str,
    text: &str,
    context_token: Option<&str>,
) -> Result<serde_json::Value, crate::wechat::api_client::ApiError> {
    api.send_text(to_user_id, text, context_token, None).await
}

/// Send typing indicator for a chat.
pub async fn set_typing(
    api: &ApiClient,
    to_user_id: &str,
    typing: bool,
) -> Result<serde_json::Value, crate::wechat::api_client::ApiError> {
    api.send_typing(to_user_id, typing).await
}
