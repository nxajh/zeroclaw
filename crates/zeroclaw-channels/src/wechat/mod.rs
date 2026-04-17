pub mod api_client;
pub mod api_types;
pub mod cdn;
pub mod config;
pub mod crypto;
pub mod inbound;
pub mod monitor;
pub mod outbound;
pub mod state;

use async_trait::async_trait;
use tokio::sync::mpsc;
use zeroclaw_api::channel::{Channel, ChannelMessage, SendMessage};
use zeroclaw_config::schema::WechatConfig;

/// WeChat iLink Bot channel implementation.
pub struct WechatChannel {
    config: WechatConfig,
    api: api_client::ApiClient,
}

impl WechatChannel {
    pub fn new(config: WechatConfig) -> Self {
        let api = api_client::ApiClient::new(config.clone());
        Self { config, api }
    }
}

#[async_trait]
impl Channel for WechatChannel {
    fn name(&self) -> &str {
        "WeChat"
    }

    async fn send(&self, message: &SendMessage) -> anyhow::Result<()> {
        let result = self
            .api
            .send_text(&message.recipient, &message.content, None, None)
            .await
            .map_err(|e| anyhow::anyhow!("WeChat send error: {e}"))?;
        tracing::debug!("WeChat send response: {result:?}");
        Ok(())
    }

    async fn listen(&self, tx: mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        tracing::info!("WeChat channel starting (iLink Bot API)");
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut mon = monitor::WechatMonitor::new_with_api(
            self.api.clone(),
            self.config.clone(),
            tx,
            cancel.clone(),
        );
        mon.run().await
    }

    async fn start_typing(&self, recipient: &str) -> anyhow::Result<()> {
        outbound::set_typing(&self.api, recipient, true).await?;
        Ok(())
    }

    async fn stop_typing(&self, recipient: &str) -> anyhow::Result<()> {
        outbound::set_typing(&self.api, recipient, false).await?;
        Ok(())
    }
}
