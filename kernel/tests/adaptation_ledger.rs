use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use proptest::prelude::*;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::process::Command;
use trellis_kernel::trust_base::{
    canonical_json, canonical_json_value, parse_compatible_adaptation_ledger, raw_sha256,
    replay_phase0_adaptation, source_tree_digest, source_tree_manifest, AdaptationCitation,
    AdaptationSeamClass, ByteSpan, CheckerStream, CompatibleAdaptationLedger, ExactBytePatch,
    Phase0AdaptationEntry, Phase0AdaptationLedger, Phase0FileOperation, Phase0LedgerStatus,
    QuotedCheckerError, Sha256Digest, PHASE0_ADAPTATION_LEDGER_SCHEMA, DomainTag,
    self_digest, tagged_hash,
};

fn targets() -> BTreeSet<String> {
    BTreeSet::from(["goal:target".to_owned()])
}

fn write_tree(root: &std::path::Path, files: &[(&str, &[u8])]) {
    fs::create_dir(root).unwrap();
    for (relative, bytes) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
}

#[allow(clippy::too_many_arguments)]
fn entry(
    file: &str,
    operation: Phase0FileOperation,
    before_file: Option<&[u8]>,
    after_file: Option<&[u8]>,
    before_span: (u64, u64),
    after_span: (u64, u64),
    removed: &[u8],
    inserted: &[u8],
) -> Phase0AdaptationEntry {
    let quote = "tool failed";
    Phase0AdaptationEntry {
        id: String::new(),
        file: file.to_owned(),
        operation,
        before_span: ByteSpan {
            start: before_span.0,
            end: before_span.1,
        },
        after_span: ByteSpan {
            start: after_span.0,
            end: after_span.1,
        },
        before_sha256: before_file.map(raw_sha256),
        after_sha256: after_file.map(raw_sha256),
        patch: ExactBytePatch {
            removed_base64: BASE64_STANDARD.encode(removed),
            inserted_base64: BASE64_STANDARD.encode(inserted),
            removed_sha256: Sha256Digest::ZERO,
            inserted_sha256: Sha256Digest::ZERO,
        },
        diff_sha256: Sha256Digest::ZERO,
        quoted_error: QuotedCheckerError {
            tool: "cargo".to_owned(),
            receipt_sha256: raw_sha256(b"discovery receipt"),
            stage_index: 1,
            stream: CheckerStream::Stderr,
            byte_offset: 7,
            byte_length: quote.len() as u64,
            quote: quote.to_owned(),
            quote_sha256: raw_sha256(quote.as_bytes()),
        },
        discovery_failure_receipt_sha256: raw_sha256(b"discovery receipt"),
        ablation_failure_receipt_sha256: raw_sha256(b"ablation receipt"),
        behavior_preservation_claim: "This atomic edit preserves the target behavior.".to_owned(),
        seam_class: AdaptationSeamClass::PackagingShim,
        citation: AdaptationCitation::LanguageGuarantee {
            statement: "Rust module and package boundaries preserve the delegated body.".to_owned(),
        },
        affected_targets: vec!["goal:target".to_owned()],
        meaning_change: false,
        status: Phase0LedgerStatus::Draft,
    }
}

fn ledger(
    generation: u64,
    unadapted: &std::path::Path,
    adapted: &std::path::Path,
    entries: Vec<Phase0AdaptationEntry>,
) -> Phase0AdaptationLedger {
    Phase0AdaptationLedger {
        schema: PHASE0_ADAPTATION_LEDGER_SCHEMA.to_owned(),
        generation,
        unadapted_tree_sha256: source_tree_digest(unadapted).unwrap(),
        adapted_tree_sha256: source_tree_digest(adapted).unwrap(),
        goal_sha256: raw_sha256(b"# target\n"),
        entries,
        ledger_entries_root: Sha256Digest::ZERO,
        ledger_sha256: Sha256Digest::ZERO,
    }
    .seal()
    .unwrap()
}

