//! The module's `params`, which is one JSON object.
//!
//! The SPACE fields are the ones `tool/`'s rows already spell, read by
//! the same code, so a space declared to the tool and a space declared
//! to this module mean the same thing field for field. What differs is
//! the front of it: a space here has a `name`, because the rows a query
//! writes name spaces and have no reason to know what a `space_id` is.
//! The wire ids are handed out in declaration order.

use ffrwd_index_core::placement::Placement;
use ffrwd_index_core::{MAX_ESCAPES, UNIT_SOFT_LIMIT};
use ffrwd_index_rows::json::Json;
use ffrwd_index_rows::space::read_fields;
use ffrwd_index_rows::weave::Config;

/// The JSON Schema `describe` publishes. It is what a caller is
/// checked against before this module ever runs, so it says the same
/// things the reader below does.
pub const PARAMS_SCHEMA: &str = r#"{
  "type": "object",
  "required": ["spaces"],
  "additionalProperties": false,
  "properties": {
    "spaces": {
      "type": "array",
      "minItems": 1,
      "maxItems": 256,
      "description": "The embedding spaces this run carries. A row names one by its name; the wire ids are handed out in this order.",
      "items": {
        "type": "object",
        "required": ["name", "dims"],
        "additionalProperties": false,
        "properties": {
          "name": {"type": "string", "minLength": 1},
          "dims": {"type": "integer", "minimum": 1, "maximum": 65536},
          "encoding": {"enum": ["i8", "f16", "f32"], "default": "i8"},
          "unit_length": {"type": "boolean", "default": false},
          "modality": {"oneOf": [{"enum": ["unspecified", "picture", "sound", "speech", "sound-text", "scene-text", "description"]}, {"type": "integer", "minimum": 0, "maximum": 255}]},
          "source": {"type": "integer", "minimum": 0, "maximum": 255},
          "model": {"type": "string"},
          "model_hash": {"type": "string"},
          "query": {"type": "string"},
          "query_hash": {"type": "string"},
          "producer": {"type": "string"}
        }
      }
    },
    "placement": {"enum": ["keyframe", "next", "spread"], "default": "keyframe"},
    "budget": {"type": "integer", "minimum": 1, "description": "Bytes of messages per access unit, for placement 'spread' and nowhere else."},
    "escapes": {"type": "integer", "minimum": 0, "maximum": 16, "default": 2},
    "planes": {"type": "integer", "minimum": 1, "maximum": 8, "description": "How many of an i8 record's eight bit-planes are sent at all. All eight by default."}
  }
}"#;

/// Every key this module knows at the top level. A key it does not is
/// refused rather than ignored: a caller that misspelled `placement`
/// should hear about it, not get the default.
const KNOWN: &[&str] = &["spaces", "placement", "budget", "escapes", "planes"];

/// Every key one space may carry.
const KNOWN_SPACE: &[&str] = &[
    "name",
    "dims",
    "encoding",
    "unit_length",
    "modality",
    "source",
    "model",
    "model_hash",
    "query",
    "query_hash",
    "producer",
];

