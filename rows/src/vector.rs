//! A vector as a row carries it, and as the wire carries it.
//!
//! Two spellings arrive, and both mean the same numbers. A JSON array
//! is what a module hands another module: rows already travel as JSON
//! text, so an array of numbers costs nothing to read and is what
//! ffrwd's own `rows_schema` entries declare. A base64 string of
//! little-endian binary32 is what ffrwd's vector tracks hold, one block
//! of a WebVTT document per row, so a writer copying rows straight out
//! of a file has them in that form already. Reading both here means
//! neither caller has to care which it was given.

use ffrwd_index_core::message::{Encoding, Space};
use ffrwd_index_core::quant::{f32_to_f16, Planes};

use crate::json::Json;

/// How many bytes one binary32 takes, which is what a base64 payload is
/// a whole number of.
const F32_BYTES: usize = 4;

/// One row's `vector` field, whichever of its two spellings it used,
/// checked against the space it belongs to.
pub fn read_values(value: Option<&Json>, dims: u32) -> Result<Vec<f32>, String> {
    let values = match value {
        Some(Json::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_f64()
                    .map(|number| number as f32)
                    .ok_or("a vector component that is not a number")
            })
            .collect::<Result<Vec<f32>, &str>>()?,
        Some(Json::String(text)) => from_base64(text)?,
        _ => return Err("a row with no vector".into()),
    };
    if values.len() as u32 != dims {
        return Err(format!(
            "a vector of {} components in a space of {dims}",
            values.len()
        ));
    }
    if values.iter().any(|value| !value.is_finite()) {
        return Err("a vector component that is not a finite number".into());
    }
    Ok(values)
}

/// Base64 of little-endian binary32, the form a vector track's blocks
/// carry, as the numbers it stands for.
pub fn from_base64(text: &str) -> Result<Vec<f32>, String> {
    let bytes = decode_base64(text)?;
    if bytes.len() % F32_BYTES != 0 {
        return Err("a base64 vector whose bytes are not whole binary32s".into());
    }
    let (whole, _) = bytes.as_chunks::<F32_BYTES>();
    Ok(whole.iter().copied().map(f32::from_le_bytes).collect())
}

/// The bodies one record goes out as, most significant first.
///
/// `split` is the difference between the two ways a writer sends the
/// layered encoding, and section 5 says when each is right: one message
/// with every plane in it for a writer with room, one message per plane
/// for a writer doling out a byte budget per carrier. A space that is
/// not `i8` has one body either way.
///
/// `plane_cap`, where it is given, is how many of the eight planes are
/// sent at all: four of them reconstruct a MiniLM vector to cosine
/// 0.991 for half the bytes, which is a trade a writer may want and the
/// format already lets a reader make sense of, since it reads whatever
/// run of planes it has starting at plane 0.
pub fn bodies(
    space: &Space,
    values: &[f32],
    escapes: usize,
    plane_cap: Option<u8>,
    split: bool,
) -> Result<Vec<Vec<u8>>, String> {
    if values.len() as u32 != space.dims {
        return Err(format!(
            "a vector of {} components in a space of {}",
            values.len(),
            space.dims
        ));
    }
    match space.encoding {
        Encoding::I8 => {
            let mut planes = Planes::quantize(values, escapes).map_err(|err| err.to_string())?;
            if let Some(cap) = plane_cap {
                planes = planes.subset(mask_for(cap));
            }
            Ok(if split {
                planes.split().iter().map(Planes::encode).collect()
            } else {
                vec![planes.encode()]
            })
        }
        Encoding::F32 => Ok(vec![values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()]),
        Encoding::F16 => Ok(vec![values
            .iter()
            .flat_map(|value| f32_to_f16(*value).to_le_bytes())
            .collect()]),
        Encoding::Other(other) => Err(format!("encoding {other} cannot be written")),
    }
}

/// The lowest `count` planes, which is the run a reader can use: the
/// spec has it ignore every plane above the first one missing, so a cap
/// that left a hole would throw away the planes past it.
fn mask_for(count: u8) -> u8 {
    match count {
        0 => 1,
        n if n >= 8 => 0xff,
        n => (1u16 << n) as u8 - 1,
    }
}

/// Which planes a body holds, as their numbers, for a row saying what
/// went out.
pub fn plane_numbers(present: u8) -> Vec<u8> {
    (0..8).filter(|plane| present >> plane & 1 == 1).collect()
}

fn decode_base64(text: &str) -> Result<Vec<u8>, String> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    let body = match digits.iter().position(|byte| *byte == b'=') {
        Some(at) => {
            if digits[at..].iter().any(|byte| *byte != b'=') || digits.len() - at > 2 {
                return Err("a base64 vector with padding inside it".into());
            }
            &digits[..at]
        }
        None => &digits[..],
    };
    if body.len() % 4 == 1 {
        return Err("a base64 vector with a stray digit".into());
    }
    let mut out = Vec::with_capacity(body.len() / 4 * 3);
    let mut held = 0u32;
    let mut bits = 0u32;
    for digit in body {
        held = (held << 6) | u32::from(sextet(*digit)?);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((held >> bits) as u8);
        }
    }
    Ok(out)
}

