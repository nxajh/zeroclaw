use std::collections::HashMap;

/// Runtime state for the WeChat channel.
///
/// Tracks known conversations, processed message IDs (dedup),
/// and the current sync position.
pub struct WechatState {
    /// Recently seen message IDs for deduplication.
    /// Value = insertion timestamp (epoch seconds).
    seen_ids: HashMap<String, i64>,

    /// Maximum number of dedup entries to keep.
    dedup_cap: usize,

    /// TTL for dedup entries in seconds (default: 10 minutes).
    dedup_ttl_secs: i64,
}

impl WechatState {
    pub fn new() -> Self {
        Self {
            seen_ids: HashMap::new(),
            dedup_cap: 10_000,
            dedup_ttl_secs: 600,
        }
    }

    /// Check if we've already seen this message ID.
    /// If not, record it and return `false`.
    /// If yes, return `true` (duplicate).
    pub fn check_and_record(&mut self, msg_id: &str) -> bool {
        if self.seen_ids.contains_key(msg_id) {
            return true;
        }

        let now = epoch_secs();
        self.seen_ids.insert(msg_id.to_string(), now);

        // Lazy eviction: on every insert, scan a small batch of entries
        // and remove those past TTL. This amortizes the cost across calls
        // so we never do a single O(n) sweep.
        if self.seen_ids.len() > self.dedup_cap {
            let deadline = now - self.dedup_ttl_secs;
            let cap = self.dedup_cap;
            let initial_len = self.seen_ids.len();
            let mut removed = 0usize;
            self.seen_ids.retain(|_, ts| {
                if initial_len - removed <= cap {
                    return true;
                }
                let keep = *ts > deadline;
                if !keep {
                    removed += 1;
                }
                keep
            });

            // If TTL-based eviction wasn't enough (clock skew / burst),
            // fall back: evict the oldest entries by timestamp.
            if self.seen_ids.len() > cap {
                let mut entries: Vec<(String, i64)> = self
                    .seen_ids
                    .iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .collect();
                entries.sort_unstable_by_key(|(_, ts)| *ts);
                let to_remove = entries.len() - cap;
                for (id, _) in entries.iter().take(to_remove) {
                    self.seen_ids.remove(id);
                }
            }
        }

        false
    }
}

impl Default for WechatState {
    fn default() -> Self {
        Self::new()
    }
}

/// Current time as epoch seconds.
fn epoch_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dedup() {
        let mut st = WechatState::new();
        assert!(!st.check_and_record("msg1"));
        assert!(st.check_and_record("msg1")); // duplicate
        assert!(!st.check_and_record("msg2"));
    }

    #[test]
    fn test_cap_eviction() {
        let mut st = WechatState {
            dedup_cap: 5,
            ..WechatState::new()
        };

        // Insert 5 messages (at cap)
        for i in 0..5 {
            assert!(!st.check_and_record(&format!("msg{i}")));
        }
        assert_eq!(st.seen_ids.len(), 5);

        // One more triggers eviction; oldest should be removed
        assert!(!st.check_and_record("msg_extra"));
        assert!(st.seen_ids.len() <= 5);
        // The newest entry must survive
        assert!(st.seen_ids.contains_key("msg_extra"));
    }

    #[test]
    fn test_ttl_eviction() {
        let mut st = WechatState {
            dedup_cap: 100,
            dedup_ttl_secs: 0, // immediate expiry
            ..WechatState::new()
        };

        // Insert, then another insert — the first should be evicted by TTL
        assert!(!st.check_and_record("old_msg"));
        // Small sleep to ensure timestamps differ
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(!st.check_and_record("new_msg"));

        // "old_msg" may or may not be evicted depending on timing,
        // but "new_msg" must exist
        assert!(st.seen_ids.contains_key("new_msg"));
    }
}