/// One params string as what the weaver needs.
pub fn read(params: &str) -> Result<Config, String> {
    let text = params.trim();
    if text.is_empty() {
        return Err("weave needs at least one space; its params are empty".into());
    }
    let row = Json::parse(text).map_err(|err| format!("params are not one JSON object: {err}"))?;
    let Json::Object(members) = &row else {
        return Err("params are not one JSON object".into());
    };
    for (name, _) in members {
        if !KNOWN.contains(&name.as_str()) {
            return Err(format!("'{name}' is not a param of weave"));
        }
    }

    let declared = row
        .get("spaces")
        .and_then(Json::as_array)
        .ok_or("params with no spaces")?;
    if declared.is_empty() {
        return Err("weave needs at least one space".into());
    }
    if declared.len() > 256 {
        return Err(format!(
            "{} spaces is more than the 256 a space id can name",
            declared.len()
        ));
    }
    let mut spaces = Vec::with_capacity(declared.len());
    for (index, entry) in declared.iter().enumerate() {
        let Json::Object(fields) = entry else {
            return Err(format!("space {index} is not an object"));
        };
        for (name, _) in fields {
            if !KNOWN_SPACE.contains(&name.as_str()) {
                return Err(format!("'{name}' is not a field of a space"));
            }
        }
        let name = entry
            .get("name")
            .and_then(Json::as_str)
            .ok_or_else(|| format!("space {index} has no name"))?;
        if name.is_empty() {
            return Err(format!("space {index} has an empty name"));
        }
        if spaces.iter().any(|(declared, _)| declared == name) {
            return Err(format!("two spaces are named '{name}'"));
        }
        let space =
            read_fields(entry, index as u8).map_err(|err| format!("space '{name}': {err}"))?;
        spaces.push((name.to_string(), space));
    }

    let budget = match row.get("budget") {
        None => None,
        Some(value) => {
            let bytes = value.as_i64().ok_or("a budget that is not a number")?;
            if bytes < 1 {
                return Err("a budget of no bytes".into());
            }
            Some(usize::try_from(bytes).map_err(|_| "a budget larger than this machine")?)
        }
    };
    let placement = match row
        .get("placement")
        .and_then(Json::as_str)
        .unwrap_or("keyframe")
    {
        "keyframe" => Placement::Keyframe,
        "next" => Placement::Next,
        "spread" => Placement::Spread {
            budget_bytes: budget.ok_or("placement 'spread' needs a budget in bytes")?,
        },
        other => return Err(format!("'{other}' is not keyframe, next or spread")),
    };
    if budget.is_some() && !matches!(placement, Placement::Spread { .. }) {
        return Err("a budget belongs to placement 'spread' and to no other".into());
    }
    if let Placement::Spread { budget_bytes } = placement {
        if budget_bytes > UNIT_SOFT_LIMIT {
            return Err(format!(
                "a budget of {budget_bytes} bytes is past the {UNIT_SOFT_LIMIT} a unit stays under"
            ));
        }
    }

    let escapes = match row.get("escapes") {
        None => 2,
        Some(value) => {
            let count = value.as_i64().ok_or("escapes that are not a number")?;
            if !(0..=i64::from(MAX_ESCAPES)).contains(&count) {
                return Err(format!("{count} escapes is not 0 to {MAX_ESCAPES}"));
            }
            count as usize
        }
    };
    let plane_cap = match row.get("planes") {
        None => None,
        Some(value) => {
            let count = value.as_i64().ok_or("planes that are not a number")?;
            if !(1..=8).contains(&count) {
                return Err(format!("{count} planes is not 1 to 8"));
            }
            Some(count as u8)
        }
    };

    let mut config = Config::new(spaces);
    config.placement = placement;
    config.escapes = escapes;
    config.plane_cap = plane_cap;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffrwd_index_core::message::{Encoding, Modality};

    #[test]
    fn the_smallest_params_name_one_space() {
        let config = read(r#"{"spaces":[{"name":"clip","dims":512}]}"#).expect("params");
        assert_eq!(config.spaces.len(), 1);
        assert_eq!(config.spaces[0].0, "clip");
        assert_eq!(config.spaces[0].1.space_id, 0);
        assert_eq!(config.spaces[0].1.dims, 512);
        assert_eq!(config.spaces[0].1.encoding, Encoding::I8);
        assert_eq!(config.placement, Placement::Keyframe);
        assert_eq!(config.escapes, 2);
        assert_eq!(config.plane_cap, None);
    }

    #[test]
    fn ids_are_handed_out_in_declaration_order() {
        let config = read(
            r#"{"spaces":[
                {"name":"clip","dims":512,"modality":"picture","unit_length":true},
                {"name":"text","dims":384,"encoding":"f16","modality":"speech"}
            ]}"#,
        )
        .expect("params");
        assert_eq!(config.spaces[0].1.space_id, 0);
        assert_eq!(config.spaces[1].1.space_id, 1);
        assert!(config.spaces[0].1.unit_length());
        assert_eq!(config.spaces[0].1.modality, Modality::Picture);
        assert_eq!(config.spaces[1].1.encoding, Encoding::F16);
    }

    #[test]
    fn a_budget_belongs_to_spread_and_spread_needs_one() {
        let spread =
            read(r#"{"spaces":[{"name":"c","dims":8}],"placement":"spread","budget":256}"#)
                .expect("params");
        assert_eq!(spread.placement, Placement::Spread { budget_bytes: 256 });
        assert!(
            read(r#"{"spaces":[{"name":"c","dims":8}],"placement":"spread"}"#)
                .unwrap_err()
                .contains("needs a budget")
        );
        assert!(
            read(r#"{"spaces":[{"name":"c","dims":8}],"placement":"next","budget":256}"#)
                .unwrap_err()
                .contains("belongs to placement")
        );
    }

    #[test]
    fn what_is_refused_is_refused_by_name() {
        for (params, wanted) in [
            ("", "empty"),
            ("not json", "not one JSON object"),
            (r#"{"spaces":[]}"#, "at least one space"),
            (r#"{"placement":"next"}"#, "no spaces"),
            (
                r#"{"spaces":[{"name":"c","dims":8}],"placemant":"next"}"#,
                "not a param",
            ),
            (
                r#"{"spaces":[{"name":"c","dims":8}],"placement":"soon"}"#,
                "not keyframe",
            ),
            (
                r#"{"spaces":[{"name":"c","dims":8}],"escapes":17}"#,
                "0 to 16",
            ),
            (r#"{"spaces":[{"name":"c","dims":8}],"planes":9}"#, "1 to 8"),
            (r#"{"spaces":[{"name":"c","dims":8}],"planes":0}"#, "1 to 8"),
            (r#"{"spaces":[{"dims":8}]}"#, "no name"),
            (r#"{"spaces":[{"name":"","dims":8}]}"#, "empty name"),
            (r#"{"spaces":[{"name":"c"}]}"#, "no dims"),
            (
                r#"{"spaces":[{"name":"c","dims":8,"colour":"red"}]}"#,
                "not a field",
            ),
            (
                r#"{"spaces":[{"name":"c","dims":8},{"name":"c","dims":4}]}"#,
                "two spaces are named",
            ),
            (
                r#"{"spaces":[{"name":"c","dims":8}],"placement":"spread","budget":99999}"#,
                "stays under",
            ),
        ] {
            assert!(
                read(params).unwrap_err().contains(wanted),
                "{params} was not refused for {wanted}"
            );
        }
    }
}
