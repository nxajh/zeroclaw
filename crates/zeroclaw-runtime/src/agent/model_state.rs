//! Model runtime state: pop-decision and warm-up for per-model tool-call trimming.
//!
//! # Overview
//!
//! This module tracks per-model runtime observations in `~/.zeroclaw/model_state.json`.
//! The primary purpose is to determine whether a model caches its prompt based on
//! the full body hash or on a prefix — which determines whether popping tool-call
//! entries from the context saves tokens.
//!
//! # `can_pop_tool_calls`
//!
//! When `true`, the agent should pop all `tool`-role messages from the conversation
//! history before each LLM API call. When `false`, popping is skipped because the
//! model caches the full body hash and popping would break the cache, leading to
//! equal or higher cost.
//!
//! # Warm-up flow
//!
//! The determination is made once via an in-process warm-up test:
//!
//! 1. Send an API call with a context that includes one tool-call/result pair.
//! 2. Record `input_before` and `cache_before` from the response usage.
//! 3. Pop all `tool`-role messages from the context.
//! 4. Send the same request again (without the tool messages).
//! 5. Record `input_after` and `cache_after`.
//! 6. Decide `can_pop_tool_calls` based on whether the cache survived.
//!
//! The result is persisted to `model_state.json` so subsequent restarts skip the
//! warm-up step.

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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelStateFile {
    /// Per-model warm-up results keyed by full `provider/model` name.
    #[serde(default)]
    pub can_pop_tool_calls: HashMap<String, ModelPopEntry>,
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

// ── In-memory state ───────────────────────────────────────────────────────────

/// Model state tracker backed by `~/.zeroclaw/model_state.json`.
///
/// Callers use `can_pop_tool_calls(provider, model)` to decide whether to trim
/// tool-role messages from the context. When the answer is unknown, `warmup()`
/// runs the measurement and records the result.
#[derive(Debug)]
pub struct ModelState {
    state_path: PathBuf,
    entries: RwLock<HashMap<String, ModelPopEntry>>,
}

impl ModelState {
    /// Load or create the state file at `~/.zeroclaw/model_state.json`.
    pub fn load(zeroclaw_dir: PathBuf) -> std::io::Result<Self> {
        let state_path = zeroclaw_dir.join(STATE_FILE);
        let entries = if state_path.exists() {
            let contents = fs::read_to_string(&state_path)?;
            match serde_json::from_str::<ModelStateFile>(&contents) {
                Ok(f) => f.can_pop_tool_calls,
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

        let entry = ModelPopEntry {
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
            entries.insert(key, entry.clone());
            drop(entries);
            self.flush()?;
        }

        Ok(can_pop)
    }

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
            can_pop_tool_calls: entries.clone(),
        };
        let dir = self.state_path.parent().unwrap();
        fs::create_dir_all(dir)?;
        let json = serde_json::to_string_pretty(&file)?;
        fs::write(&self.state_path, json)?;
        Ok(())
    }

    /// Remove tool-role messages and any assistant messages that have no following
    /// non-tool message from the given message list.
    ///
    /// This is the "pop" operation applied before each API call when
    /// `can_pop_tool_calls` returns `true`.
    pub fn pop_tool_call_entries(&self, messages: &mut Vec<ChatMessage>) {
        // Pass 1: remove tool messages
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
}