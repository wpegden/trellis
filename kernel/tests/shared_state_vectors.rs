//! Cross-language agreement gate for `trellis-shared-state/1`.
//!
//! Drives the committed golden vectors in `tests/fixtures/shared_state_vectors/`
//! (repo root — the same files the Python and JS implementations read) through
//! the kernel decoder. The vectors, not this file, are the contract.
//!
//! Vector kinds:
//!   * `roundtrip` — `decode(encoded)` must equal `decoded`, and `decode(plain)`
//!     must be the identity (a plain document has no `$format`).
//!   * `failure`   — `decode(document)` must error, with `error_contains` as a
//!     substring of the message.
//!   * `digests`   — `H(v)` reference values. Encode-side only: a decoder never
//!     computes a Merkle digest, so the kernel has no obligation here. The
//!     vector is still opened and shape-checked so it cannot rot unnoticed.

use std::fs;
use std::path::PathBuf;

use serde_json::Value;
use trellis_kernel::shared_state_codec::{decode_shared_state, is_shared_state};

fn vector_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("kernel/ has a parent")
        .join("tests")
        .join("fixtures")
        .join("shared_state_vectors")
}

fn load_vectors() -> Vec<(String, Value)> {
    let dir = vector_dir();
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", dir.display()))
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    entries.sort();
    assert!(
        !entries.is_empty(),
        "no golden vectors found in {}",
        dir.display()
    );
    entries
        .into_iter()
        .map(|path| {
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
            let value: Value = serde_json::from_str(&text)
                .unwrap_or_else(|err| panic!("failed to parse {}: {err}", path.display()));
            let name = value["name"].as_str().unwrap_or("<unnamed>").to_string();
            (name, value)
        })
        .collect()
}

