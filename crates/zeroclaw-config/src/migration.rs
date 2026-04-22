//! Config schema migration.
//!
//! Handles TOML-level transformations (field renames, restructures) that
//! `#[serde]` attributes cannot capture. The on-disk file is never rewritten
//! by migration — it runs in-memory only.
//!
//! ## Schema versioning
//!
//! Only bump the version when fields are **renamed, moved, or removed**.
//! New fields with `#[serde(default)]` don't need a bump.

use anyhow::{Context, Result};
use toml_edit::DocumentMut;

pub const CURRENT_SCHEMA_VERSION: u32 = 3;

/// Top-level keys that may appear in older config files.
/// Used by the unknown-key detector to suppress false "unknown key" warnings
/// for fields that were valid in previous schema versions.
pub const V1_LEGACY_KEYS: &[&str] = &[
    "api_key",
    "api_url",
    "api_path",
    "default_provider",
    "model_provider",
    "default_model",
    "model",
    "default_temperature",
    "provider_timeout_secs",
    "provider_max_tokens",
    "extra_headers",
    "model_providers",
    "model_routes",
    "embedding_routes",
    "channels_config",
];

/// Pre-deserialization table migration for nested field changes that
/// `#[serde]` cannot capture (e.g. removing a field from a nested
/// struct and moving its value elsewhere).
///
/// Called on the raw `toml::Table` before it is deserialized into `Config`.
pub fn prepare_table(table: &mut toml::Table) {
    // Migrate channels_config.matrix.room_id → channels_config.matrix.allowed_rooms
    for key in &["channels_config", "channels"] {
        if let Some(toml::Value::Table(channels)) = table.get_mut(*key)
            && let Some(toml::Value::Table(matrix)) = channels.get_mut("matrix")
            && let Some(toml::Value::String(room_id)) = matrix.remove("room_id")
            && !room_id.is_empty()
        {
            let rooms = matrix
                .entry("allowed_rooms")
                .or_insert_with(|| toml::Value::Array(Vec::new()));
            if let toml::Value::Array(arr) = rooms {
                let already_present = arr.iter().any(|v| v.as_str() == Some(room_id.as_str()));
                if !already_present {
                    arr.push(toml::Value::String(room_id));
                }
            }
        }
    }

    // Migrate channels.slack.channel_id → channels.slack.channel_ids
    for key in &["channels_config", "channels"] {
        if let Some(toml::Value::Table(channels)) = table.get_mut(*key)
            && let Some(toml::Value::Table(slack)) = channels.get_mut("slack")
            && let Some(toml::Value::String(channel_id)) = slack.remove("channel_id")
            && !channel_id.is_empty()
            && channel_id != "*"
        {
            let ids = slack
                .entry("channel_ids")
                .or_insert_with(|| toml::Value::Array(Vec::new()));
            if let toml::Value::Array(arr) = ids {
                let already_present = arr.iter().any(|v| v.as_str() == Some(channel_id.as_str()));
                if !already_present {
                    arr.push(toml::Value::String(channel_id));
                }
            }
        }
    }

    // Rename legacy `channels_config` key to `channels`
    if table.contains_key("channels_config")
        && !table.contains_key("channels")
        && let Some(val) = table.remove("channels_config")
    {
        table.insert("channels".to_string(), val);
    }

    // Fresh/new configs without an explicit schema_version are already
    // structurally at the current version after prepare_table runs.
    if !table.contains_key("schema_version") {
        table.insert(
            "schema_version".to_string(),
            toml::Value::Integer(CURRENT_SCHEMA_VERSION as i64),
        );
    }
}

// ── File-level migration (comment-preserving) ───────────────────────────────
//
// Computes the migrated Config, then syncs the original toml_edit document
// to match. The sync function is generic — it doesn't know field names, it
// just diffs two table structures.

