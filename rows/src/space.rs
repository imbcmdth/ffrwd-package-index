//! A SPACE message as a JSON object.
//!
//! The fields are section 3's, one for one, and they read and write the
//! same way wherever they turn up: in a `{"space": {...}}` row the tool
//! takes, in the `spaces` list the weave module's params carry, and in
//! what `read` prints back. The one thing that differs between callers
//! is the name in front of them, which is why [`read_fields`] takes the
//! id rather than looking for one.

use ffrwd_index_core::message::{Encoding, Modality, Space, FLAG_UNIT_LENGTH};
use ffrwd_index_core::MAX_DIMS;

use crate::json::{number, object, string, Json};

/// Every SPACE field but the id, which the caller has already decided.
pub fn read_fields(row: &Json, space_id: u8) -> Result<Space, String> {
    let dims = row
        .get("dims")
        .and_then(Json::as_i64)
        .ok_or("a space with no dims")?;
    let dims = u32::try_from(dims).map_err(|_| "dims that are not a count")?;
    let encoding = match row.get("encoding").and_then(Json::as_str).unwrap_or("i8") {
        "f32" => Encoding::F32,
        "f16" => Encoding::F16,
        "i8" => Encoding::I8,
        other => return Err(format!("{other} is not f32, f16 or i8")),
    };
    let mut space = Space::new(space_id, dims, encoding);
    if row.get("unit_length").and_then(Json::as_bool) == Some(true) {
        space.flags |= FLAG_UNIT_LENGTH;
    }
    space.modality = match row.get("modality") {
        None | Some(Json::Null) => Modality::Unspecified,
        Some(Json::Number(_)) => Modality::from_u8(
            u8::try_from(row.get("modality").and_then(Json::as_i64).unwrap_or(0))
                .map_err(|_| "a modality outside 0 to 255")?,
        ),
        Some(value) => modality_of(value.as_str().ok_or("a modality that is not a name")?)?,
    };
    if let Some(source) = row.get("source").and_then(Json::as_i64) {
        space.source = u8::try_from(source).map_err(|_| "a source outside 0 to 255")?;
    }
    space.model = row
        .get("model")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    space.query = row
        .get("query")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    space.producer = row
        .get("producer")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_string();
    space.model_hash = hash_of(row.get("model_hash"))?;
    space.query_hash = hash_of(row.get("query_hash"))?;
    if space.dims == 0 || space.dims > MAX_DIMS {
        return Err(format!("{} dimensions is outside the format", space.dims));
    }
    Ok(space)
}

/// A space descriptor that names its own id, which is the tool's row.
pub fn read_space(row: &Json) -> Result<Space, String> {
    let id = row
        .get("id")
        .or_else(|| row.get("space_id"))
        .and_then(Json::as_i64)
        .ok_or("a space with no id")?;
    let space_id = u8::try_from(id).map_err(|_| "a space id outside 0 to 255")?;
    read_fields(row, space_id)
}

/// A space as the row that would read back as it, with its wire id.
pub fn space_row(space: &Space) -> Json {
    object(fields(space, ("id", number(space.space_id))))
}

/// The same, named the way the weave module's params name a space.
pub fn named_space_row(name: &str, space: &Space) -> Json {
    object(fields(space, ("name", string(name))))
}

