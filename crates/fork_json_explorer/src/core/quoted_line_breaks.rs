//! Repair input copied from a terminal that broke lines mid-string. Real
//! newlines inside a quoted literal become `\n` escapes; newlines between
//! tokens stay as they are.
//!
//! Both `"..."` and `'...'` literals are recognized and backslash escapes are
//! honored. Python triple-quoted strings are not recognized.

pub fn escape_quoted_line_breaks(input: &str) -> String {
    if !input.contains(['\n', '\r']) {
        return input.to_string();
    }

    let mut output = String::with_capacity(input.len() + 8);
    let mut in_string: Option<char> = None;
    let mut escape = false;
    let mut characters = input.chars().peekable();

    while let Some(character) = characters.next() {
        let Some(quote) = in_string else {
            if character == '"' || character == '\'' {
                in_string = Some(character);
            }
            output.push(character);
            continue;
        };

        if escape {
            output.push(character);
            escape = false;
        } else if character == '\\' {
            output.push(character);
            escape = true;
        } else if character == quote {
            output.push(character);
            in_string = None;
        } else if character == '\n' || character == '\r' {
            if character == '\r' && characters.peek() == Some(&'\n') {
                characters.next();
            }
            output.push_str("\\n");
        } else {
            output.push(character);
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_without_line_breaks() {
        let input = r#"{"a":1,"b":[1,2,3]}"#;
        assert_eq!(escape_quoted_line_breaks(input), input);
        assert_eq!(escape_quoted_line_breaks(""), "");
    }

    #[test]
    fn unchanged_when_newlines_are_outside_strings() {
        let input = "{\n  \"a\": 1,\n  \"b\": 2\n}";
        assert_eq!(escape_quoted_line_breaks(input), input);
    }

    #[test]
    fn escapes_newlines_in_double_quoted_strings() {
        assert_eq!(
            escape_quoted_line_breaks("{\"a\":\"line1\nline2\"}"),
            "{\"a\":\"line1\\nline2\"}"
        );
        assert_eq!(
            escape_quoted_line_breaks("{\"a\":\"l1\nl2\nl3\"}"),
            "{\"a\":\"l1\\nl2\\nl3\"}"
        );
    }

    #[test]
    fn crlf_is_one_break_and_lone_cr_is_a_break() {
        assert_eq!(
            escape_quoted_line_breaks("{\"a\":\"l1\r\nl2\"}"),
            "{\"a\":\"l1\\nl2\"}"
        );
        assert_eq!(
            escape_quoted_line_breaks("{\"a\":\"l1\rl2\"}"),
            "{\"a\":\"l1\\nl2\"}"
        );
    }

    #[test]
    fn honors_escapes() {
        assert_eq!(
            escape_quoted_line_breaks("{\"a\":\"He said \\\"hi\\\"\nbye\"}"),
            "{\"a\":\"He said \\\"hi\\\"\\nbye\"}"
        );
        assert_eq!(
            escape_quoted_line_breaks("{\"a\":\"path\\\\\",\"b\":\"x\nx\"}"),
            "{\"a\":\"path\\\\\",\"b\":\"x\\nx\"}"
        );
    }

    #[test]
    fn recognizes_single_quoted_strings() {
        assert_eq!(
            escape_quoted_line_breaks("{'a': 'line1\nline2'}"),
            "{'a': 'line1\\nline2'}"
        );
    }

    #[test]
    fn mixed_quote_kinds() {
        assert_eq!(
            escape_quoted_line_breaks("{\"log\":\"{'a': 1,\n  'b': 2}\"}"),
            "{\"log\":\"{'a': 1,\\n  'b': 2}\"}"
        );
    }

    #[test]
    fn user_reported_terminal_wrap() {
        let input = "{\"request_id\":\"req-001\",\"session\":\"{'token': 'abc123', 'expires_in': 3600,\n  'roles':\n    ['admin', 'user']}\"}";
        let expected = "{\"request_id\":\"req-001\",\"session\":\"{'token': 'abc123', 'expires_in': 3600,\\n  'roles':\\n    ['admin', 'user']}\"}";
        assert_eq!(escape_quoted_line_breaks(input), expected);
    }

    #[test]
    fn leaves_top_level_newlines_alone() {
        assert_eq!(
            escape_quoted_line_breaks("{\"a\":\n  \"x\",\n  \"b\": \"y\nz\"}"),
            "{\"a\":\n  \"x\",\n  \"b\": \"y\\nz\"}"
        );
    }
}
