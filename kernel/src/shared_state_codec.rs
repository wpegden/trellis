//! `trellis-shared-state/1` — decoder for the structural-sharing container
//! format used by `.trellis-history/supervisor_state.json`.
//!
//! The normative specification is `SHARED_STATE_SPEC` in
//! `trellis/history_artifacts.py`; the cross-language contract is the golden
//! vector set in `tests/fixtures/shared_state_vectors/` (repo root, not
//! `kernel/tests/fixtures/`). This module implements the *decode* half only —
//! the kernel never writes this file, the Python supervisor does.
//!
//! Two properties matter for the kernel:
//!
//! * **Identity on the old format.** Detection is purely structural: a
//!   document is encoded iff it is a JSON object carrying a `$format` member.
//!   Every historical checkpoint blob lacks one, so [`decode_shared_state`]
//!   hands it back unchanged and a rewind to any tag keeps working. There is
//!   no flag, no env var and no commit-ancestry test.
//! * **Deep-copy expansion.** Each `{"$r": key}` occurrence expands to an
//!   independent [`Value`]; `serde_json::Value` owns its children, so no two
//!   positions in the decoded document can alias.
//!
//! Every malformed document is a hard error. The decoder never substitutes a
//! default, an empty object or a passthrough fallback: a `$format` it does not
//! recognise, a dangling reference or a `$pool` cycle all abort the decode.

use std::collections::BTreeSet;
use std::fmt;

use serde_json::{Map, Value};

/// The one format string version 1 accepts. Anything else is an error.
pub const SHARED_STATE_FORMAT: &str = "trellis-shared-state/1";

/// Top-level keys whose values are encoded. Every other top-level member is
/// carried through verbatim, which is why readers that only want
/// `event_count` (`segment_event_log`) need no decode step at all.
pub const SHARED_STATE_ENCODED_KEYS: [&str; 2] = ["checkpoint", "state"];

/// Envelope members version 1 defines. A `$`-prefixed top-level member
/// outside this set is rejected: an extension must bump `$format` rather than
/// smuggle a member past an older reader.
const ENVELOPE_MEMBERS: [&str; 3] = ["$format", "$strings", "$pool"];

/// A malformed or unreadable shared-state document.
///
/// The messages mirror `SharedStateError` in `trellis/history_artifacts.py`
/// closely enough that the `error_contains` substrings pinned by the failure
/// vectors match in both languages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedStateError {
    message: String,
}

impl SharedStateError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for SharedStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SharedStateError {}

impl From<SharedStateError> for String {
    fn from(err: SharedStateError) -> String {
        err.message
    }
}

/// JSON type name used in error messages.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Structural detection: a document is `trellis-shared-state/1`-shaped iff it
/// is a JSON object with a `$format` member. No flag, no env var, no
/// SHA-ancestry test.
pub fn is_shared_state(document: &Value) -> bool {
    matches!(document, Value::Object(map) if map.contains_key("$format"))
}

