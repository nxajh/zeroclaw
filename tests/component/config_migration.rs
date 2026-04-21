//! Config Schema Migration Tests
//!
//! Validates migration behavior: table preparation, file-level migration,
//! and TOML round-trips. Adapted for v3 provider configuration.

use zeroclaw::config::migration::{self, CURRENT_SCHEMA_VERSION};
use zeroclaw::config::Config;

fn parse(toml_str: &str) -> Config {
    toml::from_str(toml_str).expect("failed to parse config")
}

fn prepare_and_parse(toml_str: &str) -> Config {
    let mut table: toml::Table = toml::from_str(toml_str).expect("failed to parse table");
    migration::prepare_table(&mut table);
    let prepared = toml::to_string(&table).expect("failed to re-serialize");
    toml::from_str(&prepared).expect("failed to deserialize config")
}

// ─────────────────────────────────────────────────────────────────────────────
// Table preparation: room_id → allowed_rooms migration
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn room_id_deduped_with_existing_allowed_rooms() {
    let config = prepare_and_parse(
        r#"
[channels_config.matrix]
homeserver = "https://matrix.org"
access_token = "tok"
room_id = "!abc:matrix.org"
allowed_users = ["@user:matrix.org"]
allowed_rooms = ["!abc:matrix.org", "!other:matrix.org"]
"#,
    );

    let matrix = config.channels.matrix.as_ref().unwrap();
    assert_eq!(matrix.allowed_rooms.len(), 2);
}

// ─────────────────────────────────────────────────────────────────────────────
// Schema version
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn empty_config_produces_valid_v3() {
    let config = prepare_and_parse("");
    assert_eq!(config.schema_version, CURRENT_SCHEMA_VERSION);
}

// ─────────────────────────────────────────────────────────────────────────────
// v3 provider config parsing
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn v3_provider_config_parses() {
    let config = parse(
        r#"
schema_version = 3

[[providers]]
name = "zai-cn"
api = "openai"
api_key = "sk-test"
base_url = "https://open.bigmodel.cn/api/paas/v4"

[[providers.model]]
model_id = "glm-5.1"
context_window = 128000
reasoning = true

[[providers.model]]
model_id = "glm-4-flash"
context_window = 128000
"#,
    );

    assert_eq!(config.schema_version, 3);
    assert_eq!(config.providers.len(), 1);
    assert_eq!(config.providers[0].name, "zai-cn");
    assert_eq!(config.providers[0].api, "openai");
    assert_eq!(config.providers[0].model.len(), 2);
    assert_eq!(config.providers[0].model[0].model_id, "glm-5.1");
    assert!(config.providers[0].model[0].reasoning);
    assert_eq!(config.providers[0].model[1].model_id, "glm-4-flash");
}

#[test]
fn v3_model_routes_parse() {
    let config = parse(
        r#"
schema_version = 3

[[providers]]
name = "openai"
api = "openai"
api_key = "sk-test"

[[providers.model]]
model_id = "gpt-4o"

[model_routes]
default = "openai/gpt-4o"
reasoning = "openai/o3"
"#,
    );

    assert_eq!(config.model_routes.len(), 2);
    assert_eq!(config.model_routes["default"], "openai/gpt-4o");
    assert_eq!(config.model_routes["reasoning"], "openai/o3");
}

#[test]
fn v3_multiple_providers() {
    let config = parse(
        r#"
schema_version = 3

[[providers]]
name = "openai"
api = "openai"
api_key = "sk-openai"

[[providers.model]]
model_id = "gpt-4o"

[[providers]]
name = "anthropic"
api = "anthropic"
api_key = "sk-ant"

[[providers.model]]
model_id = "claude-sonnet-4-6"
"#,
    );

    assert_eq!(config.providers.len(), 2);
    assert_eq!(config.providers[0].name, "openai");
    assert_eq!(config.providers[1].name, "anthropic");
}

#[test]
fn v3_resolve_model() {
    let config = parse(
        r#"
schema_version = 3

[agent]
default_model = "zai-cn/glm-5.1"

[[providers]]
name = "zai-cn"
api = "openai"

[[providers.model]]
model_id = "glm-5.1"
context_window = 128000
"#,
    );

    let resolved = config.resolve_model("zai-cn/glm-5.1").expect("should resolve");
    assert_eq!(resolved.model.model_id, "glm-5.1");
    assert_eq!(resolved.provider.name, "zai-cn");
    assert_eq!(resolved.model.context_window, Some(128000));
}

#[test]
fn v3_effective_model_uses_default() {
    let config = parse(
        r#"
schema_version = 3

[agent]
default_model = "openai/gpt-4o"

[[providers]]
name = "openai"
api = "openai"

[[providers.model]]
model_id = "gpt-4o"
"#,
    );

    let effective = config.effective_model(None).expect("should resolve default");
    assert_eq!(effective.model.model_id, "gpt-4o");
}

