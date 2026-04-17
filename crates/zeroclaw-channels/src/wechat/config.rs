/// Re-export the canonical WechatConfig from the schema crate.
///
/// The channel implementation references `config::WechatConfig` so this
/// module acts as a thin façade.  All serde / Configurable / JsonSchema
/// derives live in `zeroclaw-config/src/schema.rs`.
pub use zeroclaw_config::schema::WechatConfig;
