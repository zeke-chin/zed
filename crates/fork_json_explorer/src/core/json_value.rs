use std::collections::HashMap;

const MAX_DEPTH: usize = 512;
const LINEAR_LOOKUP_LIMIT: usize = 16;

/// A JSON document that keeps number literals exactly as written, so big
/// integers and high precision decimals survive a parse and print round trip.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(String),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    pub fn is_container(&self) -> bool {
        matches!(self, JsonValue::Array(_) | JsonValue::Object(_))
    }

    /// Strict JSON, same grammar as `JSON.parse`. Duplicate keys keep the first
    /// position and the last value. Lone surrogate escapes become U+FFFD.
    pub fn parse(text: &str) -> Option<JsonValue> {
        let mut parser = Parser {
            text,
            bytes: text.as_bytes(),
            position: 0,
        };
        parser.skip_whitespace();
        let value = parser.parse_value(0)?;
        parser.skip_whitespace();
        if parser.position == parser.bytes.len() {
            Some(value)
        } else {
            None
        }
    }

    pub fn to_compact_string(&self) -> String {
        let mut output = String::new();
        self.write(&mut output, None, 0);
        output
    }

    /// Same layout as `JSON.stringify(value, null, '\t')`.
    pub fn to_pretty_string(&self) -> String {
        let mut output = String::new();
        self.write(&mut output, Some("\t"), 0);
        output
    }

    fn write(&self, output: &mut String, indent: Option<&str>, level: usize) {
        match self {
            JsonValue::Null => output.push_str("null"),
            JsonValue::Bool(true) => output.push_str("true"),
            JsonValue::Bool(false) => output.push_str("false"),
            JsonValue::Number(literal) => output.push_str(literal),
            JsonValue::String(text) => write_quoted(output, text),
            JsonValue::Array(items) => {
                if items.is_empty() {
                    output.push_str("[]");
                    return;
                }
                output.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    write_line_break(output, indent, level + 1);
                    item.write(output, indent, level + 1);
                }
                write_line_break(output, indent, level);
                output.push(']');
            }
            JsonValue::Object(members) => {
                if members.is_empty() {
                    output.push_str("{}");
                    return;
                }
                output.push('{');
                for (index, (key, value)) in members.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    write_line_break(output, indent, level + 1);
                    write_quoted(output, key);
                    output.push(':');
                    if indent.is_some() {
                        output.push(' ');
                    }
                    value.write(output, indent, level + 1);
                }
                write_line_break(output, indent, level);
                output.push('}');
            }
        }
    }
}

fn write_line_break(output: &mut String, indent: Option<&str>, level: usize) {
    if let Some(unit) = indent {
        output.push('\n');
        for _ in 0..level {
            output.push_str(unit);
        }
    }
}

/// Equivalent of `JSON.stringify(text)` for a string.
pub fn quote_json_string(text: &str) -> String {
    let mut output = String::with_capacity(text.len() + 2);
    write_quoted(&mut output, text);
    output
}

fn write_quoted(output: &mut String, text: &str) {
    output.push('"');
    for character in text.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                output.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => output.push(other),
        }
    }
    output.push('"');
}