// ─────────────────────────────────────────────────────────────────────────────
// File-level migration (comment preservation)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn migrate_file_preserves_comments() {
    let raw = r#"
# Global settings
schema_version = 3

# Agent tuning
[agent]
max_tool_iterations = 5  # keep it tight

# Matrix channel
[channels.matrix]
homeserver = "https://matrix.org"  # production server
access_token = "tok"
room_id = "!abc:matrix.org"
allowed_users = ["@user:matrix.org"]
"#;
    let migrated = migration::migrate_file(raw)
        .unwrap()
        .expect("should migrate");

    assert!(
        migrated.contains("# Agent tuning"),
        "section comment preserved"
    );
    assert!(
        migrated.contains("# keep it tight"),
        "inline comment preserved"
    );
    assert!(
        migrated.contains("# production server"),
        "matrix inline comment preserved"
    );
    assert!(!migrated.contains("room_id"), "room_id removed");
}

#[test]
fn migrate_file_returns_none_when_current() {
    let raw = r#"
schema_version = 3

[[providers]]
name = "test"
api = "openai"
"#;
    assert!(migration::migrate_file(raw).unwrap().is_none());
}

#[test]
fn migrate_file_round_trips() {
    let raw = r#"
schema_version = 0

default_temperature = 0.5

[channels_config.matrix]
homeserver = "https://matrix.org"
access_token = "tok"
room_id = "!rt:matrix.org"
allowed_users = ["@u:m"]
"#;
    let migrated_toml = migration::migrate_file(raw)
        .unwrap()
        .expect("should migrate");

    let config: Config = toml::from_str(&migrated_toml).expect("migrated TOML should parse");
    assert_eq!(config.schema_version, CURRENT_SCHEMA_VERSION);

    let matrix = config.channels.matrix.as_ref().unwrap();
    // room_id is no longer on MatrixConfig; migration moves it to allowed_rooms.
    assert!(matrix.allowed_rooms.contains(&"!rt:matrix.org".to_string()));

    // Re-migrating should be a no-op.
    assert!(migration::migrate_file(&migrated_toml).unwrap().is_none());
}

// ─────────────────────────────────────────────────────────────────────────────
// Model cost config
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn v3_model_cost_config() {
    let config = parse(
        r#"
schema_version = 3

[[providers]]
name = "openai"
api = "openai"

[[providers.model]]
model_id = "gpt-4o"

[providers.model.cost]
input = 2.5
output = 10.0
reasoning = 15.0
cache_read = 1.25
cache_write = 2.5
"#,
    );

    let cost = config.providers[0].model[0].cost.as_ref().expect("cost should be set");
    assert!((cost.input - 2.5).abs() < f64::EPSILON);
    assert!((cost.output - 10.0).abs() < f64::EPSILON);
    assert_eq!(cost.reasoning, Some(15.0));
    assert_eq!(cost.cache_read, Some(1.25));
    assert_eq!(cost.cache_write, Some(2.5));
}

// ─────────────────────────────────────────────────────────────────────────────
// Realistic config: full pipeline (deserialize → validate)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn realistic_v3_config_validates() {
    let raw = r#"
schema_version = 3

[agent]
default_model = "openai/gpt-4o"
max_tool_iterations = 10

[[providers]]
name = "openai"
api = "openai"
api_key = "sk-test"

[[providers.model]]
model_id = "gpt-4o"
context_window = 128000
max_tokens = 16384

[model_routes]
default = "openai/gpt-4o"

[channels]
cli = true

[channels.matrix]
homeserver = "https://matrix.org"
access_token = "tok"
allowed_rooms = ["!abc:matrix.org"]
allowed_users = ["@user:matrix.org"]

[memory]
backend = "sqlite"
auto_save = true

[gateway]
port = 42617
host = "127.0.0.1"
require_pairing = true

[autonomy]
level = "supervised"
workspace_only = true

[observability]
backend = "none"
"#;
    let config: Config = toml::from_str(raw).expect("realistic v3 config should parse");
    assert_eq!(config.schema_version, 3);
    assert_eq!(config.providers.len(), 1);

    let resolved = config.resolve_model("openai/gpt-4o").expect("should resolve");
    assert_eq!(resolved.model.model_id, "gpt-4o");

    // Matrix rooms
    let matrix = config.channels.matrix.as_ref().unwrap();
    assert!(matrix.allowed_rooms.contains(&"!abc:matrix.org".to_string()));

    // Full validation pipeline must pass.
    config
        .validate()
        .expect("realistic v3 config should pass validation");
}
