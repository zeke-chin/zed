//! Python repr literal to JSON text. Covers the `ast.literal_eval` subset:
//! dict / list / tuple / str / int / float / True / False / None. Anything
//! outside it (calls, object references, bytes, sets, non-string keys) makes
//! the whole conversion return `None`.

const MAX_DEPTH: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonReprResult {
    pub json: String,
}

pub fn python_repr_to_json(input: &str) -> Option<PythonReprResult> {
    let mut state = State { input, position: 0 };
    state.skip_whitespace();
    if state.at_end() {
        return None;
    }
    let json = state.parse_value(0)?;
    state.skip_whitespace();
    if !state.at_end() {
        return None;
    }
    Some(PythonReprResult { json })
}

struct State<'a> {
    input: &'a str,
    position: usize,
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

impl State<'_> {
    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.position).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.input.as_bytes().get(self.position + offset).copied()
    }

    fn at_end(&self) -> bool {
        self.position >= self.input.len()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.position += 1;
        }
    }

    fn slice(&self, start: usize, end: usize) -> &str {
        self.input.get(start..end).unwrap_or_default()
    }

    fn parse_value(&mut self, depth: usize) -> Option<String> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.skip_whitespace();
        match self.peek()? {
            b'{' => self.parse_dict(depth),
            b'[' => self.parse_sequence(b'[', b']', depth),
            b'(' => self.parse_sequence(b'(', b')', depth),
            b'\'' | b'"' => self.parse_string(),
            b'-' | b'+' | b'.' | b'0'..=b'9' => self.parse_number(),
            byte if is_identifier_start(byte) => self.parse_identifier(),
            _ => None,
        }
    }

    fn parse_hex_units(&mut self, count: usize) -> Option<u16> {
        let digits = self
            .input
            .as_bytes()
            .get(self.position..self.position + count)?;
        let mut value: u16 = 0;
        for digit in digits {
            let nibble = char::from(*digit).to_digit(16)?;
            value = value * 16 + u16::try_from(nibble).ok()?;
        }
        self.position += count;
        Some(value)
    }

    fn parse_string(&mut self) -> Option<String> {
        let quote = self.peek()?;
        self.position += 1;

        let mut is_triple = false;
        if self.peek() == Some(quote) && self.peek_at(1) == Some(quote) {
            self.position += 2;
            is_triple = true;
        }

        // UTF-16 units, so `😀` escapes join into one character the
        // way the TypeScript string concatenation does.
        let mut units: Vec<u16> = Vec::new();

        while !self.at_end() {
            let byte = self.peek()?;

            if byte == quote {
                if is_triple {
                    if self.peek_at(1) == Some(quote) && self.peek_at(2) == Some(quote) {
                        self.position += 3;
                        return Some(quote_utf16(&units));
                    }
                    units.push(u16::from(byte));
                    self.position += 1;
                    continue;
                }
                self.position += 1;
                return Some(quote_utf16(&units));
            }

            if byte == b'\\' {
                self.position += 1;
                let escaped = self.input.get(self.position..)?.chars().next()?;
                self.position += escaped.len_utf8();

                match escaped {
                    'x' => units.push(self.parse_hex_units(2)?),
                    'u' => units.push(self.parse_hex_units(4)?),
                    '\n' => {}
                    '\\' => units.push(u16::from(b'\\')),
                    '\'' => units.push(u16::from(b'\'')),
                    '"' => units.push(u16::from(b'"')),
                    'n' => units.push(0x0A),
                    'r' => units.push(0x0D),
                    't' => units.push(0x09),
                    'b' => units.push(0x08),
                    'f' => units.push(0x0C),
                    '0' => units.push(0x00),
                    'a' => units.push(0x07),
                    'v' => units.push(0x0B),
                    _ => return None,
                }
                continue;
            }

            if !is_triple && (byte == b'\n' || byte == b'\r') {
                return None;
            }

            let character = self.input.get(self.position..)?.chars().next()?;
            self.position += character.len_utf8();
            let mut buffer = [0u16; 2];
            units.extend_from_slice(character.encode_utf16(&mut buffer));
        }

        None
    }

    fn consume_digits(&mut self) -> usize {
        let start = self.position;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.position += 1;
        }
        self.position - start
    }

    fn parse_number(&mut self) -> Option<String> {
        let start = self.position;
        if matches!(self.peek(), Some(b'-' | b'+')) {
            self.position += 1;
        }

        let digits_before_dot = self.consume_digits();

        let mut has_dot = false;
        let mut digits_after_dot = 0;
        if self.peek() == Some(b'.') {
            has_dot = true;
            self.position += 1;
            digits_after_dot = self.consume_digits();
        }

        if digits_before_dot == 0 && digits_after_dot == 0 {
            return None;
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

        let mut raw = self.slice(start, self.position).to_string();

        if let Some(stripped) = raw.strip_prefix('+') {
            raw = stripped.to_string();
        }
        if let Some(rest) = raw.strip_prefix("-.") {
            raw = format!("-0.{rest}");
        } else if raw.starts_with('.') {
            raw = format!("0{raw}");
        }
        if has_dot && digits_after_dot == 0 {
            match raw.find(['e', 'E']) {
                None => raw.push('0'),
                Some(exponent_position) => raw.insert(exponent_position, '0'),
            }
        }

        Some(raw)
    }

    fn parse_identifier(&mut self) -> Option<String> {
        let start = self.position;
        while matches!(self.peek(), Some(byte) if is_identifier_start(byte) || byte.is_ascii_digit())
        {
            self.position += 1;
        }
        match self.slice(start, self.position) {
            "True" => Some("true".to_string()),
            "False" => Some("false".to_string()),
            "None" => Some("null".to_string()),
            _ => None,
        }
    }

    fn parse_sequence(&mut self, open: u8, close: u8, depth: usize) -> Option<String> {
        if self.peek()? != open {
            return None;
        }
        self.position += 1;

        let mut items: Vec<String> = Vec::new();
        self.skip_whitespace();

        if self.peek() == Some(close) {
            self.position += 1;
            return Some("[]".to_string());
        }

        loop {
            items.push(self.parse_value(depth + 1)?);

            self.skip_whitespace();
            let next = self.peek()?;
            if next == b',' {
                self.position += 1;
                self.skip_whitespace();
                if self.peek() == Some(close) {
                    self.position += 1;
                    return Some(format!("[{}]", items.join(",")));
                }
                continue;
            }
            if next == close {
                self.position += 1;
                return Some(format!("[{}]", items.join(",")));
            }
            return None;
        }
    }

    fn parse_dict(&mut self, depth: usize) -> Option<String> {
        self.position += 1;
        self.skip_whitespace();

        if self.peek() == Some(b'}') {
            self.position += 1;
            return Some("{}".to_string());
        }

        let mut pairs: Vec<String> = Vec::new();

        loop {
            let key = self.parse_value(depth + 1)?;
            if !key.starts_with('"') {
                return None;
            }

            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return None;
            }
            self.position += 1;

            let value = self.parse_value(depth + 1)?;
            pairs.push(format!("{key}:{value}"));

            self.skip_whitespace();
            let next = self.peek()?;
            if next == b',' {
                self.position += 1;
                self.skip_whitespace();
                if self.peek() == Some(b'}') {
                    self.position += 1;
                    return Some(format!("{{{}}}", pairs.join(",")));
                }
                continue;
            }
            if next == b'}' {
                self.position += 1;
                return Some(format!("{{{}}}", pairs.join(",")));
            }
            return None;
        }
    }
}