#[test]
fn python_and_rust_source_tree_manifests_agree_byte_for_byte() {
    let temp = tempfile::tempdir().unwrap();
    let tree = temp.path().join("tree");
    write_tree(
        &tree,
        &[
            ("z/bytes.bin", b"\x00\xff\r\n"),
            ("unicode-π.rs", b"pub fn value() {}\n"),
        ],
    );
    let rust = canonical_json(&source_tree_manifest(&tree).unwrap()).unwrap();
    let repository = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let output = Command::new("python3")
        .current_dir(repository)
        .arg("-c")
        .arg(
            "import sys; from pathlib import Path; from scripts.pv_source_tree import canonical_json, source_tree_manifest; sys.stdout.buffer.write(canonical_json(source_tree_manifest(Path(sys.argv[1]))))",
        )
        .arg(&tree)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, rust);
}

#[test]
fn replay_handles_disjoint_multibyte_modify_add_and_delete() {
    let temp = tempfile::tempdir().unwrap();
    let raw = temp.path().join("raw");
    let adapted = temp.path().join("adapted");
    let replay = temp.path().join("replay");
    let before = b"alpha\xce\xb2omega";
    let after = b"alphaBEND";
    write_tree(&raw, &[("mod.rs", before), ("delete.rs", b"old\n")]);
    write_tree(&adapted, &[("mod.rs", after), ("Cargo.toml", b"[package]\n")]);
    let entries = vec![
        entry(
            "mod.rs",
            Phase0FileOperation::Modify,
            Some(before),
            Some(after),
            (5, 7),
            (5, 6),
            b"\xce\xb2",
            b"B",
        ),
        entry(
            "mod.rs",
            Phase0FileOperation::Modify,
            Some(before),
            Some(after),
            (7, 12),
            (6, 9),
            b"omega",
            b"END",
        ),
        entry(
            "Cargo.toml",
            Phase0FileOperation::Add,
            None,
            Some(b"[package]\n"),
            (0, 0),
            (0, 10),
            b"",
            b"[package]\n",
        ),
        entry(
            "delete.rs",
            Phase0FileOperation::Delete,
            Some(b"old\n"),
            None,
            (0, 4),
            (0, 0),
            b"old\n",
            b"",
        ),
    ];
    let ledger = ledger(4, &raw, &adapted, entries);
    ledger.validate(&targets()).unwrap();
    let root = replay_phase0_adaptation(&raw, &ledger, &targets(), &adapted, &replay).unwrap();
    assert_eq!(root, source_tree_digest(&adapted).unwrap());
    assert_eq!(source_tree_manifest(&replay).unwrap(), source_tree_manifest(&adapted).unwrap());
}

#[test]
fn identity_ledger_replays_the_complete_tree() {
    let temp = tempfile::tempdir().unwrap();
    let raw = temp.path().join("raw");
    let adapted = temp.path().join("adapted");
    let replay = temp.path().join("replay");
    write_tree(&raw, &[("Cargo.toml", b"[package]\n"), ("src/lib.rs", b"pub fn f() {}\n")]);
    write_tree(&adapted, &[("Cargo.toml", b"[package]\n"), ("src/lib.rs", b"pub fn f() {}\n")]);
    let ledger = ledger(0, &raw, &adapted, vec![]);
    replay_phase0_adaptation(&raw, &ledger, &targets(), &adapted, &replay).unwrap();
    assert_eq!(source_tree_digest(&raw).unwrap(), source_tree_digest(&replay).unwrap());
}