/// Decode `document`, or return it unchanged when it is a plain snapshot.
///
/// Every `$r` occurrence expands to a fresh copy, so no two positions in the
/// result share ownership.
pub fn decode_shared_state(document: Value) -> Result<Value, SharedStateError> {
    if !is_shared_state(&document) {
        // A document carrying the tables but no `$format` would otherwise be
        // misread as plain and hand raw `{"$r": ...}` references back to the
        // caller. Reject it instead.
        if let Value::Object(map) = &document {
            for key in ["$strings", "$pool"] {
                if map.contains_key(key) {
                    return Err(SharedStateError::new(format!(
                        "document carries {key} but no $format member; it is neither a plain \
                         snapshot nor a well-formed shared-state document"
                    )));
                }
            }
        }
        return Ok(document);
    }

    let Value::Object(mut map) = document else {
        unreachable!("is_shared_state only accepts objects");
    };

    let format = map
        .remove("$format")
        .expect("is_shared_state guarantees a $format member");
    if format.as_str() != Some(SHARED_STATE_FORMAT) {
        return Err(SharedStateError::new(format!(
            "unsupported supervisor-state format: {format}"
        )));
    }

    let strings = match map.remove("$strings") {
        Some(Value::Object(table)) => table,
        _ => {
            return Err(SharedStateError::new(
                "shared-state document has no $strings object",
            ))
        }
    };
    let pool = match map.remove("$pool") {
        Some(Value::Object(table)) => table,
        _ => {
            return Err(SharedStateError::new(
                "shared-state document has no $pool object",
            ))
        }
    };

    let mut expand = Expand {
        strings: &strings,
        pool: &pool,
        active: Vec::new(),
        active_set: BTreeSet::new(),
    };

    let mut out = Map::new();
    for (key, value) in map {
        if key.starts_with('$') {
            // `$format`/`$strings`/`$pool` were removed above, so anything
            // left is an unknown envelope member.
            debug_assert!(!ENVELOPE_MEMBERS.contains(&key.as_str()));
            return Err(SharedStateError::new(format!(
                "unrecognised envelope member {key:?} for {SHARED_STATE_FORMAT}; \
                 an extension must bump $format"
            )));
        }
        if SHARED_STATE_ENCODED_KEYS.contains(&key.as_str()) {
            let decoded = expand.node(&value)?;
            out.insert(key, decoded);
        } else {
            out.insert(key, value);
        }
    }
    Ok(Value::Object(out))
}

/// Parse `text` as JSON and decode it if it is a shared-state document.
pub fn decode_shared_state_str(text: &str) -> Result<Value, SharedStateError> {
    let parsed: Value = serde_json::from_str(text)
        .map_err(|err| SharedStateError::new(format!("failed to parse JSON: {err}")))?;
    decode_shared_state(parsed)
}

/// Parse `bytes` as JSON and decode it if it is a shared-state document.
pub fn decode_shared_state_slice(bytes: &[u8]) -> Result<Value, SharedStateError> {
    let parsed: Value = serde_json::from_slice(bytes)
        .map_err(|err| SharedStateError::new(format!("failed to parse JSON: {err}")))?;
    decode_shared_state(parsed)
}

struct Expand<'a> {
    strings: &'a Map<String, Value>,
    pool: &'a Map<String, Value>,
    /// `$pool` keys on the current expansion path, in order, for the cycle
    /// error message.
    active: Vec<&'a str>,
    /// Same set, for O(log n) membership on deep chains.
    active_set: BTreeSet<&'a str>,
}

