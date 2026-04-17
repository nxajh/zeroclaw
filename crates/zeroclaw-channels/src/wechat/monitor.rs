use crate::wechat::api_client::ApiClient;
use crate::wechat::config::WechatConfig;
use crate::wechat::inbound::{parse_inbound, InboundEvent};
use crate::wechat::state::WechatState;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zeroclaw_api::channel::ChannelMessage;

/// Main WeChat monitor — handles login, long-poll loop, and dispatch.
pub struct WechatMonitor {
    api: ApiClient,
    state: WechatState,
    tx: mpsc::Sender<ChannelMessage>,
    cancel: CancellationToken,
}

impl WechatMonitor {
    pub fn new(config: WechatConfig, tx: mpsc::Sender<ChannelMessage>, cancel: CancellationToken) -> Self {
        let api = ApiClient::new(config);
        Self {
            api,
            state: WechatState::new(),
            tx,
            cancel,
        }
    }

    /// Create monitor with an existing (shared) `ApiClient`.
    pub fn new_with_api(
        api: ApiClient,
        config: WechatConfig,
        tx: mpsc::Sender<ChannelMessage>,
        cancel: CancellationToken,
    ) -> Self {
        let _config = config; // kept for future per-monitor config if needed
        Self {
            api,
            state: WechatState::new(),
            tx,
            cancel,
        }
    }

    /// Run the monitor: login → poll loop.
    pub async fn run(&mut self) -> anyhow::Result<()> {
        // Step 1: Login (QR code or saved token)
        self.login().await?;

        // Step 2: Main long-poll loop
        // Note: get_config is deferred — it is called per-user when needed
        // (e.g. to obtain typing_ticket), not at startup. bot_wxid is set
        // on the first successful get_config call, matching openclaw-weixin.
        let mut consecutive_errors = 0u32;

        loop {
            if self.cancel.is_cancelled() {
                tracing::info!("WeChat monitor cancelled, shutting down");
                return Ok(());
            }

            match self.api.get_updates().await {
                Ok(resp) => {
                    consecutive_errors = 0;

                    // -14 means we need to pause
                    if resp.ret == -14 {
                        tracing::warn!("WeChat API returned -14, pausing 30s");
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(30)) => {},
                            _ = self.cancel.cancelled() => return Ok(()),
                        }
                        continue;
                    }

                    let bot_wxid = self
                        .api
                        .state()
                        .read()
                        .await
                        .bot_wxid
                        .clone()
                        .unwrap_or_default();

                    for msg in resp.msgs {
                            // Dedup
                            if self.state.check_and_record(&msg.client_id) {
                                continue;
                            }
                            let event = parse_inbound(&msg, &bot_wxid);
                            self.dispatch(event).await;
                        }
                }
                Err(e) => {
                    let backoff_secs = Self::classify_and_backoff(&e, consecutive_errors);

                    match Self::error_class(&e) {
                        ErrorClass::Auth => {
                            tracing::error!("WeChat: auth error ({consecutive_errors}+1): {e}");
                            // Token expired / unauthorized — re-login immediately
                            self.api.state().write().await.token = None;
                            match self.login().await {
                                Ok(()) => {
                                    tracing::info!("WeChat: re-login successful, resuming poll");
                                    consecutive_errors = 0;
                                    continue;
                                }
                                Err(login_err) => {
                                    tracing::error!("WeChat: re-login failed: {login_err}");
                                    // Re-login failed — backoff then retry the loop
                                    // (don't clear token again, login() already tried)
                                }
                            }
                        }
                        ErrorClass::Network => {
                            // Network glitch — don't count toward re-login threshold
                            tracing::warn!(
                                "WeChat: network error, retrying in {backoff_secs}s: {e}"
                            );
                        }
                        ErrorClass::Server => {
                            consecutive_errors += 1;
                            tracing::error!(
                                "WeChat: server error ({consecutive_errors}): {e}, retrying in {backoff_secs}s"
                            );
                            if consecutive_errors >= 10 {
                                tracing::error!(
                                    "WeChat: {consecutive_errors} consecutive server errors, attempting re-login"
                                );
                                self.api.state().write().await.token = None;
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
                            // Likely a transient non-JSON response — short backoff, no counting
                            tracing::warn!("WeChat: parse error, retrying in {backoff_secs}s: {e}");
                        }
                    }

                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(backoff_secs)) => {},
                        _ = self.cancel.cancelled() => return Ok(()),
                    }
                }
            }
        }
    }

    /// Login via saved token or QR code scan.
    async fn login(&self) -> anyhow::Result<()> {
        // If we already have a token, skip login
        if self.api.state().read().await.token.is_some() {
            tracing::info!("WeChat: using saved bot_token");
            return Ok(());
        }

        // Start QR login flow
        tracing::info!("WeChat: starting QR login flow");
        let qr_resp = self.api.get_bot_qrcode().await?;

        if !qr_resp.qrcode_img_content.is_empty() {
            tracing::info!("WeChat QR code image available (base64, {} bytes)", qr_resp.qrcode_img_content.len());
            // TODO: push QR image/URL to user via sender channel
        }

        let qrcode = qr_resp.qrcode;
        loop {
            if self.cancel.is_cancelled() {
                return Err(anyhow::anyhow!("cancelled during login"));
            }

            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(3)) => {},
                _ = self.cancel.cancelled() => return Err(anyhow::anyhow!("cancelled")),
            }

            let status = self.api.get_qrcode_status(&qrcode).await?;
            if status.is_confirmed() {
                tracing::info!(
                    "WeChat QR login confirmed: {} ({})",
                    status.nickname,
                    status.wxid
                );
                self.api.set_token(status.bot_token.clone()).await;
                return Ok(());
            }
            if status.is_expired() {
                return Err(anyhow::anyhow!("QR code expired"));
            }
            // Still waiting for scan...
        }
    }

    /// Convert an InboundEvent into a ChannelMessage and send to the orchestrator.
    async fn dispatch(&self, event: InboundEvent) {
        let channel_msg = ChannelMessage {
            id: uuid::Uuid::new_v4().to_string(),
            sender: event.sender_wxid.clone(),
            reply_target: event.chat_id.clone(),
            content: match &event.content {
                crate::wechat::inbound::InboundContent::Text(t) => t.clone(),
                _ => String::new(),
            },
            channel: "WeChat".to_string(),
            timestamp: event.raw.timestamp as u64,
            thread_ts: None,
            interruption_scope_id: None,
            attachments: vec![],
        };
        if let Err(e) = self.tx.send(channel_msg).await {
            tracing::error!("WeChat dispatch error: {e}");
        }
    }
}