#[test]
fn replay_rejects_overlap_zero_op_wrong_bytes_and_unlogged_paths() {
    let temp = tempfile::tempdir().unwrap();
    let raw = temp.path().join("raw");
    let adapted = temp.path().join("adapted");
    write_tree(&raw, &[("a.rs", b"abcdef")]);
    write_tree(&adapted, &[("a.rs", b"aXYf")]);

    let overlap = ledger(
        1,
        &raw,
        &adapted,
        vec![
            entry("a.rs", Phase0FileOperation::Modify, Some(b"abcdef"), Some(b"aXYf"), (1, 4), (1, 2), b"bcd", b"X"),
            entry("a.rs", Phase0FileOperation::Modify, Some(b"abcdef"), Some(b"aXYf"), (3, 5), (2, 3), b"de", b"Y"),
        ],
    );
    assert!(replay_phase0_adaptation(
        &raw, &overlap, &targets(), &adapted, &temp.path().join("overlap")
    ).unwrap_err().detail.contains("overlap"));

    let unchanged = temp.path().join("unchanged");
    write_tree(&unchanged, &[("a.rs", b"abcdef")]);
    let zero = ledger(
        1,
        &raw,
        &unchanged,
        vec![entry("a.rs", Phase0FileOperation::Modify, Some(b"abcdef"), Some(b"abcdef"), (1, 2), (1, 2), b"b", b"b")],
    );
    assert!(zero.validate(&targets()).unwrap_err().detail.contains("zero-op"));

    let wrong = ledger(
        1,
        &raw,
        &adapted,
        vec![entry("a.rs", Phase0FileOperation::Modify, Some(b"abcdef"), Some(b"aXYf"), (1, 5), (1, 3), b"WRONG", b"XY")],
    );
    assert!(replay_phase0_adaptation(
        &raw, &wrong, &targets(), &adapted, &temp.path().join("wrong")
    ).unwrap_err().detail.contains("removed-byte"));

    fs::write(adapted.join("hidden.rs"), b"unlogged\n").unwrap();
    let mut hidden = overlap;
    hidden.entries.clear();
    hidden.adapted_tree_sha256 = source_tree_digest(&adapted).unwrap();
    hidden = hidden.seal().unwrap();
    assert!(replay_phase0_adaptation(
        &raw, &hidden, &targets(), &adapted, &temp.path().join("hidden")
    ).unwrap_err().detail.contains("unlogged"));
}

#[test]
fn replay_rejects_after_span_mismatch_and_noncanonical_entry_order() {
    let temp = tempfile::tempdir().unwrap();
    let raw = temp.path().join("raw");
    let adapted = temp.path().join("adapted");
    write_tree(&raw, &[("a.rs", b"abcdef")]);
    write_tree(&adapted, &[("a.rs", b"aXdeY")]);
    let mut first = entry("a.rs", Phase0FileOperation::Modify, Some(b"abcdef"), Some(b"aXdeY"), (1, 3), (1, 2), b"bc", b"X");
    let second = entry("a.rs", Phase0FileOperation::Modify, Some(b"abcdef"), Some(b"aXdeY"), (5, 6), (4, 5), b"f", b"Y");
    first.after_span = ByteSpan { start: 2, end: 3 };
    let wrong_span = ledger(8, &raw, &adapted, vec![first, second.clone()]);
    assert!(replay_phase0_adaptation(
        &raw, &wrong_span, &targets(), &adapted, &temp.path().join("wrong-span")
    ).unwrap_err().detail.contains("after-span"));

    let first = entry("a.rs", Phase0FileOperation::Modify, Some(b"abcdef"), Some(b"aXdeY"), (1, 3), (1, 2), b"bc", b"X");
    let canonical = ledger(8, &raw, &adapted, vec![first, second]);
    let mut value = serde_json::to_value(canonical).unwrap();
    value["entries"].as_array_mut().unwrap().reverse();
    value["ledger_entries_root"] = Value::String(tagged_hash(
        DomainTag::AdaptationLedgerRow,
        &canonical_json_value(&value["entries"]).unwrap(),
    ).to_string());
    value["ledger_sha256"] = Value::String(Sha256Digest::ZERO.to_string());
    value["ledger_sha256"] = Value::String(
        self_digest(DomainTag::RawArtifact, &value, "ledger_sha256").unwrap().to_string()
    );
    let bytes = canonical_json_value(&value).unwrap();
    assert!(Phase0AdaptationLedger::parse_and_validate(&bytes, &targets())
        .unwrap_err().detail.contains("canonical order"));
}

#[cfg(unix)]
#[test]
fn canonical_source_tree_rejects_links_and_special_files() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let linked = temp.path().join("linked");
    write_tree(&linked, &[("a.rs", b"a")]);
    symlink("a.rs", linked.join("alias.rs")).unwrap();
    assert_eq!(source_tree_digest(&linked).unwrap_err().code, "source_tree_symlink_forbidden");

    let special = temp.path().join("special");
    write_tree(&special, &[("a.rs", b"a")]);
    let fifo = special.join("input.fifo");
    let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    assert_eq!(source_tree_digest(&special).unwrap_err().code, "source_tree_special_file");
}