/// `JSON.stringify` over UTF-16 units: unpaired surrogates become `\udXXX`
/// escapes, matching the well-formed JSON.stringify output.
fn quote_utf16(units: &[u16]) -> String {
    let mut output = String::with_capacity(units.len() + 2);
    output.push('"');
    for decoded in char::decode_utf16(units.iter().copied()) {
        match decoded {
            Ok('"') => output.push_str("\\\""),
            Ok('\\') => output.push_str("\\\\"),
            Ok('\u{8}') => output.push_str("\\b"),
            Ok('\u{c}') => output.push_str("\\f"),
            Ok('\n') => output.push_str("\\n"),
            Ok('\r') => output.push_str("\\r"),
            Ok('\t') => output.push_str("\\t"),
            Ok(control) if (control as u32) < 0x20 => {
                output.push_str(&format!("\\u{:04x}", control as u32));
            }
            Ok(character) => output.push(character),
            Err(error) => output.push_str(&format!("\\u{:04x}", error.unpaired_surrogate())),
        }
    }
    output.push('"');
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_json(input: &str, expected: &str) {
        let result = python_repr_to_json(input);
        assert_eq!(
            result.map(|result| result.json),
            Some(expected.to_string()),
            "input: {input:?}"
        );
    }

    fn expect_reject(input: &str) {
        assert_eq!(python_repr_to_json(input), None, "input: {input:?}");
    }

    #[test]
    fn input_shapes() {
        expect_json(r#"{"a": 1}"#, r#"{"a":1}"#);
        expect_json("[1, 2, 3]", "[1,2,3]");
        expect_reject(r#"{"a": null}"#);
        expect_reject(r#"{"a": true}"#);
        expect_reject("not python at all");
        expect_reject("");
        expect_reject("   ");
        expect_json("True", "true");
        expect_json("None", "null");
    }

    #[test]
    fn dicts() {
        expect_json(
            "{'name': 'alice', 'count': 3}",
            r#"{"name":"alice","count":3}"#,
        );
        expect_json(
            "{'a': True, 'b': False, 'c': None}",
            r#"{"a":true,"b":false,"c":null}"#,
        );
        expect_json("{}", "{}");
        expect_json("{'a': 1,}", r#"{"a":1}"#);
        expect_json(r#"{'a': "x", "b": 'y'}"#, r#"{"a":"x","b":"y"}"#);
        expect_reject("{1: 'a'}");
        expect_reject("{'a', 'b', 'c'}");
    }

    #[test]
    fn lists_and_tuples() {
        expect_json("[1, 'a', True, None]", r#"[1,"a",true,null]"#);
        expect_json("[]", "[]");
        expect_json("[1, 2, 3,]", "[1,2,3]");
        expect_json("(1, 2, 3)", "[1,2,3]");
        expect_json("(1,)", "[1]");
        expect_json("()", "[]");
    }

    #[test]
    fn nesting() {
        expect_json(
            "{'items': [1, 2], 'meta': {'ok': True}}",
            r#"{"items":[1,2],"meta":{"ok":true}}"#,
        );
        expect_json(
            "[{'a': 1}, {'a': 2}, {'a': 3}]",
            r#"[{"a":1},{"a":2},{"a":3}]"#,
        );
        expect_json("{'coords': (10, 20)}", r#"{"coords":[10,20]}"#);
        expect_json(
            "{'a': {'b': {'c': {'d': True}}}}",
            r#"{"a":{"b":{"c":{"d":true}}}}"#,
        );
    }

    #[test]
    fn strings() {
        expect_json(r"{'msg': 'It\'s fine'}", r#"{"msg":"It's fine"}"#);
        expect_json(r#"{'msg': 'He said "hi"'}"#, r#"{"msg":"He said \"hi\""}"#);
        expect_json(r"{'log': 'line1\nline2'}", r#"{"log":"line1\nline2"}"#);
        expect_json(r"{'a': 'x\ty\rz'}", r#"{"a":"x\ty\rz"}"#);
        expect_json(r"{'a': '\x41'}", r#"{"a":"A"}"#);
        expect_json(r"{'a': '\u00e9'}", r#"{"a":"é"}"#);
        expect_json("{'doc': '''line1\nline2'''}", r#"{"doc":"line1\nline2"}"#);
        expect_json("{'a': ''}", r#"{"a":""}"#);
        expect_reject("{'a': 'line1\nline2'}");
        expect_reject("{'a': 'unterminated");
        expect_reject(r"{'a': '\q'}");
    }

    #[test]
    fn unicode_strings() {
        expect_json("{'名字': '你好😀'}", r#"{"名字":"你好😀"}"#);
        expect_json(r"{'a': '\ud83d\ude00'}", r#"{"a":"😀"}"#);
        expect_json(r"{'a': '\ud83d'}", r#"{"a":"\ud83d"}"#);
    }

    #[test]
    fn numbers() {
        expect_json("{'a': 42}", r#"{"a":42}"#);
        expect_json("{'a': -7}", r#"{"a":-7}"#);
        expect_json("{'a': 3.14}", r#"{"a":3.14}"#);
        expect_json("{'a': .5}", r#"{"a":0.5}"#);
        expect_json("{'a': -.5}", r#"{"a":-0.5}"#);
        expect_json("{'a': 1.}", r#"{"a":1.0}"#);
        expect_json("{'a': 1.e5}", r#"{"a":1.0e5}"#);
        expect_json("{'a': 1.5e2, 'b': 2E-3}", r#"{"a":1.5e2,"b":2E-3}"#);
        expect_json("{'a': +5}", r#"{"a":5}"#);
        expect_json(
            "{'a': 12345678901234567890, 'b': 0.1000000000000000055}",
            r#"{"a":12345678901234567890,"b":0.1000000000000000055}"#,
        );
    }

    #[test]
    fn unsupported_forms_are_rejected() {
        expect_reject("{'a': b'raw'}");
        expect_reject("{'t': datetime(2024, 1, 1)}");
        expect_reject("{'obj': <Foo object at 0x100>}");
        expect_reject("{'id': UUID('abc')}");
        expect_reject("{'a': 1} junk");
        expect_reject("{'a': 1");
        expect_reject("[1, 2");
        expect_reject("{'a' 1}");
        expect_reject("{'a': {1, 2}}");
        expect_reject("{'a': 1e}");
    }

    #[test]
    fn whitespace() {
        expect_json("  {  'a'  :  1  ,  'b'  :  2  }  ", r#"{"a":1,"b":2}"#);
        expect_json("{\n  'a': 1,\n  'b': 2\n}", r#"{"a":1,"b":2}"#);
    }

    #[test]
    fn deep_nesting_does_not_overflow() {
        let input = "[".repeat(100_000);
        expect_reject(&input);
    }
}
