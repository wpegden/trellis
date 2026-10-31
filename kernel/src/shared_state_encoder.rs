//! Integer-valued fast path for the existing shared-state/1 format.
//!
//! Floats return None: the caller must use the established Python encoder,
//! whose repr(float) is normative. Never coerce a float into an integer.
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

type Hash = [u8; 32];
fn hex_bytes(value: Hash) -> [u8;64] {
    const DIGITS: &[u8;16] = b"0123456789abcdef";
    let mut result = [0;64];
    for (i, byte) in value.into_iter().enumerate() {
        result[2*i] = DIGITS[(byte >> 4) as usize];
        result[2*i+1] = DIGITS[(byte & 15) as usize];
    }
    result
}
fn hex(value: Hash) -> String { String::from_utf8(hex_bytes(value).to_vec()).unwrap() }
fn string_hash(value: &str) -> Hash {
    let mut hash = Sha256::new(); hash.update(b"s"); hash.update(value.as_bytes()); hash.finalize().into()
}
struct Entry<'a> { value: &'a Value, count: usize, weight: usize }
#[derive(Default)]
struct Scan<'a> {
    entries: HashMap<Hash, Entry<'a>>,
    nodes: HashMap<usize, Hash>,
    key_hashes: HashMap<&'a str, Hash>,
}
impl<'a> Scan<'a> {
    fn visit(&mut self, value: &'a Value) -> Result<(Hash, usize), ()> {
        let (hash, weight) = match value {
            Value::Null => (Sha256::digest(b"n").into(), 4),
            Value::Bool(true) => (Sha256::digest(b"t").into(), 4),
            Value::Bool(false) => (Sha256::digest(b"f").into(), 5),
            Value::Number(number) => {
                if !number.is_i64() && !number.is_u64() { return Err(()); }
                let text = number.to_string();
                let mut hash = Sha256::new(); hash.update(b"i"); hash.update(text.as_bytes());
                (hash.finalize().into(), text.len())
            },
            Value::String(text) => (string_hash(text), 2 + text.len()),
            Value::Array(items) => {
                let mut hash = Sha256::new(); hash.update(b"a");
                let mut weight = 2 + items.len().saturating_sub(1);
                for item in items {
                    let (child, child_weight) = self.visit(item)?;
                    hash.update(hex_bytes(child)); weight += child_weight;
                }
                (hash.finalize().into(), weight)
            },
            Value::Object(items) => {
                let mut hash = Sha256::new(); hash.update(b"o");
                let mut weight = 2 + items.len().saturating_sub(1);
                let mut keys: Vec<_> = items.keys().collect(); keys.sort();
                for key in keys {
                    let key_hash = *self.key_hashes.entry(key.as_str()).or_insert_with(|| string_hash(key));
                    hash.update(hex_bytes(key_hash));
                    let (child, child_weight) = self.visit(&items[key])?;
                    hash.update(hex_bytes(child)); weight += 3 + key.len() + child_weight;
                }
                (hash.finalize().into(), weight)
            },
        };
        self.entries.entry(hash).and_modify(|e| e.count += 1)
            .or_insert(Entry { value, count: 1, weight });
        if matches!(value, Value::String(_) | Value::Object(_) | Value::Array(_)) {
            self.nodes.insert(value as *const Value as usize, hash);
        }
        Ok((hash, weight))
    }
}