/// Whitespace as defined by ECMAScript `\s` and `String.prototype.trim`.
pub(super) fn is_script_whitespace(character: char) -> bool {
    matches!(
        character,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

pub(super) fn script_trim(text: &str) -> &str {
    text.trim_matches(is_script_whitespace)
}

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    position: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.position += 1;
        }
    }

    fn consume_literal(&mut self, literal: &str) -> bool {
        if self
            .bytes
            .get(self.position..)
            .is_some_and(|rest| rest.starts_with(literal.as_bytes()))
        {
            self.position += literal.len();
            true
        } else {
            false
        }
    }

    fn parse_value(&mut self, depth: usize) -> Option<JsonValue> {
        if depth > MAX_DEPTH {
            return None;
        }
        match self.peek()? {
            b'{' => self.parse_object(depth),
            b'[' => self.parse_array(depth),
            b'"' => self.parse_string().map(JsonValue::String),
            b't' => self
                .consume_literal("true")
                .then_some(JsonValue::Bool(true)),
            b'f' => self
                .consume_literal("false")
                .then_some(JsonValue::Bool(false)),
            b'n' => self.consume_literal("null").then_some(JsonValue::Null),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => None,
        }
    }

    fn consume_digits(&mut self) -> usize {
        let start = self.position;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.position += 1;
        }
        self.position - start
    }

    fn parse_number(&mut self) -> Option<JsonValue> {
        let start = self.position;
        if self.peek() == Some(b'-') {
            self.position += 1;
        }
        match self.peek()? {
            b'0' => self.position += 1,
            b'1'..=b'9' => {
                self.consume_digits();
            }
            _ => return None,
        }
        if self.peek() == Some(b'.') {
            self.position += 1;
            if self.consume_digits() == 0 {
                return None;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.position += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.position += 1;
            }
            if self.consume_digits() == 0 {
                return None;
            }
        }
        let literal = self.text.get(start..self.position)?;
        Some(JsonValue::Number(literal.to_string()))
    }

    fn parse_hex_quad(&mut self) -> Option<u16> {
        let digits = self.bytes.get(self.position..self.position + 4)?;
        let mut value: u16 = 0;
        for digit in digits {
            let nibble = char::from(*digit).to_digit(16)?;
            value = value * 16 + u16::try_from(nibble).ok()?;
        }
        self.position += 4;
        Some(value)
    }

    fn parse_string(&mut self) -> Option<String> {
        self.position += 1;
        let mut output = String::new();
        let mut run_start = self.position;
        loop {
            let byte = self.peek()?;
            match byte {
                b'"' => {
                    output.push_str(self.text.get(run_start..self.position)?);
                    self.position += 1;
                    return Some(output);
                }
                b'\\' => {
                    output.push_str(self.text.get(run_start..self.position)?);
                    self.position += 1;
                    let escape = self.peek()?;
                    self.position += 1;
                    match escape {
                        b'"' => output.push('"'),
                        b'\\' => output.push('\\'),
                        b'/' => output.push('/'),
                        b'b' => output.push('\u{8}'),
                        b'f' => output.push('\u{c}'),
                        b'n' => output.push('\n'),
                        b'r' => output.push('\r'),
                        b't' => output.push('\t'),
                        b'u' => {
                            let unit = self.parse_hex_quad()?;
                            output.push(self.resolve_surrogates(unit)?);
                        }
                        _ => return None,
                    }
                    run_start = self.position;
                }
                0..=0x1f => return None,
                _ => self.position += 1,
            }
        }
    }

    fn resolve_surrogates(&mut self, unit: u16) -> Option<char> {
        if (0xD800..0xDC00).contains(&unit) {
            let following = self.bytes.get(self.position..self.position + 2);
            if following == Some(b"\\u") {
                let saved = self.position;
                self.position += 2;
                match self.parse_hex_quad() {
                    Some(low) if (0xDC00..0xE000).contains(&low) => {
                        let combined = 0x10000
                            + ((u32::from(unit) - 0xD800) << 10)
                            + (u32::from(low) - 0xDC00);
                        return Some(char::from_u32(combined).unwrap_or('\u{FFFD}'));
                    }
                    _ => self.position = saved,
                }
            }
            return Some('\u{FFFD}');
        }
        Some(char::from_u32(u32::from(unit)).unwrap_or('\u{FFFD}'))
    }

    fn parse_array(&mut self, depth: usize) -> Option<JsonValue> {
        self.position += 1;
        self.skip_whitespace();
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            self.position += 1;
            return Some(JsonValue::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.parse_value(depth + 1)?);
            self.skip_whitespace();
            match self.peek()? {
                b',' => self.position += 1,
                b']' => {
                    self.position += 1;
                    return Some(JsonValue::Array(items));
                }
                _ => return None,
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> Option<JsonValue> {
        self.position += 1;
        self.skip_whitespace();
        let mut members: Vec<(String, JsonValue)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        if self.peek() == Some(b'}') {
            self.position += 1;
            return Some(JsonValue::Object(members));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return None;
            }
            let key = self.parse_string()?;
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return None;
            }
            self.position += 1;
            self.skip_whitespace();
            let value = self.parse_value(depth + 1)?;
            insert_member(&mut members, &mut index, key, value);
            self.skip_whitespace();
            match self.peek()? {
                b',' => self.position += 1,
                b'}' => {
                    self.position += 1;
                    return Some(JsonValue::Object(members));
                }
                _ => return None,
            }
        }
    }
}