#[test]
fn validation_rejects_unknown_target_bad_path_seed_seam_and_forged_diff() {
    let temp = tempfile::tempdir().unwrap();
    let raw = temp.path().join("raw");
    let adapted = temp.path().join("adapted");
    write_tree(&raw, &[("a.rs", b"a")]);
    write_tree(&adapted, &[("a.rs", b"b")]);
    let base = entry("a.rs", Phase0FileOperation::Modify, Some(b"a"), Some(b"b"), (0, 1), (0, 1), b"a", b"b");

    let mut unknown = base.clone();
    unknown.affected_targets = vec!["goal:other".to_owned()];
    assert!(ledger(2, &raw, &adapted, vec![unknown]).validate(&targets()).is_err());

    let mut bad_path = base.clone();
    bad_path.file = "../a.rs".to_owned();
    assert!(ledger(2, &raw, &adapted, vec![bad_path]).validate(&targets()).is_err());

    let mut ambiguous_path = base.clone();
    ambiguous_path.file = "nested//a.rs".to_owned();
    assert!(ledger(2, &raw, &adapted, vec![ambiguous_path])
        .validate(&targets())
        .is_err());

    let mut seed = base;
    seed.seam_class = AdaptationSeamClass::Seed;
    assert!(ledger(2, &raw, &adapted, vec![seed]).validate(&targets()).is_err());

    let valid = ledger(2, &raw, &adapted, vec![entry("a.rs", Phase0FileOperation::Modify, Some(b"a"), Some(b"b"), (0, 1), (0, 1), b"a", b"b")]);
    let mut value = serde_json::to_value(valid).unwrap();
    value["entries"][0]["diff_sha256"] = Value::String(raw_sha256(b"forged").to_string());
    let forged = canonical_json_value(&value).unwrap();
    assert!(Phase0AdaptationLedger::parse_and_validate(&forged, &targets()).is_err());
}

#[test]
fn compatible_parser_accepts_empty_legacy_ledgers() {
    let parsed = parse_compatible_adaptation_ledger(b"[]", &targets()).unwrap();
    assert!(matches!(parsed, CompatibleAdaptationLedger::Legacy(rows) if rows.is_empty()));
    let plan = br#"{"adaptation_ledger":[],"schema":"trellis-campaign-trust-plan/v4","targets":[{"resolution":"prove","target_id":"goal:target"}]}"#;
    let parsed = parse_compatible_adaptation_ledger(plan, &targets()).unwrap();
    assert!(matches!(parsed, CompatibleAdaptationLedger::Legacy(rows) if rows.is_empty()));
}

#[test]
fn replay_cli_rejects_one_out_of_ledger_byte() {
    let temp = tempfile::tempdir().unwrap();
    let raw = temp.path().join("raw");
    let adapted = temp.path().join("adapted");
    write_tree(&raw, &[("a.rs", b"a")]);
    write_tree(&adapted, &[("a.rs", b"b")]);
    let mut ledger = ledger(
        3,
        &raw,
        &adapted,
        vec![entry("a.rs", Phase0FileOperation::Modify, Some(b"a"), Some(b"b"), (0, 1), (0, 1), b"a", b"b")],
    );
    fs::write(adapted.join("extra.rs"), b"one out-of-ledger byte: x").unwrap();
    ledger.adapted_tree_sha256 = source_tree_digest(&adapted).unwrap();
    ledger = ledger.seal().unwrap();
    let ledger_path = temp.path().join("ledger.json");
    fs::write(&ledger_path, canonical_json(&ledger).unwrap()).unwrap();
    let targets_path = temp.path().join("targets.json");
    fs::write(
        &targets_path,
        br#"{"schema":"trellis-phase0-goal-target-set/v1","targets":["goal:target"]}"#,
    )
    .unwrap();
    let status = Command::new(env!("CARGO_BIN_EXE_trellis_trust_cli"))
        .arg("replay-phase0-adaptation")
        .arg(&raw)
        .arg(&ledger_path)
        .arg(&targets_path)
        .arg(&adapted)
        .arg(temp.path().join("replay"))
        .status()
        .unwrap();
    assert!(!status.success());
}

