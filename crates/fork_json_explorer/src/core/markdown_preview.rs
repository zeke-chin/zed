/// A truncated markdown preview can leave a code fence open, which would swallow
/// everything appended after it. Closing the fence lets trailing content escape.
pub fn close_dangling_code_fence(markdown: &str) -> String {
    let mut fence_count = 0;
    let mut at_line_start = true;
    let mut remaining = markdown;
    while !remaining.is_empty() {
        if at_line_start && remaining.starts_with("```") {
            fence_count += 1;
        }
        let Some(character) = remaining.chars().next() else {
            break;
        };
        at_line_start = matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}');
        remaining = remaining.get(character.len_utf8()..).unwrap_or_default();
    }
    if fence_count % 2 == 1 {
        format!("{markdown}\n```")
    } else {
        markdown.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaves_fence_free_text_unchanged() {
        assert_eq!(close_dangling_code_fence("hello world"), "hello world");
    }

    #[test]
    fn leaves_balanced_fences_unchanged() {
        let markdown = "text\n```json\n{\"a\":1}\n```\nmore";
        assert_eq!(close_dangling_code_fence(markdown), markdown);
    }

    #[test]
    fn closes_single_dangling_fence() {
        let markdown = "text\n```json\n{\"a\":1}";
        assert_eq!(
            close_dangling_code_fence(markdown),
            format!("{markdown}\n```")
        );
    }

    #[test]
    fn closes_fence_with_language_tag() {
        let markdown = "```ts\nconst x = 1;";
        assert_eq!(
            close_dangling_code_fence(markdown),
            format!("{markdown}\n```")
        );
    }

    #[test]
    fn ignores_inline_backticks() {
        let markdown = "inline ``` not a fence";
        assert_eq!(close_dangling_code_fence(markdown), markdown);
    }
}
