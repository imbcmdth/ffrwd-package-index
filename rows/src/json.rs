//! Just enough JSON for one object per line.
//!
//! Written out rather than pulled in: the rows this tool reads are flat
//! objects of numbers, strings and one array, and a parser for that is
//! shorter than the argument about which crate to depend on. It is
//! strict about what it accepts and says where it stopped.
//!
//! The reader recurses into objects and arrays, so it is bounded: a row
//! is somebody else's text and nesting it a few thousand deep would
//! otherwise walk the stack off the end of a wasm module. [`MAX_DEPTH`]
//! is where it stops, and it is serde_json's own default, so a row this
//! refuses is a row every other reader of it refuses too.

use std::fmt::Write as _;

/// How deep a value may nest before a read gives up.
///
/// 128, which is serde_json's `Deserializer` default. A row of this
/// format is a flat object with one array of numbers in it, two deep,
/// so nothing a writer here produces is anywhere near it.
pub const MAX_DEPTH: usize = 128;

/// A JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    /// Members in the order they were written, which is the order they
    /// are written back out in.
    Object(Vec<(String, Json)>),
    /// A number already written as text, and written back out as it
    /// stands. A binary32 widened to a binary64 prints its own rounding
    /// error, and a row of vector components is unreadable that way, so
    /// components are formatted as the binary32 they are.
    Written(String),
}

impl Json {
    /// A member of an object.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(members) => members
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Number(value) => Some(*value),
            // A number this crate already formatted is still a number:
            // a row built in memory and read back without going through
            // text reads the same as one that did.
            Json::Written(text) => text.parse().ok(),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        let value = self.as_f64()?;
        if value.is_finite() && value.fract() == 0.0 {
            Some(value as i64)
        } else {
            None
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(values) => Some(values),
            _ => None,
        }
    }

    /// The value as one line of JSON.
    pub fn write(&self) -> String {
        let mut out = String::new();
        self.write_into(&mut out);
        out
    }

    fn write_into(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Number(value) => {
                if value.is_finite() {
                    let _ = write!(out, "{value}");
                } else {
                    // JSON has no infinities and no NaN, and a row that
                    // holds one is a row somebody has to look at.
                    out.push_str("null");
                }
            }
            Json::Written(text) => out.push_str(text),
            Json::String(value) => write_string(value, out),
            Json::Array(values) => {
                out.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    value.write_into(out);
                }
                out.push(']');
            }
            Json::Object(members) => {
                out.push('{');
                for (index, (name, value)) in members.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write_string(name, out);
                    out.push(':');
                    value.write_into(out);
                }
                out.push('}');
            }
        }
    }

    /// One JSON value, which must be the whole of `text`.
    pub fn parse(text: &str) -> Result<Json, String> {
        let bytes = text.as_bytes();
        let mut at = 0usize;
        let value = parse_value(bytes, &mut at, MAX_DEPTH)?;
        skip_space(bytes, &mut at);
        if at != bytes.len() {
            return Err(format!("trailing bytes at {at}"));
        }
        Ok(value)
    }
}

/// An object, for building a row.
pub fn object(members: Vec<(&str, Json)>) -> Json {
    Json::Object(
        members
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect(),
    )
}

/// A string value.
pub fn string(value: impl Into<String>) -> Json {
    Json::String(value.into())
}

/// A number value.
pub fn number(value: impl Into<f64>) -> Json {
    Json::Number(value.into())
}

/// A binary32, written as a binary32 rather than as the binary64 it
/// widens to.
pub fn float(value: f32) -> Json {
    if value.is_finite() {
        Json::Written(format!("{value}"))
    } else {
        Json::Null
    }
}

fn write_string(value: &str, out: &mut String) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