fn fields(space: &Space, first: (&'static str, Json)) -> Vec<(&'static str, Json)> {
    vec![
        first,
        ("dims", number(space.dims)),
        ("encoding", string(space.encoding.name())),
        ("unit_length", Json::Bool(space.unit_length())),
        ("modality", string(space.modality.name())),
        ("source", number(space.source)),
        ("model", string(space.model.clone())),
        ("model_hash", string(hex(&space.model_hash))),
        ("query", string(space.query.clone())),
        ("query_hash", string(hex(&space.query_hash))),
        ("producer", string(space.producer.clone())),
    ]
}

/// The first sixteen bytes of a SHA-256, as hex, or all zero.
pub fn hash_of(value: Option<&Json>) -> Result<[u8; 16], String> {
    let Some(text) = value.and_then(Json::as_str) else {
        return Ok([0; 16]);
    };
    if text.is_empty() {
        return Ok([0; 16]);
    }
    let digits: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if digits.len() < 32 {
        return Err("a hash shorter than sixteen bytes of hex".into());
    }
    let mut out = [0u8; 16];
    for (index, byte) in out.iter_mut().enumerate() {
        let pair = std::str::from_utf8(&digits[index * 2..index * 2 + 2])
            .map_err(|_| "a hash that is not hex")?;
        *byte = u8::from_str_radix(pair, 16).map_err(|_| "a hash that is not hex")?;
    }
    Ok(out)
}

/// Sixteen bytes as hex, or the empty string for a hash nobody gave.
pub fn hex(bytes: &[u8; 16]) -> String {
    if bytes.iter().all(|byte| *byte == 0) {
        return String::new();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A modality by the name section 3's table gives it.
pub fn modality_of(name: &str) -> Result<Modality, String> {
    match name {
        "unspecified" => Ok(Modality::Unspecified),
        "picture" => Ok(Modality::Picture),
        "sound" => Ok(Modality::Sound),
        "speech" => Ok(Modality::Speech),
        "sound-text" => Ok(Modality::SoundText),
        "scene-text" => Ok(Modality::SceneText),
        "description" => Ok(Modality::Description),
        other => Err(format!("{other} is not a modality this format names")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_space_row_reads_every_field() {
        let row = Json::parse(
            r#"{"id":3,"dims":4,"encoding":"f32","unit_length":true,
                "modality":"speech","source":2,"model":"hf:a/b@c/d.safetensors",
                "model_hash":"000102030405060708090a0b0c0d0e0f","query":"hf:q",
                "query_hash":"","producer":"a test"}"#,
        )
        .expect("a row");
        let space = read_space(&row).expect("a descriptor");
        assert_eq!(space.space_id, 3);
        assert_eq!(space.dims, 4);
        assert_eq!(space.encoding, Encoding::F32);
        assert!(space.unit_length());
        assert_eq!(space.modality, Modality::Speech);
        assert_eq!(space.source, 2);
        assert_eq!(space.model, "hf:a/b@c/d.safetensors");
        assert_eq!(space.model_hash[..4], [0, 1, 2, 3]);
        assert_eq!(space.query_model(), "hf:q");
        assert_eq!(space.query_hash, [0; 16]);
        assert_eq!(space.producer, "a test");
        // And back out again as the same row.
        let written = space_row(&space);
        assert_eq!(
            read_space(&written).expect("a descriptor again"),
            space,
            "{}",
            written.write()
        );
    }

    #[test]
    fn a_named_row_reads_back_the_same_fields() {
        let space = read_fields(
            &Json::parse(r#"{"dims":8,"encoding":"i8","modality":"picture"}"#).expect("a row"),
            7,
        )
        .expect("a descriptor");
        let written = named_space_row("clip", &space);
        assert_eq!(written.get("name").and_then(Json::as_str), Some("clip"));
        assert!(
            written.get("id").is_none(),
            "a named row carries no wire id"
        );
        assert_eq!(read_fields(&written, 7).expect("again"), space);
    }

    #[test]
    fn a_modality_may_be_a_name_or_a_number() {
        let by_name = Json::parse(r#"{"dims":2,"modality":"scene-text"}"#).expect("a row");
        assert_eq!(
            read_fields(&by_name, 1).expect("a space").modality,
            Modality::SceneText
        );
        let by_number = Json::parse(r#"{"dims":2,"modality":9}"#).expect("a row");
        assert_eq!(
            read_fields(&by_number, 1).expect("a space").modality,
            Modality::Other(9)
        );
    }

    #[test]
    fn a_hash_reads_from_hex_and_writes_back() {
        assert_eq!(hash_of(None).expect("zeroes"), [0; 16]);
        assert_eq!(hash_of(Some(&string(""))).expect("zeroes"), [0; 16]);
        let text = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
        let bytes = hash_of(Some(&string(text))).expect("a hash");
        assert_eq!(bytes[0], 0x0f);
        assert_eq!(hex(&bytes), text);
        assert_eq!(hex(&[0; 16]), "");
        assert!(hash_of(Some(&string("00"))).is_err());
        assert!(hash_of(Some(&string("zz1e2d3c4b5a69788796a5b4c3d2e1f0"))).is_err());
    }
}
