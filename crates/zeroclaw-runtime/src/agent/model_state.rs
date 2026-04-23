//! Model runtime state: per-model observations persisted to `~/.zeroclaw/model_state.json`.
//!
//! # Overview
//!
//! This module tracks per-model runtime observations. The file is keyed by
//! route key (`provider/model`) at the top level, with each entry aggregating
//! all known state for that model.
//!
//! # Tracked state
//!
//! ## `can_pop_tool_calls`
//!
//! When `true`, the agent should pop all `tool`-role messages from the
//! conversation history before each LLM API call. When `false`, popping is
//! skipped because the model caches the full body hash and popping would
//! break the cache, leading to equal or higher cost.
//!
//! Determined via a warm-up test (see `warmup()`).
//!
//! ## `content_format`
//!
//! Auto-detected content format flags that describe how a model structures
//! its responses (think tags, native reasoning field, tool-calls in reasoning, etc.).
//!
//! These flags are accumulated across conversations — each response may reveal
//! new format characteristics. Used by the normalize/denormalize pipeline to
//! correctly separate content, reasoning, and tool-calls.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::RwLock;
use zeroclaw_api::provider::{ChatMessage, ChatRequest, Provider};

/// Directory name inside `$HOME/.zeroclaw/` where runtime state is stored.
pub const STATE_DIR: &str = ".zeroclaw";
/// File name for the model state store.
pub const STATE_FILE: &str = "model_state.json";

// ── State file schema ─────────────────────────────────────────────────────────

/// The on-disk model state file.
///
/// File location: `~/.zeroclaw/model_state.json`
///
/// Top-level key is the route key (`provider/model`). Each entry aggregates
/// all runtime observations for that model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelStateFile {
    /// Per-model state keyed by full `provider/model` route key.
    #[serde(default)]
    pub models: HashMap<String, ModelEntry>,

    // ── Legacy field (for migration from old format) ───────────────────────
    /// Old format: `can_pop_tool_calls` as top-level HashMap.
    /// Kept for deserialization compatibility; migrated to `models` on load.
    #[serde(default)]
    pub can_pop_tool_calls: HashMap<String, ModelPopEntry>,
}

/// Aggregated runtime state for a single model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelEntry {
    /// Whether tool-call entries should be popped before each API call.
    #[serde(default)]
    pub can_pop_tool_calls: bool,

    /// Detailed measurements from the pop-decision warm-up test.
    #[serde(default)]
    pub pop_decision: Option<ModelPopEntry>,

    /// Auto-detected content format flags.
    #[serde(default)]
    pub content_format: Option<ContentFormatEntry>,
}

/// A single model's warm-up result and the measurements that produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPopEntry {
    /// Whether tool-call entries should be popped before each API call.
    pub can_pop_tool_calls: bool,
    /// Input tokens in the warm-up call with tool-call entries present.
    pub input_before: u64,
    /// Cached tokens in the warm-up call with tool-call entries present.
    pub cache_before: u64,
    /// Input tokens in the second warm-up call after popping tool-call entries.
    pub input_after: u64,
    /// Cached tokens in the second warm-up call after popping.
    pub cache_after: u64,
}

/// Auto-detected content format flags for a model.
///
/// These flags accumulate over time — each response may reveal new
/// characteristics about how the model structures its output.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContentFormatEntry {
    /// The API returns a native `reasoning_content` field (separate from `content`).
    #[serde(default)]
    pub has_native_reasoning: bool,

    /// Model embeds `<think...</think*>` tags inside `content`.
    #[serde(default)]
    pub has_think_tags: bool,

    /// Model places tool-call XML/JSON inside `reasoning_content` or thinking blocks.
    #[serde(default)]
    pub has_tool_call_in_reasoning: bool,
}

// ── In-memory state ───────────────────────────────────────────────────────────

/// Model state tracker backed by `~/.zeroclaw/model_state.json`.
///
/// All reads and writes go through this struct. Provider implementations
/// query content format for denormalization and report detected format
/// via `update_content_format()`.
#[derive(Debug)]
pub struct ModelState {
    state_path: PathBuf,
    entries: RwLock<HashMap<String, ModelEntry>>,
}

impl ModelState {
    /// Load or create the state file at `~/.zeroclaw/model_state.json`.
    ///
    /// Handles migration from the old format (top-level `can_pop_tool_calls`)
    /// to the new format (top-level `models` with `ModelEntry` values).
    pub fn load(zeroclaw_dir: PathBuf) -> std::io::Result<Self> {
        let state_path = zeroclaw_dir.join(STATE_FILE);
        let entries = if state_path.exists() {
            let contents = fs::read_to_string(&state_path)?;
            match serde_json::from_str::<ModelStateFile>(&contents) {
                Ok(f) => {
                    let mut models = f.models;

                    // Migrate old format: can_pop_tool_calls → models
                    if models.is_empty() && !f.can_pop_tool_calls.is_empty() {
                        for (key, pop_entry) in &f.can_pop_tool_calls {
                            models.insert(
                                key.clone(),
                                ModelEntry {
                                    can_pop_tool_calls: pop_entry.can_pop_tool_calls,
                                    pop_decision: Some((*pop_entry).clone()),
                                    content_format: None,
                                },
                            );
                        }
                    }

                    models
                }
                Err(_) => HashMap::new(),
            }
        } else {
            HashMap::new()
        };
        Ok(Self {
            state_path,
            entries: RwLock::new(entries),
        })
    }