fn skip_space(bytes: &[u8], at: &mut usize) {
    while matches!(bytes.get(*at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        *at += 1;
    }
}

/// One value, with `left` levels of nesting still allowed.
fn parse_value(bytes: &[u8], at: &mut usize, left: usize) -> Result<Json, String> {
    skip_space(bytes, at);
    match bytes.get(*at) {
        None => Err("the line ends where a value was expected".into()),
        Some(b'{' | b'[') if left == 0 => {
            Err(format!("more than {MAX_DEPTH} levels of nesting at {at}"))
        }
        Some(b'{') => parse_object(bytes, at, left - 1),
        Some(b'[') => parse_array(bytes, at, left - 1),
        Some(b'"') => Ok(Json::String(parse_string(bytes, at)?)),
        Some(b't') => parse_word(bytes, at, "true", Json::Bool(true)),
        Some(b'f') => parse_word(bytes, at, "false", Json::Bool(false)),
        Some(b'n') => parse_word(bytes, at, "null", Json::Null),
        Some(_) => parse_number(bytes, at),
    }
}

fn parse_word(bytes: &[u8], at: &mut usize, word: &str, value: Json) -> Result<Json, String> {
    if bytes[*at..].starts_with(word.as_bytes()) {
        *at += word.len();
        Ok(value)
    } else {
        Err(format!("not a value at {at}"))
    }
}

fn parse_object(bytes: &[u8], at: &mut usize, left: usize) -> Result<Json, String> {
    *at += 1;
    let mut members = Vec::new();
    skip_space(bytes, at);
    if bytes.get(*at) == Some(&b'}') {
        *at += 1;
        return Ok(Json::Object(members));
    }
    loop {
        skip_space(bytes, at);
        let name = parse_string(bytes, at)?;
        skip_space(bytes, at);
        if bytes.get(*at) != Some(&b':') {
            return Err(format!("no colon after a name at {at}"));
        }
        *at += 1;
        members.push((name, parse_value(bytes, at, left)?));
        skip_space(bytes, at);
        match bytes.get(*at) {
            Some(b',') => *at += 1,
            Some(b'}') => {
                *at += 1;
                return Ok(Json::Object(members));
            }
            _ => return Err(format!("an object does not end at {at}")),
        }
    }
}

fn parse_array(bytes: &[u8], at: &mut usize, left: usize) -> Result<Json, String> {
    *at += 1;
    let mut values = Vec::new();
    skip_space(bytes, at);
    if bytes.get(*at) == Some(&b']') {
        *at += 1;
        return Ok(Json::Array(values));
    }
    loop {
        values.push(parse_value(bytes, at, left)?);
        skip_space(bytes, at);
        match bytes.get(*at) {
            Some(b',') => *at += 1,
            Some(b']') => {
                *at += 1;
                return Ok(Json::Array(values));
            }
            _ => return Err(format!("an array does not end at {at}")),
        }
    }
}

fn parse_string(bytes: &[u8], at: &mut usize) -> Result<String, String> {
    if bytes.get(*at) != Some(&b'"') {
        return Err(format!("no string at {at}"));
    }
    *at += 1;
    let mut out = String::new();
    loop {
        let byte = *bytes.get(*at).ok_or("a string does not end")?;
        *at += 1;
        match byte {
            b'"' => return Ok(out),
            b'\\' => {
                let escape = *bytes.get(*at).ok_or("an escape does not end")?;
                *at += 1;
                match escape {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => out.push(parse_escape(bytes, at)?),
                    other => return Err(format!("an escape of {other:#04x}")),
                }
            }
            byte => {
                // The bytes came from a &str, so a run of non-ASCII
                // bytes is valid UTF-8 as it stands.
                let start = *at - 1;
                let mut end = *at;
                if byte >= 0x80 {
                    while bytes.get(end).is_some_and(|next| *next & 0xc0 == 0x80) {
                        end += 1;
                    }
                    *at = end;
                }
                out.push_str(
                    std::str::from_utf8(&bytes[start..end]).map_err(|_| "a broken character")?,
                );
            }
        }
    }
}

/// One `\uXXXX` escape, with its surrogate pair if it has one.
fn parse_escape(bytes: &[u8], at: &mut usize) -> Result<char, String> {
    let first = hex4(bytes, at)?;
    if (0xd800..0xdc00).contains(&first) {
        if bytes.get(*at) != Some(&b'\\') || bytes.get(*at + 1) != Some(&b'u') {
            return Err("a lone surrogate".into());
        }
        *at += 2;
        let second = hex4(bytes, at)?;
        if !(0xdc00..0xe000).contains(&second) {
            return Err("a broken surrogate pair".into());
        }
        let value = 0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00);
        return char::from_u32(value).ok_or_else(|| "not a character".into());
    }
    char::from_u32(first).ok_or_else(|| "not a character".into())
}

fn hex4(bytes: &[u8], at: &mut usize) -> Result<u32, String> {
    let digits = bytes.get(*at..*at + 4).ok_or("a short escape")?;
    *at += 4;
    let text = std::str::from_utf8(digits).map_err(|_| "a broken escape")?;
    u32::from_str_radix(text, 16).map_err(|_| "a broken escape".into())
}