#[test]
fn replay_cli_reconstructs_identity_crate_and_multifile_raw_module() {
    let temp = tempfile::tempdir().unwrap();
    let targets_path = temp.path().join("targets.json");
    fs::write(
        &targets_path,
        br#"{"schema":"trellis-phase0-goal-target-set/v1","targets":["goal:target"]}"#,
    )
    .unwrap();

    let identity_raw = temp.path().join("identity-raw");
    let identity_adapted = temp.path().join("identity-adapted");
    write_tree(
        &identity_raw,
        &[("Cargo.toml", b"[package]\n"), ("src/lib.rs", b"pub fn f() {}\n")],
    );
    write_tree(
        &identity_adapted,
        &[("Cargo.toml", b"[package]\n"), ("src/lib.rs", b"pub fn f() {}\n")],
    );
    let identity_ledger = ledger(0, &identity_raw, &identity_adapted, vec![]);
    let identity_path = temp.path().join("identity-ledger.json");
    fs::write(&identity_path, canonical_json(&identity_ledger).unwrap()).unwrap();
    let identity_replay = temp.path().join("identity-replay");
    let identity = Command::new(env!("CARGO_BIN_EXE_trellis_trust_cli"))
        .arg("replay-phase0-adaptation")
        .arg(&identity_raw)
        .arg(&identity_path)
        .arg(&targets_path)
        .arg(&identity_adapted)
        .arg(&identity_replay)
        .status()
        .unwrap();
    assert!(identity.success());
    assert_eq!(
        source_tree_manifest(&identity_replay).unwrap(),
        source_tree_manifest(&identity_adapted).unwrap()
    );

    let module_raw = temp.path().join("module-raw");
    let module_adapted = temp.path().join("module-adapted");
    write_tree(&module_raw, &[("mod.rs", b"abc"), ("nested/traits.rs", b"old")]);
    write_tree(&module_adapted, &[("mod.rs", b"aXc"), ("nested/traits.rs", b"new")]);
    let module_ledger = ledger(
        5,
        &module_raw,
        &module_adapted,
        vec![
            entry(
                "mod.rs",
                Phase0FileOperation::Modify,
                Some(b"abc"),
                Some(b"aXc"),
                (1, 2),
                (1, 2),
                b"b",
                b"X",
            ),
            entry(
                "nested/traits.rs",
                Phase0FileOperation::Modify,
                Some(b"old"),
                Some(b"new"),
                (0, 3),
                (0, 3),
                b"old",
                b"new",
            ),
        ],
    );
    let module_path = temp.path().join("module-ledger.json");
    fs::write(&module_path, canonical_json(&module_ledger).unwrap()).unwrap();
    let module_replay = temp.path().join("module-replay");
    let module = Command::new(env!("CARGO_BIN_EXE_trellis_trust_cli"))
        .arg("replay-phase0-adaptation")
        .arg(&module_raw)
        .arg(&module_path)
        .arg(&targets_path)
        .arg(&module_adapted)
        .arg(&module_replay)
        .status()
        .unwrap();
    assert!(module.success());
    assert_eq!(
        source_tree_manifest(&module_replay).unwrap(),
        source_tree_manifest(&module_adapted).unwrap()
    );
}

proptest! {
    #[test]
    fn arbitrary_single_byte_patch_replays_exactly(
        prefix in proptest::collection::vec(any::<u8>(), 0..24),
        removed in proptest::collection::vec(any::<u8>(), 1..24),
        inserted in proptest::collection::vec(any::<u8>(), 0..24),
        suffix in proptest::collection::vec(any::<u8>(), 0..24),
    ) {
        prop_assume!(removed != inserted);
        let mut before = prefix.clone();
        before.extend_from_slice(&removed);
        before.extend_from_slice(&suffix);
        let mut after = prefix.clone();
        after.extend_from_slice(&inserted);
        after.extend_from_slice(&suffix);
        let temp = tempfile::tempdir().unwrap();
        let raw = temp.path().join("raw");
        let adapted = temp.path().join("adapted");
        write_tree(&raw, &[("bytes.bin", &before)]);
        write_tree(&adapted, &[("bytes.bin", &after)]);
        let row = entry(
            "bytes.bin",
            Phase0FileOperation::Modify,
            Some(&before),
            Some(&after),
            (prefix.len() as u64, (prefix.len() + removed.len()) as u64),
            (prefix.len() as u64, (prefix.len() + inserted.len()) as u64),
            &removed,
            &inserted,
        );
        let ledger = ledger(12, &raw, &adapted, vec![row]);
        let replay = temp.path().join("replay");
        replay_phase0_adaptation(&raw, &ledger, &targets(), &adapted, &replay).unwrap();
        prop_assert_eq!(fs::read(replay.join("bytes.bin")).unwrap(), after);
    }
}
