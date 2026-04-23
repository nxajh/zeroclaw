//! Content normalization utilities for provider implementations.
//!
//! These functions are shared tools for parsing model responses. Each provider
//! decides how and when to call them - this module does not impose a processing
//! pipeline.

// ── Constants ─────────────────────────────────────────────────────────────────

const THINK_OPEN: &str = "<think";
const THINK_CLOSE: &str = "</think";

// ── Think tag operations ──────────────────────────────────────────────────────

/// Check whether content contains <think tags.
pub fn has_think_tags(content: &str) -> bool {
    content.contains(THINK_OPEN)
}

/// Extract <think...</think* blocks from content.
///
/// Returns `(remaining_text, extracted_thinking)`.
pub fn extract_think_tags(content: &str) -> (String, Option<String>) {
    if !has_think_tags(content) {
        return (content.to_string(), None);
    }

    let mut remaining = String::with_capacity(content.len());
    let mut thinking = String::new();
    let mut rest = content;

    loop {
        if let Some(start) = rest.find(THINK_OPEN) {
            remaining.push_str(&rest[..start]);
            let tag_rest = &rest[start + THINK_OPEN.len()..];
            if let Some(tag_end) = tag_rest.find(">") {
                let close_offset = start + THINK_OPEN.len() + tag_end + 1;
                if let Some(end) = rest[close_offset..].find(THINK_CLOSE) {
                    let think_content = &rest[close_offset..close_offset + end];
                    if !thinking.is_empty() {
                        thinking.push('\n');
                    }
                    thinking.push_str(think_content.trim());
                    let mut after_close = close_offset + end + THINK_CLOSE.len();
                    let after_rest = &rest[after_close..];
                    if let Some(c) = after_rest.chars().next() {
                        if c == '>' {
                            after_close += 1;
                        }
                    }
                    rest = &rest[after_close..];
                } else {
                    let think_content = &rest[close_offset..];
                    if !thinking.is_empty() {
                        thinking.push('\n');
                    }
                    thinking.push_str(think_content.trim());
                    break;
                }
            } else {
                remaining.push_str(rest);
                break;
            }
        } else {
            remaining.push_str(rest);
            break;
        }
    }

    let remaining = remaining.trim().to_string();
    let thinking = if thinking.is_empty() { None } else { Some(thinking) };
    (remaining, thinking)
}

/// Strip <think...</think* tags from content, returning only visible text.
pub fn strip_think_tags(content: &str) -> String {
    extract_think_tags(content).0
}

// ── Tool-call XML detection ───────────────────────────────────────────────────

/// Quick check for tool-call XML markers (used by prompt-guided tool models).
///
/// Looks for the unicode bracket pair that wraps embedded tool-call JSON.
pub fn has_tool_call_xml(content: &str) -> bool {
    content.contains("\u{2768}")
}

// ── Tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_think_tags_removes_single_block() {
        let input = "visible<think>reasoning</think>rest";
        assert_eq!(strip_think_tags(input), "visible rest");
    }

    #[test]
    fn strip_think_tags_drops_unclosed_block() {
        let input = "visible<think>hidden";
        assert_eq!(strip_think_tags(input), "visible");
    }

    #[test]
    fn strip_think_tags_preserves_text_without_tags() {
        assert_eq!(strip_think_tags("no tags here"), "no tags here");
    }

    #[test]
    fn strip_think_tags_handles_empty() {
        assert_eq!(strip_think_tags(""), "");
    }

    #[test]
    fn extract_think_tags_splits_single_block() {
        let (text, thinking) = extract_think_tags("hello<think>reasoning</think>world");
        assert_eq!(text, "hello world");
        assert_eq!(thinking, Some("reasoning".to_string()));
    }

    #[test]
    fn extract_think_tags_splits_multiple_blocks() {
        let (text, thinking) =
            extract_think_tags("A <think>hidden 1</think> and B <think>hidden 2</think> done");
        assert_eq!(text, "A  and B  done");
        assert_eq!(thinking, Some("hidden 1\nhidden 2".to_string()));
    }

    #[test]
    fn extract_think_tags_unclosed_block() {
        let (text, thinking) = extract_think_tags("Visible<think>hidden tail");
        assert_eq!(text, "Visible");
        assert_eq!(thinking, Some("hidden tail".to_string()));
    }

    #[test]
    fn extract_think_tags_no_tags() {
        let (text, thinking) = extract_think_tags("just text");
        assert_eq!(text, "just text");
        assert!(thinking.is_none());
    }

    #[test]
    fn extract_think_tags_only_thinking() {
        let (text, thinking) = extract_think_tags("<think>only reasoning</think>");
        assert_eq!(text, "");
        assert_eq!(thinking, Some("only reasoning".to_string()));
    }

    #[test]
    fn extract_think_tags_empty_think_block() {
        let (text, thinking) = extract_think_tags("before<think></think>after");
        assert_eq!(text, "beforeafter");
        assert!(thinking.is_none());
    }

    #[test]
    fn has_tool_call_xml_detects_markers() {
        assert!(has_tool_call_xml("❨{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}❩"));
        assert!(!has_tool_call_xml("no tool calls here"));
        assert!(!has_tool_call_xml(""));
    }
}
