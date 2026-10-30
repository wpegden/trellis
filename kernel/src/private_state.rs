//! Read plain runtime state or a self-contained task-private shared carrier.
//! Canonical runtime writers deliberately remain unchanged.
use serde_json::Value;
use std::{fs, io::Write, path::{Path, Component}};
use sha2::{Digest, Sha256};

pub fn decode_value(document: Value) -> Result<Value, String> {
    let object = document.as_object().ok_or("private protocol state must be an object")?;
    if !object.contains_key("$format") { return Ok(document); }
    if object.len() != 4 || ["$format", "$strings", "$pool", "state"].iter().any(|k| !object.contains_key(*k)) {
        return Err("private shared-state carrier has unexpected members".into());
    }
    let mut decoded = crate::shared_state_codec::decode_shared_state(document).map_err(|e| e.to_string())?;
    let state = decoded.as_object_mut().unwrap().remove("state").unwrap();
    if !state.is_object() { return Err("private shared-state payload must be an object".into()); }
    Ok(state)
}

pub fn from_slice(bytes: &[u8]) -> Result<crate::ProtocolState, serde_json::Error> {
    let value = decode_value(serde_json::from_slice(bytes)?)
        .map_err(|e| serde_json::Error::io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
    serde_json::from_value(value)
}

/// Write only a new file inside an unpublished private workspace. No canonical
/// transaction or in-place rewriting is performed by this storage operation.
pub fn write_new(path: &Path, state: Value) -> Result<Value, String> {
    if !state.is_object() || state.get("$format").is_some() || state.get("dispatch").is_some_and(|v| !v.is_null()) {
        return Err("private snapshot must be a plain state without a dispatch ledger".into());
    }
    if !path.is_absolute() || path.file_name().and_then(|v| v.to_str()) != Some("protocol_state.json") {
        return Err("private snapshot destination must be an absolute protocol_state.json path".into());
    }
    let mut parent = std::path::PathBuf::new();
    for part in path.parent().unwrap().components() {
        match part {
            Component::RootDir | Component::Normal(_) => parent.push(part),
            _ => return Err("invalid private snapshot destination".into()),
        }
        if !fs::symlink_metadata(&parent).map_err(|e| e.to_string())?.is_dir() {
            return Err("private snapshot parent is not a regular directory".into());
        }
    }
    if fs::symlink_metadata(path).is_ok() { return Err("private snapshot destination already exists".into()); }
    let document = Value::Object(serde_json::Map::from_iter([("state".into(), state)]));
    let Some(encoded) = crate::shared_state_encoder::encode_integer_shared_state(&document)? else {
        return Ok(serde_json::json!({"status":"unsupported_float"}));
    };
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).map_err(|e| e.to_string())?;
    struct Hashed<W> { sink: W, hash: Sha256, bytes: u64 }
    impl<W: Write> Write for Hashed<W> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let n = self.sink.write(bytes)?; self.hash.update(&bytes[..n]); self.bytes += n as u64; Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> { self.sink.flush() }
    }
    let result = (|| {
        let mut output = std::io::BufWriter::new(Hashed { sink: file, hash: Sha256::new(), bytes: 0 });
        serde_json::to_writer(&mut output, &encoded).map_err(|e| e.to_string())?;
        output.flush().map_err(|e| e.to_string())?;
        output.get_ref().sink.sync_all().map_err(|e| e.to_string())?;
        fs::File::open(path.parent().unwrap()).and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
        let output = output.into_inner().map_err(|e| e.to_string())?;
        Ok(serde_json::json!({"status":"written", "bytes":output.bytes,
            "sha256":format!("{:x}",output.hash.finalize())}))
    })();
    if result.is_err() { let _ = fs::remove_file(path); }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn new_file_receipt_binds_bytes_and_preserves_decoded_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("protocol_state.json");
        let state = serde_json::json!({"cycle":1,"a":vec!["large repeated string";100],"b":vec!["large repeated string";100]});
        let receipt = write_new(&path, state.clone()).unwrap();
        let raw = fs::read(&path).unwrap();
        assert_eq!(receipt["bytes"], raw.len());
        assert_eq!(receipt["sha256"], format!("{:x}", Sha256::digest(&raw)));
        assert_eq!(decode_value(serde_json::from_slice(&raw).unwrap()).unwrap(), state);
        assert!(write_new(&path, state).is_err());
        assert_eq!(fs::read(path).unwrap(), raw);
    }
    #[test]
    fn unsupported_float_creates_nothing_and_symlinks_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("protocol_state.json");
        assert_eq!(write_new(&path, serde_json::json!({"value":1.0})).unwrap(),
            serde_json::json!({"status":"unsupported_float"}));
        assert!(!path.exists());
        let target = directory.path().join("target");
        fs::write(&target,b"untouched").unwrap();
        std::os::unix::fs::symlink(&target,&path).unwrap();
        assert!(write_new(&path,serde_json::json!({})).is_err());
        assert_eq!(fs::read(target).unwrap(),b"untouched");
    }
    #[test]
    fn canonical_deserializer_rejects_any_carrier_marker_without_changing_plain_bytes() {
        let state = crate::ProtocolState::default();
        let plain = serde_json::to_value(&state).unwrap();
        assert!(plain.get("$format").is_none());
        assert!(plain.get("private_carrier_rejected").is_none());
        assert_eq!(serde_json::from_value::<crate::ProtocolState>(plain).unwrap(), state);
        for raw in [r#"{"$format":null}"#, r#"{"cycle":4,"$format":"trellis-shared-state/1"}"#] {
            assert!(serde_json::from_str::<crate::ProtocolState>(raw).unwrap_err().to_string()
                .contains("explicit private-state reader"));
        }
    }
    #[test]
    fn private_carrier_rejects_other_envelopes() {
        for value in [serde_json::json!({"$format":"unknown","$pool":{},"$strings":{},"state":{}}),
            serde_json::json!({"$format":"trellis-shared-state/1","$pool":{},"$strings":{},"state":{},"checkpoint":{}})] {
            assert!(decode_value(value).is_err());
        }
    }
}