    /// Returns the full state file path.
    pub fn state_path(&self) -> &PathBuf {
        &self.state_path
    }

    // ── Pop-decision queries ───────────────────────────────────────────────

    /// Return `true` if `can_pop_tool_calls` is set to `true` for the given model.
    ///
    /// Returns `false` if the model has never been warm-up'd (entry absent).
    pub fn can_pop_tool_calls(&self, provider: &str, model: &str) -> bool {
        let key = format!("{provider}/{model}");
        if let Ok(entries) = self.entries.read() {
            if let Some(entry) = entries.get(&key) {
                return entry.can_pop_tool_calls;
            }
        }
        false
    }

    /// Run a warm-up test for `provider/model` and persist the result.
    ///
    /// The test constructs a minimal context, sends it, pops tool-role messages,
    /// sends the same context again, and records the input/cache measurements.
    /// The resulting `can_pop_tool_calls` value is written to `model_state.json`.
    pub async fn warmup(
        &self,
        provider: &dyn Provider,
        provider_name: &str,
        model: &str,
    ) -> anyhow::Result<bool> {
        tracing::info!(provider = provider_name, model, "Running model warm-up for can_pop_tool_calls");

        // Step 1: build context with one tool-call/result pair
        let system = ChatMessage::system("You are a helpful assistant.");
        let user = ChatMessage::user("What is 1+1?");
        let assistant = ChatMessage::assistant("Sure, let me calculate that.");
        let tool_call = ChatMessage {
            role: "assistant".into(),
            content: serde_json::json!({
                "tool_calls": [{
                    "function": {"name": "add", "arguments": {"a": 1, "b": 1}},
                    "id": "call_warmup_001",
                    "type": "function"
                }]
            })
            .to_string(),
        };
        let tool_result = ChatMessage {
            role: "tool".into(),
            content: serde_json::json!({
                "tool_call_id": "call_warmup_001",
                "content": "2"
            })
            .to_string(),
        };
        let messages = vec![system, user, assistant, tool_call, tool_result];

        // Step 2: first API call
        let (input_before, cache_before) =
            Self::call_and_extract_tokens(provider, model, &messages).await?;

        // Step 3: pop all tool-role messages
        let messages_no_tool: Vec<ChatMessage> =
            messages.into_iter().filter(|m| m.role != "tool").collect();

        // Step 4: second API call without tool messages
        let (input_after, cache_after) =
            Self::call_and_extract_tokens(provider, model, &messages_no_tool).await?;

        // Step 5: decide
        let can_pop = input_before > 0
            && input_after > 0
            && cache_after >= cache_before
            && input_after < input_before;

        let pop_entry = ModelPopEntry {
            can_pop_tool_calls: can_pop,
            input_before,
            cache_before,
            input_after,
            cache_after,
        };

        tracing::info!(
            provider = provider_name,
            model,
            can_pop,
            input_before,
            cache_before,
            input_after,
            cache_after,
            "Warm-up complete"
        );

        // Step 6: persist
        {
            let mut entries = self.entries.write().unwrap();
            let key = format!("{provider_name}/{model}");
            let entry = entries.entry(key).or_default();
            entry.can_pop_tool_calls = can_pop;
            entry.pop_decision = Some(pop_entry);
            drop(entries);
            self.flush()?;
        }

        Ok(can_pop)
    }

    // ── Content format queries ─────────────────────────────────────────────

    /// Get the content format entry for a model, if it exists.
    pub fn content_format(&self, provider: &str, model: &str) -> Option<ContentFormatEntry> {
        let key = format!("{provider}/{model}");
        if let Ok(entries) = self.entries.read() {
            if let Some(entry) = entries.get(&key) {
                return entry.content_format.clone();
            }
        }
        None
    }

    /// Update content format flags for a model.
    ///
    /// This merges new detections with existing flags (OR semantics) — once a
    /// flag is set to `true`, it stays `true`. The result is persisted to disk.
    pub fn update_content_format(
        &self,
        provider: &str,
        model: &str,
        new_flags: &ContentFormatEntry,
    ) {
        let key = format!("{provider}/{model}");
        let mut entries = match self.entries.write() {
            Ok(e) => e,
            Err(_) => return,
        };
        let entry = entries.entry(key).or_default();
        if let Some(ref mut existing) = entry.content_format {
            // Merge: OR semantics — once seen, stays true
            existing.has_native_reasoning |= new_flags.has_native_reasoning;
            existing.has_think_tags |= new_flags.has_think_tags;
            existing.has_tool_call_in_reasoning |= new_flags.has_tool_call_in_reasoning;
        } else {
            entry.content_format = Some(new_flags.clone());
        }
        drop(entries);

        if let Err(e) = self.flush() {
            tracing::warn!("Failed to persist content_format to model_state.json: {e}");
        }
    }