fn sextet(digit: u8) -> Result<u8, String> {
    match digit {
        b'A'..=b'Z' => Ok(digit - b'A'),
        b'a'..=b'z' => Ok(digit - b'a' + 26),
        b'0'..=b'9' => Ok(digit - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        other => Err(format!(
            "{:?} is not a base64 digit",
            char::from_u32(u32::from(other)).unwrap_or('?')
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::float;

    /// What `_vector_payload` in ffrwd's own compiler writes: little
    /// endian binary32, base64, no line breaks.
    fn base64(values: &[f32]) -> String {
        const DIGITS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let mut held = 0u32;
            for (index, byte) in chunk.iter().enumerate() {
                held |= u32::from(*byte) << (16 - index * 8);
            }
            for index in 0..chunk.len() + 1 {
                out.push(DIGITS[(held >> (18 - index * 6) & 0x3f) as usize] as char);
            }
            for _ in chunk.len() + 1..4 {
                out.push('=');
            }
        }
        out
    }

    #[test]
    fn both_spellings_of_a_vector_are_the_same_numbers() {
        let values = [0.5f32, -1.25, 0.0, 3.5, -0.125, 1e-8, 65504.0];
        let array = Json::Array(values.iter().copied().map(float).collect());
        let text = Json::String(base64(&values));
        let dims = values.len() as u32;
        assert_eq!(
            read_values(Some(&array), dims).expect("an array"),
            values.to_vec()
        );
        assert_eq!(
            read_values(Some(&text), dims).expect("base64"),
            values.to_vec()
        );
    }

    #[test]
    fn base64_reads_at_every_padding() {
        for count in 1..7usize {
            let values: Vec<f32> = (0..count).map(|i| i as f32 * -0.75).collect();
            assert_eq!(
                from_base64(&base64(&values)).expect("a vector"),
                values,
                "{count} components"
            );
        }
        // Unpadded is read too: a writer that trims is not a writer that
        // meant something else.
        let trimmed = base64(&[1.0f32, 2.0]).trim_end_matches('=').to_string();
        assert_eq!(from_base64(&trimmed).expect("a vector"), vec![1.0, 2.0]);
    }

    #[test]
    fn a_broken_vector_is_refused_and_says_why() {
        assert!(read_values(None, 4).unwrap_err().contains("no vector"));
        assert!(read_values(Some(&Json::Null), 4)
            .unwrap_err()
            .contains("no vector"));
        let short = Json::Array(vec![float(1.0)]);
        assert!(read_values(Some(&short), 4)
            .unwrap_err()
            .contains("components"));
        let text = Json::String("not base64!!".into());
        assert!(read_values(Some(&text), 4).is_err());
        // Five digits: four bytes and a stray sextet that says nothing.
        let stray = format!("{}A", &base64(&[1.0f32])[..4]);
        assert!(from_base64(&stray).unwrap_err().contains("stray"));
        let not_a_number = Json::Array(vec![Json::String("0.5".into())]);
        assert!(read_values(Some(&not_a_number), 1)
            .unwrap_err()
            .contains("not a number"));
    }

    #[test]
    fn a_non_finite_component_never_reaches_the_wire() {
        let infinite = Json::String(base64(&[f32::INFINITY, 0.0]));
        assert!(read_values(Some(&infinite), 2)
            .unwrap_err()
            .contains("finite"));
        let nan = Json::String(base64(&[f32::NAN, 0.0]));
        assert!(read_values(Some(&nan), 2).unwrap_err().contains("finite"));
    }

    #[test]
    fn a_plane_cap_keeps_the_run_a_reader_can_use() {
        let space = Space::new(1, 16, Encoding::I8);
        let values: Vec<f32> = (0..16).map(|i| (i as f32 * 0.4).sin()).collect();
        let whole = bodies(&space, &values, 2, None, false).expect("a body");
        assert_eq!(whole.len(), 1, "a writer with room sends one message");
        let capped = bodies(&space, &values, 2, Some(4), false).expect("a body");
        assert!(capped[0].len() < whole[0].len(), "the cap saved nothing");
        let planes = Planes::decode(16, &capped[0]).expect("the capped planes");
        assert_eq!(plane_numbers(planes.present()), vec![0, 1, 2, 3]);
        // A cap of zero still leaves plane 0: a record without it cannot
        // be read at all.
        let signs = bodies(&space, &values, 0, Some(0), false).expect("a body");
        let planes = Planes::decode(16, &signs[0]).expect("the sign plane");
        assert_eq!(plane_numbers(planes.present()), vec![0]);
    }

    #[test]
    fn a_budget_gets_one_body_per_plane() {
        let space = Space::new(1, 16, Encoding::I8);
        let values: Vec<f32> = (0..16).map(|i| (i as f32 * 0.4).cos()).collect();
        assert_eq!(
            bodies(&space, &values, 2, None, true)
                .expect("bodies")
                .len(),
            8
        );
        assert_eq!(
            bodies(&space, &values, 2, Some(3), true)
                .expect("bodies")
                .len(),
            3
        );
        // Splitting is the layered encoding's alone.
        let f32_space = Space::new(2, 4, Encoding::F32);
        let four = [1.0f32, 2.0, 3.0, 4.0];
        assert_eq!(
            bodies(&f32_space, &four, 0, None, true)
                .expect("bodies")
                .len(),
            1
        );
    }
}