/// Migrate a raw TOML config file, preserving comments and formatting.
/// Returns `None` if already at current version.
pub fn migrate_file(raw: &str) -> Result<Option<String>> {
    let mut table: toml::Table = toml::from_str(raw).context("Failed to parse config table")?;
    let original_table = table.clone();
    prepare_table(&mut table);
    let structural_changes = table != original_table;
    let prepared = toml::to_string(&table).context("Failed to re-serialize prepared table")?;

    let mut config: super::schema::Config =
        toml::from_str(&prepared).context("Failed to deserialize config")?;

    if config.schema_version >= CURRENT_SCHEMA_VERSION && !structural_changes {
        return Ok(None);
    }

    config.schema_version = CURRENT_SCHEMA_VERSION;

    tracing::info!(
        to = CURRENT_SCHEMA_VERSION,
        "Config schema migrated in-memory to version {CURRENT_SCHEMA_VERSION}. \
         Run `zeroclaw config migrate` to update the file on disk.",
    );

    // Serialize the migrated config to get the target table structure.
    let target: toml::Table = toml::from_str(&toml::to_string(&config)?)
        .context("Failed to round-trip migrated config")?;

    // Sync the original document (with comments) to match the target.
    let mut doc: DocumentMut = raw.parse().context("Failed to parse config.toml")?;

    // Rename channels_config → channels in the document to preserve comments.
    if doc.contains_key("channels_config")
        && !doc.contains_key("channels")
        && let Some(val) = doc.remove("channels_config")
    {
        doc.insert("channels", val);
    }

    sync_table(doc.as_table_mut(), &target);

    Ok(Some(doc.to_string()))
}

/// Recursively sync a `toml_edit` table to match a target `toml::Table`.
/// - Keys absent from target are removed.
/// - Keys present in target but not in doc are inserted.
/// - Sub-tables are recursed. Leaf values are updated only if changed.
/// - Unchanged entries retain their original formatting and comments.
pub fn sync_table(doc: &mut toml_edit::Table, target: &toml::Table) {
    // Remove keys not in target.
    let to_remove: Vec<String> = doc
        .iter()
        .map(|(k, _)| k.to_string())
        .filter(|k| !target.contains_key(k))
        .collect();
    for key in &to_remove {
        doc.remove(key);
    }

    // Add or update keys from target.
    for (key, target_value) in target {
        match target_value {
            toml::Value::Table(sub_target) => {
                let entry = doc
                    .entry(key)
                    .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
                if let Some(sub_doc) = entry.as_table_mut() {
                    sync_table(sub_doc, sub_target);
                }
            }
            _ => {
                if let Some(existing) = doc.get(key).and_then(|i| i.as_value()) {
                    // Compare raw values, ignoring formatting/comments.
                    if values_equal(existing, target_value) {
                        continue;
                    }
                }
                doc.insert(key, toml_edit::value(toml_to_edit_value(target_value)));
            }
        }
    }
}

/// Compare a `toml_edit::Value` and a `toml::Value` for semantic equality,
/// ignoring formatting, whitespace, and comments.
fn values_equal(edit: &toml_edit::Value, toml: &toml::Value) -> bool {
    match (edit, toml) {
        (toml_edit::Value::String(a), toml::Value::String(b)) => a.value() == b,
        (toml_edit::Value::Integer(a), toml::Value::Integer(b)) => a.value() == b,
        (toml_edit::Value::Float(a), toml::Value::Float(b)) => (a.value() - b).abs() < f64::EPSILON,
        (toml_edit::Value::Boolean(a), toml::Value::Boolean(b)) => a.value() == b,
        (toml_edit::Value::Array(a), toml::Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(ae, be)| values_equal(ae, be))
        }
        _ => false,
    }
}

/// Convert a `toml::Value` to a `toml_edit::Value`.
fn toml_to_edit_value(v: &toml::Value) -> toml_edit::Value {
    match v {
        toml::Value::String(s) => toml_edit::Value::from(s.as_str()),
        toml::Value::Integer(i) => toml_edit::Value::from(*i),
        toml::Value::Float(f) => toml_edit::Value::from(*f),
        toml::Value::Boolean(b) => toml_edit::Value::from(*b),
        toml::Value::Array(arr) => {
            let mut a = toml_edit::Array::new();
            for item in arr {
                a.push(toml_to_edit_value(item));
            }
            toml_edit::Value::Array(a)
        }
        toml::Value::Datetime(dt) => dt
            .to_string()
            .parse()
            .unwrap_or_else(|_| toml_edit::Value::from(dt.to_string())),
        toml::Value::Table(tbl) => {
            let mut inline = toml_edit::InlineTable::new();
            for (k, v) in tbl {
                inline.insert(k, toml_to_edit_value(v));
            }
            toml_edit::Value::InlineTable(inline)
        }
    }
}