#[test]
fn golden_vectors_agree_with_the_reference_implementation() {
    let mut roundtrip = 0usize;
    let mut failure = 0usize;
    let mut digests = 0usize;

    for (name, vector) in load_vectors() {
        // Visible under `--nocapture`; silent in a normal run.
        eprintln!("vector {name} ({})", vector["kind"].as_str().unwrap_or("?"));
        match vector["kind"].as_str() {
            Some("roundtrip") => {
                roundtrip += 1;
                let plain = vector["plain"].clone();
                let encoded = vector["encoded"].clone();
                let expected = vector["decoded"].clone();

                assert!(
                    is_shared_state(&encoded),
                    "{name}: the encoded form must be detected structurally"
                );
                assert!(
                    !is_shared_state(&plain),
                    "{name}: the plain form must NOT be detected as shared state"
                );

                let decoded = decode_shared_state(encoded)
                    .unwrap_or_else(|err| panic!("{name}: decode failed: {err}"));
                assert_eq!(decoded, expected, "{name}: decode(encoded) != decoded");

                let identity = decode_shared_state(plain.clone())
                    .unwrap_or_else(|err| panic!("{name}: decode(plain) failed: {err}"));
                assert_eq!(identity, plain, "{name}: decode(plain) is not the identity");
                assert_eq!(
                    identity, expected,
                    "{name}: decode(plain) and decode(encoded) must agree"
                );
            }
            Some("failure") => {
                failure += 1;
                let document = vector["document"].clone();
                let needle = vector["error_contains"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{name}: failure vector has no error_contains"));
                let err = decode_shared_state(document)
                    .err()
                    .unwrap_or_else(|| panic!("{name}: decode succeeded but must fail"));
                let message = err.to_string();
                assert!(
                    message.contains(needle),
                    "{name}: error {message:?} does not contain {needle:?}"
                );
            }
            Some("digests") => {
                digests += 1;
                let entries = vector["digests"]
                    .as_array()
                    .unwrap_or_else(|| panic!("{name}: digests vector has no digests array"));
                assert!(!entries.is_empty(), "{name}: digests array is empty");
                for entry in entries {
                    let digest = entry["digest"]
                        .as_str()
                        .unwrap_or_else(|| panic!("{name}: entry has no digest string"));
                    assert_eq!(digest.len(), 64, "{name}: digest is not 64 hex characters");
                    assert!(
                        digest.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
                        "{name}: digest is not lowercase hex"
                    );
                }
            }
            other => panic!("{name}: unrecognised vector kind {other:?}"),
        }
    }

    assert!(roundtrip > 0 && failure > 0 && digests > 0, "vector set is incomplete");
}

/// Belt-and-braces on the property the kernel actually depends on: an
/// unencoded historical checkpoint is returned byte-for-byte identical.
#[test]
fn plain_checkpoint_shapes_are_returned_unchanged() {
    for (name, vector) in load_vectors() {
        if vector["kind"].as_str() != Some("roundtrip") {
            continue;
        }
        let plain = vector["plain"].clone();
        let before = serde_json::to_string(&plain).unwrap();
        let after = serde_json::to_string(&decode_shared_state(plain).unwrap()).unwrap();
        assert_eq!(before, after, "{name}: plain document was rewritten");
    }
}

/// Identity against real historical blobs. Reads `.trellis-history/supervisor_state.json`
/// out of a live run's git history, read-only, via `git show`. Skipped unless
/// `TRELLIS_SHARED_STATE_IDENTITY_REPO` names a repo, so the test suite stays
/// self-contained on any machine.
#[test]
fn historical_blobs_decode_to_themselves() {
    let Ok(repo) = std::env::var("TRELLIS_SHARED_STATE_IDENTITY_REPO") else {
        eprintln!("TRELLIS_SHARED_STATE_IDENTITY_REPO unset; skipping historical-blob identity");
        return;
    };
    let path = ".trellis-history/supervisor_state.json";
    let limit: usize = std::env::var("TRELLIS_SHARED_STATE_IDENTITY_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(25);

    let log = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["log", "--format=%H", "--", path])
        .output()
        .expect("git log");
    assert!(log.status.success(), "git log failed in {repo}");
    let text = String::from_utf8(log.stdout).expect("git log utf-8");
    let shas: Vec<&str> = text.lines().map(str::trim).filter(|s| !s.is_empty()).collect();
    assert!(!shas.is_empty(), "no history for {path} in {repo}");

    let step = std::cmp::max(1, shas.len() / limit);
    let mut checked = 0usize;
    for sha in shas.iter().step_by(step) {
        let show = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["show", &format!("{sha}:{path}")])
            .output()
            .expect("git show");
        if !show.status.success() {
            continue;
        }
        let parsed: Value = match serde_json::from_slice(&show.stdout) {
            Ok(value) => value,
            Err(_) => continue,
        };
        assert!(
            !is_shared_state(&parsed),
            "{sha}: historical blob unexpectedly carries $format"
        );
        let decoded = decode_shared_state(parsed.clone())
            .unwrap_or_else(|err| panic!("{sha}: decode failed: {err}"));
        assert_eq!(decoded, parsed, "{sha}: decode(plain) is not the identity");
        checked += 1;
    }
    assert!(checked > 0, "no historical blobs were checked in {repo}");
    eprintln!("historical-blob identity: {checked} of {} revisions", shas.len());
}

/// End-to-end gate for the writer flip: Python encodes a real 70 MB
/// supervisor state, the kernel decodes it, and the result must equal the
/// original plain document. Point `TRELLIS_SHARED_STATE_PLAIN` at the plain
/// snapshot and `TRELLIS_SHARED_STATE_ENCODED` at
/// `encode_shared_state(json.load(plain))` to run it; skipped otherwise.
#[test]
fn python_encoded_state_decodes_back_to_the_plain_document() {
    let (Ok(plain_path), Ok(encoded_path)) = (
        std::env::var("TRELLIS_SHARED_STATE_PLAIN"),
        std::env::var("TRELLIS_SHARED_STATE_ENCODED"),
    ) else {
        eprintln!("TRELLIS_SHARED_STATE_PLAIN/_ENCODED unset; skipping Python-encoder cross-check");
        return;
    };
    let plain: Value = serde_json::from_slice(&fs::read(&plain_path).expect("read plain"))
        .expect("parse plain");
    let encoded: Value = serde_json::from_slice(&fs::read(&encoded_path).expect("read encoded"))
        .expect("parse encoded");
    assert!(!is_shared_state(&plain), "the plain fixture carries $format");
    assert!(is_shared_state(&encoded), "the encoded fixture has no $format");
    let decoded = decode_shared_state(encoded).expect("decode");
    assert_eq!(decoded, plain, "decode(python_encode(plain)) != plain");
}