/// Classification of API errors for differentiated handling.
#[derive(Debug)]
enum ErrorClass {
    /// 401/403 or API response indicating token expiry — re-login immediately.
    Auth,
    /// Network / timeout — backoff only, no re-login counting.
    Network,
    /// Server-side 5xx — count toward re-login threshold.
    Server,
    /// Response parse failure — short backoff, no counting.
    Parse,
}

impl WechatMonitor {
    /// Classify an `ApiError` into an error handling category.
    fn error_class(err: &crate::wechat::api_client::ApiError) -> ErrorClass {
        use crate::wechat::api_client::ApiError;
        match err {
            ApiError::Http(code, _) => match *code {
                401 | 403 => ErrorClass::Auth,
                400..=499 => ErrorClass::Server, // other 4xx treated as server issue
                500..=599 => ErrorClass::Server,
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
        }
    }

    /// Calculate backoff duration (seconds) based on error class and error count.
    fn classify_and_backoff(err: &crate::wechat::api_client::ApiError, count: u32) -> u64 {
        use crate::wechat::api_client::ApiError;
        match err {
            ApiError::Network(_) => {
                // Network: moderate backoff 5-30s
                std::cmp::min(5 + 2 * count as u64, 30)
            }
            ApiError::Parse(_) => {
                // Parse: short fixed backoff — likely transient
                3
            }
            ApiError::Http(401, _) | ApiError::Http(403, _) => {
                // Auth: wait a bit before re-login attempt
                5
            }
            _ => {
                // Server/other: exponential 2^n, capped at 60s
                std::cmp::min(2u64.pow(std::cmp::min(count, 6)), 60)
            }
        }
    }
}