fn parse_number(bytes: &[u8], at: &mut usize) -> Result<Json, String> {
    let start = *at;
    if bytes.get(*at) == Some(&b'-') {
        *at += 1;
    }
    while matches!(bytes.get(*at), Some(byte) if byte.is_ascii_digit()) {
        *at += 1;
    }
    if bytes.get(*at) == Some(&b'.') {
        *at += 1;
        while matches!(bytes.get(*at), Some(byte) if byte.is_ascii_digit()) {
            *at += 1;
        }
    }
    if matches!(bytes.get(*at), Some(b'e' | b'E')) {
        *at += 1;
        if matches!(bytes.get(*at), Some(b'+' | b'-')) {
            *at += 1;
        }
        while matches!(bytes.get(*at), Some(byte) if byte.is_ascii_digit()) {
            *at += 1;
        }
    }
    let text = std::str::from_utf8(&bytes[start..*at]).map_err(|_| "a broken number")?;
    text.parse::<f64>()
        .map(Json::Number)
        .map_err(|_| format!("not a number at {start}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_round_trips() {
        let text = r#"{"space_id":1,"start_ms":0,"end_ms":2000,"vector":[0.5,-1.25,0]}"#;
        let value = Json::parse(text).expect("a row");
        assert_eq!(value.write(), text);
        assert_eq!(value.get("space_id").and_then(Json::as_i64), Some(1));
        assert_eq!(
            value
                .get("vector")
                .and_then(Json::as_array)
                .map(<[Json]>::len),
            Some(3)
        );
    }

    #[test]
    fn strings_keep_what_was_in_them() {
        for text in [
            r#""plain""#,
            r#""with \"quotes\" and \\ and \n""#,
            r#""é中""#,
            r#""😀""#,
            r#""already é as bytes""#,
        ] {
            let value = Json::parse(text).expect("a string");
            let back = Json::parse(&value.write()).expect("a string again");
            assert_eq!(back, value, "{text}");
        }
        assert_eq!(
            Json::parse(r#""😀""#).expect("a pair"),
            Json::String("\u{1f600}".into())
        );
        assert_eq!(
            Json::parse("\"\u{e9} direct\"").expect("utf-8"),
            Json::String("\u{e9} direct".into())
        );
    }

    #[test]
    fn a_binary32_prints_as_itself() {
        assert_eq!(float(0.1).write(), "0.1");
        assert_eq!(float(-1.0 / 3.0).write(), "-0.33333334");
        assert_eq!(float(f32::NAN).write(), "null");
        let back = Json::parse(&float(0.1).write()).expect("a number");
        assert_eq!(back.as_f64().expect("a number") as f32, 0.1f32);
        // And without going through text at all.
        assert_eq!(float(0.1).as_f64().expect("a number") as f32, 0.1f32);
    }

    #[test]
    fn numbers_read_the_shapes_a_writer_uses() {
        for (text, want) in [
            ("0", 0.0),
            ("-1", -1.0),
            ("1.5", 1.5),
            ("-0.25", -0.25),
            ("1e3", 1000.0),
            ("1.5E-2", 0.015),
        ] {
            assert_eq!(Json::parse(text).expect("a number"), Json::Number(want));
        }
    }

    #[test]
    fn broken_rows_are_refused_and_say_where() {
        for text in [
            "{",
            "{\"a\"}",
            "{\"a\":}",
            "[1,2",
            "\"unterminated",
            "{} trailing",
            "nul",
            "{\"a\":1,}",
            "",
        ] {
            assert!(Json::parse(text).is_err(), "{text} was accepted");
        }
    }

    #[test]
    fn empty_containers_read() {
        assert_eq!(Json::parse("{}").expect("an object"), Json::Object(vec![]));
        assert_eq!(Json::parse("[]").expect("an array"), Json::Array(vec![]));
        assert_eq!(Json::parse(" null ").expect("null"), Json::Null);
    }

    #[test]
    fn nesting_is_bounded_where_serde_json_bounds_it() {
        // A row is somebody else's text, and a reader that recurses on
        // it must stop somewhere before the stack does. Both shapes
        // nest, so both are bounded.
        for (open, close) in [("[", "]"), ("{\"a\":", "}")] {
            let at_limit = format!("{}null{}", open.repeat(MAX_DEPTH), close.repeat(MAX_DEPTH));
            assert!(
                Json::parse(&at_limit).is_ok(),
                "{MAX_DEPTH} levels was refused"
            );

            let one_past = format!(
                "{}null{}",
                open.repeat(MAX_DEPTH + 1),
                close.repeat(MAX_DEPTH + 1)
            );
            let err = Json::parse(&one_past).expect_err("a refusal");
            assert!(err.contains("nesting"), "{err}");

            // And the shape that would really have cost a stack: deep
            // enough to overflow one, with no closing brackets at all,
            // which is what an attacker writes.
            let flood = open.repeat(200_000);
            let err = Json::parse(&flood).expect_err("a refusal");
            assert!(err.contains("nesting"), "{err}");
        }
    }

    #[test]
    fn random_text_never_panics() {
        let mut seed = 0x5bf0_3635_9a1b_c2d3u64;
        let alphabet = b"{}[]\",:0123456789.eE-+ \\untrue";
        for _ in 0..4000 {
            let mut text = String::new();
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            for _ in 0..(seed >> 40) % 24 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                text.push(alphabet[(seed >> 33) as usize % alphabet.len()] as char);
            }
            let _ = Json::parse(&text);
        }
    }
}
