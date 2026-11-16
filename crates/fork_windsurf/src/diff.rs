//! Turns a `GetTab` result into buffer edits. The response carries no
//! positions: `old_text` is the window that was sent as `selection` and
//! `new_text` is its rewrite, so the edits come from diffing the two.

use std::ops::Range;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PredictionError {
    /// The returned old text isn't what the buffer holds at the window.
    OldTextMismatch,
    /// The window doesn't fit in the buffer text.
    WindowOutOfBounds,
}

/// Returns the minimal edits, as byte ranges into `buffer_text`, that turn the
/// text at `window` from `old_text` into `new_text`. An empty list means there
/// is nothing to predict.
pub fn prediction_edits(
    buffer_text: &str,
    window: Range<usize>,
    old_text: &str,
    new_text: &str,
) -> Result<Vec<(Range<usize>, String)>, PredictionError> {
    let old_text = old_text.replace("\r\n", "\n");
    let new_text = new_text.replace("\r\n", "\n");
    let window_text = buffer_text
        .get(window.clone())
        .ok_or(PredictionError::WindowOutOfBounds)?;
    if window_text != old_text {
        return Err(PredictionError::OldTextMismatch);
    }
    if old_text == new_text || only_whitespace_differs(&old_text, &new_text) {
        return Ok(Vec::new());
    }

    let mut edits = Vec::new();
    for (range, replacement) in language::text_diff(&old_text, &new_text) {
        let (range, replacement) = narrow(&old_text[range.clone()], range.start, &replacement);
        if range.is_empty() && replacement.is_empty() {
            continue;
        }
        edits.push((
            window.start + range.start..window.start + range.end,
            replacement.to_string(),
        ));
    }
    Ok(edits)
}

fn only_whitespace_differs(old_text: &str, new_text: &str) -> bool {
    old_text
        .chars()
        .filter(|character| !character.is_whitespace())
        .eq(new_text
            .chars()
            .filter(|character| !character.is_whitespace()))
}

/// Trims the prefix and suffix the replaced text and its replacement have in
/// common, so that e.g. `l` → `length` becomes an insertion of `ength`.
fn narrow<'a>(old: &str, offset: usize, new: &'a str) -> (Range<usize>, &'a str) {
    let prefix = common_prefix_len(old, new);
    let (old_rest, new_rest) = (&old[prefix..], &new[prefix..]);
    let suffix = common_suffix_len(old_rest, new_rest);
    (
        offset + prefix..offset + old.len() - suffix,
        &new_rest[..new_rest.len() - suffix],
    )
}

fn common_prefix_len(left: &str, right: &str) -> usize {
    left.char_indices()
        .zip(right.chars())
        .find(|((_, left_char), right_char)| left_char != right_char)
        .map(|((index, _), _)| index)
        .unwrap_or_else(|| left.len().min(right.len()))
}

