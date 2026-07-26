//! Rolling conversation compaction. Full messages remain in SQLite; only the request context is
//! replaced by a summary plus recent messages.

use crate::provider::Message;

const KEEP_RECENT: usize = 6;
const DEFAULT_THRESHOLD_BYTES: usize = 48 * 1024;
/// History is never squeezed below this, however large the per-request overhead grows. Tool
/// schemas and the system prompt are not something compaction can shrink, so without a floor a
/// large enough MCP tool set would trigger a summarization on every single turn that could not
/// possibly bring the request back under budget.
const MIN_MESSAGE_BUDGET_BYTES: usize = 8 * 1024;

pub fn threshold(context_window: Option<u64>) -> usize {
    context_window.map_or(DEFAULT_THRESHOLD_BYTES, |window| {
        (window as usize).saturating_mul(4) / 2
    })
}

/// How many bytes of conversation history fit in the budget once the fixed parts of a request are
/// paid for. `overhead` is everything sent on every turn that is not history: the system prompt,
/// the memory snapshot, the compaction summary, and the tool schemas — which, with a few MCP
/// servers connected, can be larger than the history itself.
pub fn message_budget(context_window: Option<u64>, overhead: usize) -> usize {
    let threshold = threshold(context_window);
    // The floor never raises the budget above the threshold itself: it exists to stop overhead
    // from consuming the whole budget, not to overrule a deliberately small context window.
    let floor = MIN_MESSAGE_BUDGET_BYTES.min(threshold);
    threshold.saturating_sub(overhead).max(floor)
}

/// The bytes a set of messages contributes to a request: text, tool-call arguments, and any
/// base64 image payload — the last of which a single photo can dominate.
pub fn total_bytes(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|message| {
            message.content.len()
                + message
                    .tool_calls
                    .iter()
                    .map(|call| call.arguments.len())
                    .sum::<usize>()
                + message
                    .images
                    .iter()
                    .map(|image| image.data.len())
                    .sum::<usize>()
        })
        .sum()
}

pub fn cutoff(messages: &[Message]) -> Option<usize> {
    let mut cutoff = messages.len().saturating_sub(KEEP_RECENT);
    // A tool result must remain adjacent to its assistant tool request in the provider payload.
    while cutoff > 0 && messages[cutoff].role_name() == "tool" {
        cutoff -= 1;
    }
    (cutoff > 0).then_some(cutoff)
}

pub fn render(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|message| {
            let who = match message.role_name() {
                "user" => "User",
                "assistant" => "Assistant",
                "tool" => "Tool",
                "system" => "System",
                _ => "?",
            };
            if message.content.is_empty() && !message.tool_calls.is_empty() {
                let names = message
                    .tool_calls
                    .iter()
                    .map(|call| call.name.as_str())
                    .collect::<Vec<_>>();
                format!("{who} (called tools: {})", names.join(", "))
            } else {
                format!("{who}: {}", message.content)
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub fn summary_messages(existing: Option<&str>, rendered: &str) -> Vec<Message> {
    let instruction = "Maintain a concise running summary of this personal assistant conversation. Capture the user's goals, facts and preferences, key decisions, host actions and results, and open threads. Output only the updated summary.";
    vec![
        Message::system(instruction),
        Message::user(format!(
            "Prior summary:\n{}\n\nNew messages to fold in:\n{rendered}",
            existing.unwrap_or("(none yet)")
        )),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolCall;

    #[test]
    fn threshold_scales_with_context_window() {
        assert_eq!(threshold(Some(1000)), 2000);
        assert_eq!(threshold(None), DEFAULT_THRESHOLD_BYTES);
    }

    #[test]
    fn overhead_comes_out_of_the_history_budget() {
        // 1000 tokens -> 2000 bytes of request budget, of which the tool schemas take half.
        assert_eq!(message_budget(Some(1000), 0), 2000);
        assert_eq!(message_budget(Some(10_000), 4_000), 16_000);
    }

    #[test]
    fn history_keeps_a_floor_however_large_the_overhead() {
        // Tool schemas larger than the whole budget would otherwise ask for a summarization on
        // every turn, one that cannot shrink the thing actually over budget.
        assert_eq!(
            message_budget(Some(100_000), 1_000_000),
            MIN_MESSAGE_BUDGET_BYTES
        );
        // But the floor never overrules a deliberately small window.
        assert_eq!(message_budget(Some(1000), 1_000_000), 2000);
    }

    #[test]
    fn total_bytes_counts_image_payloads() {
        let text = Message::user("hello");
        let mut with_image = Message::user("look");
        with_image.images.push(crate::provider::ImageAttachment {
            media_type: "image/jpeg".into(),
            data: "x".repeat(2048),
        });

        // A photo dwarfs its caption, and used to be counted as nothing at all.
        assert_eq!(total_bytes(&[text]), "hello".len());
        assert_eq!(total_bytes(&[with_image]), "look".len() + 2048);
    }

    #[test]
    fn cutoff_preserves_six_recent_messages() {
        assert_eq!(cutoff(&vec![Message::user("x"); 6]), None);
        assert_eq!(cutoff(&vec![Message::user("x"); 10]), Some(4));
    }

    #[test]
    fn cutoff_does_not_split_tool_requests_from_results() {
        let mut messages = vec![Message::user("old"); 3];
        messages.push(Message::tool_request(
            "",
            vec![ToolCall {
                id: "1".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            }],
        ));
        messages.push(Message::tool_result("1", "body"));
        messages.extend(vec![Message::assistant("recent"); 5]);

        assert_eq!(cutoff(&messages), Some(3));
    }

    #[test]
    fn counts_and_renders_tool_calls() {
        let messages = vec![Message::tool_request(
            "",
            vec![ToolCall {
                id: "1".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            }],
        )];
        assert_eq!(total_bytes(&messages), 2);
        assert!(render(&messages).contains("called tools: read_file"));
    }
}