struct Emit<'a> {
    nodes: &'a HashMap<usize, Hash>,
    strings_selected: HashMap<Hash, String>,
    pool_selected: HashMap<Hash, String>,
    strings: Map<String, Value>,
    pool: Map<String, Value>,
}
impl Emit<'_> {
    fn node(&mut self, value: &Value) -> Value {
        if matches!(value, Value::String(_) | Value::Object(_) | Value::Array(_)) {
            let hash = self.nodes[&(value as *const Value as usize)];
            if let Some(key) = self.strings_selected.get(&hash) {
                self.strings.entry(key.clone()).or_insert_with(|| value.clone());
                return json!({"$s": key});
            }
            if let Some(key) = self.pool_selected.get(&hash).cloned() {
                if !self.pool.contains_key(&key) {
                    // A finite JSON tree cannot contain its own digest unless
                    // SHA256 collides; the selection phase rejects key clashes.
                    let body = self.body(value);
                    self.pool.insert(key.clone(), body);
                }
                return json!({"$r": key});
            }
        }
        self.body(value)
    }
    fn body(&mut self, value: &Value) -> Value {
        match value {
            Value::Array(items) => Value::Array(items.iter().map(|v| self.node(v)).collect()),
            Value::Object(items) => {
                let map: Map<_, _> = items.iter().map(|(k,v)| (k.clone(), self.node(v))).collect();
                if map.len() == 1 && map.keys().any(|k| matches!(k.as_str(), "$r" | "$s" | "$e")) {
                    json!({"$e": map})
                } else { Value::Object(map) }
            },
            _ => value.clone(),
        }
    }
}

pub fn encode_integer_shared_state(document: &Value) -> Result<Option<Value>, String> {
    let object = document.as_object().ok_or("shared-state document must be an object")?;
    if object.keys().any(|k| k.starts_with('$')) { return Err("shared-state envelope key conflict".into()); }
    let mut scan = Scan::default();
    for key in ["checkpoint", "state"] {
        if let Some(value) = object.get(key) {
            if scan.visit(value).is_err() { return Ok(None); }
        }
    }
    let mut strings_selected = HashMap::new();
    let mut pool_selected = HashMap::new();
    let mut taken = BTreeMap::new();
    for (hash, entry) in &scan.entries {
        if entry.count < 2 { continue; }
        let selected = match entry.value {
            Value::String(text) => entry.count * entry.weight > entry.count * 25 + 22 + text.len(),
            Value::Array(_) | Value::Object(_) => entry.count * entry.weight > entry.count * 25 + 20 + entry.weight,
            _ => false,
        };
        if !selected { continue; }
        let key = hex(*hash)[..16].to_owned();
        if taken.insert(key.clone(), *hash).is_some_and(|old| old != *hash) {
            return Err("shared-state table key collision".into());
        }
        if entry.value.is_string() { strings_selected.insert(*hash, key); }
        else { pool_selected.insert(*hash, key); }
    }
    let mut emit = Emit { nodes: &scan.nodes, strings_selected, pool_selected,
        strings: Map::new(), pool: Map::new() };
    let mut output = Map::new();
    output.insert("$format".into(), Value::String(crate::shared_state_codec::SHARED_STATE_FORMAT.into()));
    for (key, value) in object {
        output.insert(key.clone(), if key == "checkpoint" || key == "state" { emit.node(value) } else { value.clone() });
    }
    output.insert("$strings".into(), Value::Object(emit.strings));
    output.insert("$pool".into(), Value::Object(emit.pool));
    Ok(Some(Value::Object(output)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_encoding_matches_normative_integer_vectors() {
        let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tests/fixtures/shared_state_vectors");
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            let vector: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            if vector["kind"] != "roundtrip" { continue; }
            let encoded = encode_integer_shared_state(&vector["plain"]).unwrap();
            if path.file_name().unwrap() == "02_scalars_only.json" {
                assert!(encoded.is_none());
            } else {
                assert_eq!(encoded.unwrap(), vector["encoded"], "{}", path.display());
            }
        }
    }
    #[test]
    fn integer_limits_remain_integers_and_floats_are_explicit_fallbacks() {
        let document = json!({"state":{"min":i64::MIN,"max":u64::MAX,"text":"λ😀"}});
        let encoded = encode_integer_shared_state(&document).unwrap().unwrap();
        assert_eq!(crate::shared_state_codec::decode_shared_state(encoded).unwrap(), document);
        for text in ["1.0", "-0.0", "1e100"] {
            let document: Value = serde_json::from_str(&format!("{{\"state\":{{\"nested\":[{text}]}}}}" )).unwrap();
            assert!(encode_integer_shared_state(&document).unwrap().is_none());
        }
    }
}