fn common_suffix_len(left: &str, right: &str) -> usize {
    left.chars()
        .rev()
        .zip(right.chars().rev())
        .take_while(|(left_char, right_char)| left_char == right_char)
        .map(|(character, _)| character.len_utf8())
        .sum()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn apply(text: &str, edits: &[(Range<usize>, String)]) -> String {
        let mut result = text.to_string();
        for (range, replacement) in edits.iter().rev() {
            result.replace_range(range.clone(), replacement);
        }
        result
    }

    /// Runs the conversion for a window given as a substring of `buffer` and
    /// checks that applying the edits produces the rewritten buffer.
    fn edits_for(buffer: &str, old: &str, new: &str) -> Vec<(Range<usize>, String)> {
        let start = buffer.find(old).expect("window should be in the buffer");
        let window = start..start + old.len();
        let edits =
            prediction_edits(buffer, window.clone(), old, new).expect("prediction should apply");
        let mut expected = buffer.to_string();
        expected.replace_range(window, new);
        assert_eq!(apply(buffer, &edits), expected);
        edits
    }

    #[test]
    fn test_pure_insertion_at_cursor() {
        let buffer = "fn main() {\n    let v = vec![1];\n    v.l\n}\n";
        let edits = edits_for(
            buffer,
            "    let v = vec![1];\n    v.l",
            "    let v = vec![1];\n    v.len()",
        );
        let offset = buffer.find("v.l").expect("cursor") + 3;
        assert_eq!(edits, vec![(offset..offset, "en()".to_string())]);
    }

    #[test]
    fn test_insertion_of_new_lines_after_window() {
        let buffer = "def f():\n    return 1\n";
        let edits = edits_for(
            buffer,
            "def f():\n    return 1",
            "def f():\n    return 1\n\n\ndef g():\n    return 2",
        );
        assert!(edits.iter().all(|(range, _)| range.is_empty()));
    }

    #[test]
    fn test_pure_deletion() {
        let buffer = "a = 1\nprint(a)\nprint(a)\nb = 2\n";
        let edits = edits_for(
            buffer,
            "a = 1\nprint(a)\nprint(a)\nb = 2",
            "a = 1\nprint(a)\nb = 2",
        );
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].1, "");
        assert_eq!(&buffer[edits[0].0.clone()], "print(a)\n");
    }

    #[test]
    fn test_single_line_replacement() {
        let buffer = "x = factor * 2\ny = factor + 1\n";
        let edits = edits_for(
            buffer,
            "x = factor * 2\ny = factor + 1",
            "x = k * 2\ny = k + 1",
        );
        assert_eq!(edits.len(), 2);
        for (range, replacement) in &edits {
            assert_eq!(&buffer[range.clone()], "factor");
            assert_eq!(replacement, "k");
        }
    }

    #[test]
    fn test_multi_line_replacement() {
        let buffer = "fn a() {\n    one();\n    two();\n    three();\n}\n";
        let old = "fn a() {\n    one();\n    two();\n    three();\n}";
        let new = "fn a() {\n    first();\n    second();\n    third();\n}";
        let edits = edits_for(buffer, old, new);
        assert!(!edits.is_empty());
        assert!(
            edits
                .iter()
                .all(|(range, _)| range.start >= buffer.find("one").expect("one"))
        );
    }

    #[test]
    fn test_window_at_start_and_end_of_file() {
        let buffer = "first\nsecond";
        let edits = edits_for(buffer, "first\nsecond", "zeroth\nfirst\nsecond\nthird");
        assert_eq!(
            edits,
            vec![
                (0..0, "zeroth\n".to_string()),
                (buffer.len()..buffer.len(), "\nthird".to_string()),
            ]
        );
    }

    #[test]
    fn test_multibyte_characters() {
        let buffer = "// 你好 🌍\nlet 名字 = \"世界\";\nprint(名);\n";
        let edits = edits_for(
            buffer,
            "// 你好 🌍\nlet 名字 = \"世界\";\nprint(名);",
            "// 你好 🌍\nlet 名字 = \"世界\";\nprint(名字);",
        );
        let offset = buffer.find("print(名").expect("cursor") + "print(名".len();
        assert_eq!(edits, vec![(offset..offset, "字".to_string())]);

        let emoji_buffer = "a 🌍 b\n";
        let edits = edits_for(emoji_buffer, "a 🌍 b", "a 🌏 b");
        let start = emoji_buffer.find('🌍').expect("emoji");
        assert_eq!(
            edits,
            vec![(start..start + '🌍'.len_utf8(), "🌏".to_string())]
        );
    }

    #[test]
    fn test_old_text_mismatch_is_rejected() {
        let buffer = "let a = 1;\nlet b = 2;\n";
        assert_eq!(
            prediction_edits(buffer, 0..10, "let a = 9;", "let a = 10;"),
            Err(PredictionError::OldTextMismatch)
        );
        assert_eq!(
            prediction_edits(buffer, 0..100, "let a = 1;", "let a = 10;"),
            Err(PredictionError::WindowOutOfBounds)
        );
        // Not on a character boundary.
        assert_eq!(
            prediction_edits("é", 0..1, "é", "e"),
            Err(PredictionError::WindowOutOfBounds)
        );
    }

    #[test]
    fn test_no_prediction_when_unchanged_or_whitespace_only() {
        let buffer = "if x:\n    y()\n";
        assert_eq!(
            prediction_edits(buffer, 0..13, "if x:\n    y()", "if x:\n    y()"),
            Ok(Vec::new())
        );
        assert_eq!(
            prediction_edits(buffer, 0..13, "if x:\n    y()", "if x:\n  y()  \n"),
            Ok(Vec::new())
        );
    }

    #[test]
    fn test_crlf_in_response_is_normalized() {
        let buffer = "a\nb\n";
        let edits = prediction_edits(buffer, 0..3, "a\r\nb", "a\r\nbc").expect("edits");
        assert_eq!(edits, vec![(3..3, "c".to_string())]);
    }

    #[test]
    fn test_narrow_respects_character_boundaries() {
        assert_eq!(narrow("名字", 10, "名称"), (13..16, "称"));
        assert_eq!(narrow("ab", 0, "ab"), (2..2, ""));
        assert_eq!(narrow("aXa", 0, "aa"), (1..2, ""));
    }
}