impl<'a> Expand<'a> {
    fn node(&mut self, value: &Value) -> Result<Value, SharedStateError> {
        match value {
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(self.node(item)?);
                }
                Ok(Value::Array(out))
            }
            Value::Object(map) => {
                if map.len() == 1 {
                    let (key, inner) = map
                        .iter()
                        .next()
                        .expect("len() == 1 guarantees one member");
                    match key.as_str() {
                        "$r" => return self.expand(inner),
                        "$s" => return self.string(inner),
                        "$e" => {
                            let Value::Object(literal) = inner else {
                                return Err(SharedStateError::new(format!(
                                    "\"$e\" must hold an object, got {}",
                                    type_name(inner)
                                )));
                            };
                            // The inner object is a literal: its own
                            // single-member sigil shape is NOT re-interpreted,
                            // but its member values still decode.
                            let mut out = Map::new();
                            for (member, child) in literal {
                                out.insert(member.clone(), self.node(child)?);
                            }
                            return Ok(Value::Object(out));
                        }
                        _ => {}
                    }
                }
                let mut out = Map::new();
                for (member, child) in map {
                    out.insert(member.clone(), self.node(child)?);
                }
                Ok(Value::Object(out))
            }
            scalar => Ok(scalar.clone()),
        }
    }

    fn string(&self, key: &Value) -> Result<Value, SharedStateError> {
        let Value::String(key) = key else {
            return Err(SharedStateError::new(format!(
                "\"$s\" reference must be a string, got {}",
                type_name(key)
            )));
        };
        let Some(text) = self.strings.get(key.as_str()) else {
            return Err(SharedStateError::new(format!(
                "\"$s\" reference {key:?} is absent from $strings"
            )));
        };
        if !text.is_string() {
            return Err(SharedStateError::new(format!(
                "$strings[{key:?}] is not a string"
            )));
        }
        Ok(text.clone())
    }

    fn expand(&mut self, key: &Value) -> Result<Value, SharedStateError> {
        let Value::String(key) = key else {
            return Err(SharedStateError::new(format!(
                "\"$r\" reference must be a string, got {}",
                type_name(key)
            )));
        };
        // Borrow the key out of `self.pool`, not out of the caller's node, so
        // the cycle stack outlives the recursive call. Copying the `&'a Map`
        // out of `self` first keeps that borrow independent of `&mut self`.
        let pool: &'a Map<String, Value> = self.pool;
        let Some((pooled_key, body)) = pool.get_key_value(key.as_str()) else {
            return Err(SharedStateError::new(format!(
                "\"$r\" reference {key:?} is absent from $pool"
            )));
        };
        let pooled_key: &'a str = pooled_key.as_str();
        if self.active_set.contains(pooled_key) {
            let mut chain: Vec<&str> = self.active.clone();
            chain.push(pooled_key);
            return Err(SharedStateError::new(format!(
                "$pool reference cycle: {}",
                chain.join(" -> ")
            )));
        }
        self.active.push(pooled_key);
        self.active_set.insert(pooled_key);
        let result = self.node(body);
        self.active.pop();
        self.active_set.remove(pooled_key);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_documents_pass_through_unchanged() {
        for plain in [
            json!({}),
            json!({"event_count": 5841, "state": {"phase": "ProofFormalization"}}),
            json!({"state": {"nested": {"$r": "looks-like-a-ref"}}}),
            json!([1, 2, 3]),
            json!("bare string"),
            Value::Null,
        ] {
            assert_eq!(decode_shared_state(plain.clone()).unwrap(), plain);
        }
    }

    #[test]
    fn detection_is_structural() {
        assert!(is_shared_state(&json!({"$format": "trellis-shared-state/1"})));
        assert!(is_shared_state(&json!({"$format": 3})));
        assert!(!is_shared_state(&json!({"state": {}})));
        assert!(!is_shared_state(&json!([{"$format": "x"}])));
        // A nested `$format` is payload, not an envelope.
        assert!(!is_shared_state(&json!({"state": {"$format": "x"}})));
    }

    #[test]
    fn expansions_are_independent_deep_copies() {
        let document = json!({
            "$format": SHARED_STATE_FORMAT,
            "$strings": {},
            "$pool": {"aaaaaaaaaaaaaaaa": {"nodes": ["Alpha"]}},
            "checkpoint": {"committed": {"$r": "aaaaaaaaaaaaaaaa"}},
            "state": {"committed": {"$r": "aaaaaaaaaaaaaaaa"}},
        });
        let mut decoded = decode_shared_state(document).unwrap();
        assert_eq!(decoded["checkpoint"]["committed"], decoded["state"]["committed"]);

        decoded["state"]["committed"]["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!("Beta"));
        assert_eq!(
            decoded["checkpoint"]["committed"]["nodes"],
            json!(["Alpha"]),
            "mutating one expansion must not be visible through the other"
        );
    }

    #[test]
    fn diamond_expansion_of_one_key_does_not_alias() {
        let document = json!({
            "$format": SHARED_STATE_FORMAT,
            "$strings": {},
            "$pool": {
                "1111111111111111": {"leaf": [0]},
                "2222222222222222": {"a": {"$r": "1111111111111111"},
                                     "b": {"$r": "1111111111111111"}},
            },
            "state": {"x": {"$r": "2222222222222222"}, "y": {"$r": "2222222222222222"}},
        });
        let mut decoded = decode_shared_state(document).unwrap();
        decoded["state"]["x"]["a"]["leaf"]
            .as_array_mut()
            .unwrap()
            .push(json!(1));
        assert_eq!(decoded["state"]["x"]["b"]["leaf"], json!([0]));
        assert_eq!(decoded["state"]["y"]["a"]["leaf"], json!([0]));
    }

    #[test]
    fn non_encoded_top_level_members_are_never_expanded() {
        // `metadata` is passthrough: a reference-shaped object inside it is
        // payload and must survive verbatim.
        let document = json!({
            "$format": SHARED_STATE_FORMAT,
            "metadata": {"$r": "aaaaaaaaaaaaaaaa"},
            "$strings": {},
            "$pool": {"aaaaaaaaaaaaaaaa": {"nodes": []}},
        });
        let decoded = decode_shared_state(document).unwrap();
        assert_eq!(decoded["metadata"], json!({"$r": "aaaaaaaaaaaaaaaa"}));
    }

    #[test]
    fn escape_inner_object_is_literal_but_its_values_decode() {
        let document = json!({
            "$format": SHARED_STATE_FORMAT,
            "$strings": {"aaaaaaaaaaaaaaaa": "interned"},
            "$pool": {},
            "state": {"x": {"$e": {"$r": {"$s": "aaaaaaaaaaaaaaaa"}}}},
        });
        let decoded = decode_shared_state(document).unwrap();
        assert_eq!(decoded["state"]["x"], json!({"$r": "interned"}));
    }

    #[test]
    fn string_table_entry_must_be_a_string() {
        let document = json!({
            "$format": SHARED_STATE_FORMAT,
            "$strings": {"aaaaaaaaaaaaaaaa": 17},
            "$pool": {},
            "state": {"x": {"$s": "aaaaaaaaaaaaaaaa"}},
        });
        let err = decode_shared_state(document).unwrap_err();
        assert!(err.to_string().contains("is not a string"), "{err}");
    }

    #[test]
    fn missing_strings_table_is_an_error() {
        let document = json!({
            "$format": SHARED_STATE_FORMAT,
            "$pool": {},
            "state": {},
        });
        let err = decode_shared_state(document).unwrap_err();
        assert!(err.to_string().contains("no $strings object"), "{err}");
    }

    #[test]
    fn non_object_tables_are_an_error() {
        for table in ["$strings", "$pool"] {
            let mut document = json!({
                "$format": SHARED_STATE_FORMAT,
                "$strings": {},
                "$pool": {},
                "state": {},
            });
            document[table] = json!([]);
            let err = decode_shared_state(document).unwrap_err();
            assert!(err.to_string().contains(&format!("no {table} object")), "{err}");
        }
    }

    #[test]
    fn long_pool_chain_is_not_mistaken_for_a_cycle() {
        let mut pool = Map::new();
        let keys: Vec<String> = (0..64).map(|i| format!("{i:016x}")).collect();
        for (idx, key) in keys.iter().enumerate() {
            let body = match keys.get(idx + 1) {
                Some(next) => json!({"next": {"$r": next}}),
                None => json!({"leaf": true}),
            };
            pool.insert(key.clone(), body);
        }
        let document = json!({
            "$format": SHARED_STATE_FORMAT,
            "$strings": {},
            "$pool": Value::Object(pool),
            // The same chain head twice: the cycle stack must unwind between
            // sibling expansions.
            "state": {"a": {"$r": keys[0]}, "b": {"$r": keys[0]}},
        });
        let decoded = decode_shared_state(document).unwrap();
        assert_eq!(decoded["state"]["a"], decoded["state"]["b"]);
    }
}