fn insert_member(
    members: &mut Vec<(String, JsonValue)>,
    index: &mut HashMap<String, usize>,
    key: String,
    value: JsonValue,
) {
    if members.len() >= LINEAR_LOOKUP_LIMIT && index.is_empty() {
        for (position, (existing_key, _)) in members.iter().enumerate() {
            index.insert(existing_key.clone(), position);
        }
    }
    let existing = if index.is_empty() {
        members
            .iter()
            .position(|(existing_key, _)| *existing_key == key)
    } else {
        index.get(&key).copied()
    };
    match existing {
        Some(position) => {
            if let Some(member) = members.get_mut(position) {
                member.1 = value;
            }
        }
        None => {
            if !index.is_empty() {
                index.insert(key.clone(), members.len());
            }
            members.push((key, value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_key_order_and_number_text() {
        let text = r#"{"z":1,"a":12345678901234567890,"m":[0.1000000000000000055,1.0,1E2,-0]}"#;
        let value = JsonValue::parse(text).unwrap();
        assert_eq!(value.to_compact_string(), text);
    }

    #[test]
    fn duplicate_keys_keep_first_position_and_last_value() {
        let value = JsonValue::parse(r#"{"a":1,"b":2,"a":3}"#).unwrap();
        assert_eq!(value.to_compact_string(), r#"{"a":3,"b":2}"#);
    }

    #[test]
    fn duplicate_keys_in_large_objects() {
        let mut text = String::from("{");
        for index in 0..40 {
            text.push_str(&format!("\"k{index}\":{index},"));
        }
        text.push_str("\"k3\":\"x\"}");
        let value = JsonValue::parse(&text).unwrap();
        let JsonValue::Object(members) = value else {
            panic!("expected object");
        };
        assert_eq!(members.len(), 40);
        assert_eq!(
            members[3],
            ("k3".to_string(), JsonValue::String("x".into()))
        );
    }

    #[test]
    fn pretty_layout_matches_json_stringify() {
        let value = JsonValue::parse(r#"{"a":[],"b":{},"c":[1,{"d":null}]}"#).unwrap();
        assert_eq!(
            value.to_pretty_string(),
            "{\n\t\"a\": [],\n\t\"b\": {},\n\t\"c\": [\n\t\t1,\n\t\t{\n\t\t\t\"d\": null\n\t\t}\n\t]\n}"
        );
    }

    #[test]
    fn string_escaping_matches_json_stringify() {
        let value =
            JsonValue::parse(r#""a\"b\\c\/d\b\f\n\r\t\u0001\u007f\u00e9\ud83d\ude00""#).unwrap();
        assert_eq!(
            value.to_compact_string(),
            "\"a\\\"b\\\\c/d\\b\\f\\n\\r\\t\\u0001\u{7f}é😀\""
        );
    }

    #[test]
    fn rejects_invalid_json() {
        for text in [
            "",
            "{a:1}",
            "[1,]",
            "{\"a\":1,}",
            "01",
            "1.",
            ".5",
            "+1",
            "'a'",
            "\"a\nb\"",
            "[1] x",
            "nul",
            "{\"a\"}",
            "\"\\x\"",
            "\"\\u12\"",
        ] {
            assert_eq!(JsonValue::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn lone_surrogate_becomes_replacement_character() {
        let value = JsonValue::parse(r#""\ud800x""#).unwrap();
        assert_eq!(value, JsonValue::String("\u{FFFD}x".into()));
    }

    #[test]
    fn depth_limit_returns_none_instead_of_overflowing() {
        assert_eq!(JsonValue::parse(&"[".repeat(100_000)), None);
    }

    #[test]
    fn script_trim_covers_bom_and_nbsp() {
        assert_eq!(script_trim("\u{feff}\u{a0} x \u{2028}"), "x");
    }
}
