//! Converts common Markdown (as LLMs write it: `**bold**`, `_italic_`, `` `code` ``, fenced code
//! blocks, links) into Telegram's MarkdownV2 dialect, escaping every character MarkdownV2 treats as
//! reserved outside of an open entity. Telegram rejects a whole message if a reserved character is
//! left unescaped, so a naive "just forward the model's Markdown" approach fails unpredictably.

/// Characters MarkdownV2 requires to be escaped with a backslash outside of an entity.
const RESERVED: &[char] = &[
    '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!',
];

/// Convert `text` (assumed to be roughly CommonMark, as an LLM produces it) into a MarkdownV2
/// string safe to send with `ParseMode::MarkdownV2`. Fenced code blocks are passed through with
/// only backtick/backslash escaping (MarkdownV2 code spans do not otherwise interpret Markdown);
/// everything outside of them is escaped and re-marked with MarkdownV2 entities.
pub fn to_telegram_markdown_v2(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;

    while let Some(start) = rest.find("```") {
        out.push_str(&render_inline(&rest[..start]));
        let after_fence = &rest[start + 3..];
        // The fence's info string (e.g. "rust") runs to the end of its line and is dropped;
        // Telegram code blocks carry no language tag.
        let body_start = after_fence.find('\n').map_or(after_fence.len(), |i| i + 1);
        let body = &after_fence[body_start..];
        match body.find("```") {
            Some(end) => {
                out.push_str("```\n");
                out.push_str(&escape_code(&body[..end]));
                out.push_str("```");
                rest = &body[end + 3..];
            }
            None => {
                // Unterminated fence: treat the rest of the message as code rather than dropping it.
                out.push_str("```\n");
                out.push_str(&escape_code(body));
                out.push_str("```");
                rest = "";
            }
        }
    }
    out.push_str(&render_inline(rest));
    out
}

/// Escape text inside a `pre`/code block: MarkdownV2 only needs backtick and backslash escaped
/// there, since no other entity syntax is parsed inside one.
fn escape_code(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '`' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Render a fence-free segment: inline code spans, bold, italic, and links become MarkdownV2
/// entities; everything else is escaped as plain text.
fn render_inline(text: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '`' {
            if let Some(end) = find_char(&chars, i + 1, '`') {
                out.push('`');
                out.push_str(&escape_code(&chars[i + 1..end].iter().collect::<String>()));
                out.push('`');
                i = end + 1;
                continue;
            }
        }
        if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find_str(&chars, i + 2, "**") {
                out.push('*');
                out.push_str(&render_inline(
                    &chars[i + 2..end].iter().collect::<String>(),
                ));
                out.push('*');
                i = end + 2;
                continue;
            }
        }
        if chars[i] == '_' && i + 1 < chars.len() && chars[i + 1] == '_' {
            if let Some(end) = find_str(&chars, i + 2, "__") {
                out.push('*');
                out.push_str(&render_inline(
                    &chars[i + 2..end].iter().collect::<String>(),
                ));
                out.push('*');
                i = end + 2;
                continue;
            }
        }
        if chars[i] == '_' {
            if let Some(end) = find_char(&chars, i + 1, '_') {
                out.push('_');
                out.push_str(&render_inline(
                    &chars[i + 1..end].iter().collect::<String>(),
                ));
                out.push('_');
                i = end + 1;
                continue;
            }
        }
        // A URL containing its own unescaped ')' (rare) closes early here; full paren-matching
        // is not worth the complexity for model-generated links.
        if chars[i] == '['
            && let Some(close_bracket) = find_char(&chars, i + 1, ']')
            && chars.get(close_bracket + 1) == Some(&'(')
            && let Some(close_paren) = find_char(&chars, close_bracket + 2, ')')
        {
            let label: String = chars[i + 1..close_bracket].iter().collect();
            let url: String = chars[close_bracket + 2..close_paren].iter().collect();
            out.push('[');
            out.push_str(&render_inline(&label));
            out.push_str("](");
            out.push_str(&escape_url(&url));
            out.push(')');
            i = close_paren + 1;
            continue;
        }

        let ch = chars[i];
        if RESERVED.contains(&ch) || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
        i += 1;
    }
    out
}

/// Escape a URL for MarkdownV2's link target: only `)` and `\` are reserved there.
fn escape_url(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    for ch in url.chars() {
        if ch == ')' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn find_char(chars: &[char], from: usize, needle: char) -> Option<usize> {
    chars[from..]
        .iter()
        .position(|&c| c == needle)
        .map(|i| i + from)
}

fn find_str(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    if from + needle.len() > chars.len() {
        return None;
    }
    (from..=chars.len() - needle.len()).find(|&i| chars[i..i + needle.len()] == needle[..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_plain_reserved_characters() {
        let rendered = to_telegram_markdown_v2("Cost: $5.00 (really!)");
        assert_eq!(rendered, "Cost: $5\\.00 \\(really\\!\\)");
    }

    #[test]
    fn converts_bold_and_italic() {
        assert_eq!(to_telegram_markdown_v2("**bold**"), "*bold*");
        assert_eq!(to_telegram_markdown_v2("_italic_"), "_italic_");
        assert_eq!(to_telegram_markdown_v2("__also italic__"), "*also italic*");
    }

    #[test]
    fn converts_inline_code_without_interpreting_its_contents() {
        let rendered = to_telegram_markdown_v2("`a_b*c`");
        assert_eq!(rendered, "`a_b*c`");
    }

    #[test]
    fn converts_fenced_code_blocks_and_drops_the_language_tag() {
        let rendered = to_telegram_markdown_v2("```rust\nfn main() {}\n```");
        assert_eq!(rendered, "```\nfn main() {}\n```");
    }

    #[test]
    fn escapes_backticks_inside_fenced_code() {
        let rendered = to_telegram_markdown_v2("```\nlet s = `x`;\n```");
        assert_eq!(rendered, "```\nlet s = \\`x\\`;\n```");
    }

    #[test]
    fn converts_links_and_escapes_the_url() {
        let rendered = to_telegram_markdown_v2("[docs](https://example.com/a/b)");
        assert_eq!(rendered, "[docs](https://example.com/a/b)");
    }

    #[test]
    fn handles_an_unterminated_fence_without_dropping_content() {
        let rendered = to_telegram_markdown_v2("```\nno closing fence");
        assert_eq!(rendered, "```\nno closing fence```");
    }

    #[test]
    fn leaves_plain_text_without_markdown_untouched_in_meaning() {
        assert_eq!(to_telegram_markdown_v2("hello world"), "hello world");
    }
}