    // ── Internal helpers ───────────────────────────────────────────────────

    /// Call `provider.chat()` and extract `(input_tokens, cached_tokens)`.
    async fn call_and_extract_tokens(
        provider: &dyn Provider,
        model: &str,
        messages: &[ChatMessage],
    ) -> anyhow::Result<(u64, u64)> {
        let resp = provider
            .chat(
                ChatRequest {
                    messages,
                    tools: None,
                },
                model,
                0.0,
            )
            .await?;

        let usage = resp.usage.as_ref();
        let input_tokens = usage.and_then(|u| u.input_tokens).unwrap_or(0);
        let cached_tokens = usage
            .and_then(|u| u.cached_input_tokens)
            .unwrap_or(0);
        Ok((input_tokens, cached_tokens))
    }

    /// Persist the current in-memory state to disk.
    fn flush(&self) -> std::io::Result<()> {
        let entries = self.entries.read().unwrap();
        let file = ModelStateFile {
            models: entries.clone(),
            can_pop_tool_calls: HashMap::new(), // old field, now empty after migration
        };
        let dir = self.state_path.parent().unwrap();
        fs::create_dir_all(dir)?;
        let json = serde_json::to_string_pretty(&file)?;
        fs::write(&self.state_path, json)?;
        Ok(())
    }

    /// Remove tool-role messages from the given message list.
    ///
    /// This is the "pop" operation applied before each API call when
    /// `can_pop_tool_calls` returns `true`.
    pub fn pop_tool_call_entries(&self, messages: &mut Vec<ChatMessage>) {
        let mut i = 0;
        while i < messages.len() {
            if messages[i].role == "tool" {
                messages.remove(i);
            } else {
                i += 1;
            }
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_messages() -> Vec<ChatMessage> {
        vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("What's 2+2?"),
            ChatMessage::assistant("Let me calculate."),
            ChatMessage {
                role: "assistant".into(),
                content: serde_json::json!({
                    "tool_calls": [{
                        "function": {"name": "add", "arguments": {"a": 2, "b": 2}},
                        "id": "call_abc",
                        "type": "function"
                    }]
                })
                .to_string(),
            },
            ChatMessage {
                role: "tool".into(),
                content: serde_json::json!({
                    "tool_call_id": "call_abc",
                    "content": "4"
                })
                .to_string(),
            },
        ]
    }

    #[test]
    fn pop_tool_call_entries_removes_tool_messages() {
        let mut messages = make_messages();
        let state = ModelState::load(PathBuf::from("/tmp/zeroclaw_test")).unwrap();
        state.pop_tool_call_entries(&mut messages);

        assert!(messages.iter().all(|m| m.role != "tool"));
        assert_eq!(messages.len(), 3); // system + user + assistant
    }

    #[test]
    fn pop_tool_call_entries_idempotent() {
        let mut messages = make_messages();
        let state = ModelState::load(PathBuf::from("/tmp/zeroclaw_test")).unwrap();
        state.pop_tool_call_entries(&mut messages);
        state.pop_tool_call_entries(&mut messages);
        assert_eq!(messages.len(), 3);
    }

    #[test]
    fn content_format_merge_or_semantics() {
        let state = ModelState::load(PathBuf::from("/tmp/zeroclaw_test")).unwrap();

        // First detection: only native reasoning seen
        state.update_content_format("test", "model-a", &ContentFormatEntry {
            has_native_reasoning: true,
            has_think_tags: false,
            has_tool_call_in_reasoning: false,
        });

        // Second detection: think tags also seen
        state.update_content_format("test", "model-a", &ContentFormatEntry {
            has_native_reasoning: false,
            has_think_tags: true,
            has_tool_call_in_reasoning: false,
        });

        let fmt = state.content_format("test", "model-a").unwrap();
        assert!(fmt.has_native_reasoning);
        assert!(fmt.has_think_tags);
        assert!(!fmt.has_tool_call_in_reasoning);
    }

    #[test]
    fn migrate_old_format() {
        use std::io::Write;
        let tmp_dir = tempfile::tempdir().unwrap();
        let state_path = tmp_dir.path().join(STATE_FILE);

        // Write old-format file
        let old_json = r#"{
  "can_pop_tool_calls": {
    "my-provider/my-model": {
      "can_pop_tool_calls": true,
      "input_before": 100,
      "cache_before": 50,
      "input_after": 80,
      "cache_after": 50
    }
  }
}"#;
        let mut f = fs::File::create(&state_path).unwrap();
        f.write_all(old_json.as_bytes()).unwrap();
        drop(f);

        let state = ModelState::load(tmp_dir.path().to_path_buf()).unwrap();

        // Old data should be accessible via new API
        assert!(state.can_pop_tool_calls("my-provider", "my-model"));

        // Flush should write new format
        state.flush().unwrap();
        let new_contents = fs::read_to_string(&state_path).unwrap();
        let parsed: ModelStateFile = serde_json::from_str(&new_contents).unwrap();
        assert!(parsed.models.contains_key("my-provider/my-model"));
        // Old field should be empty after migration
        assert!(parsed.can_pop_tool_calls.is_empty());
    }
}
