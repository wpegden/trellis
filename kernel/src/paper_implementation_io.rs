//! File-backed admission for paper implementation subjects. Identity and scope
//! checks only: ordinary Lean closure and the existing semantic lanes remain
//! authoritative. No assumption admission or extra human gate is created here.
use crate::model::{ProtocolState, TargetId};
use crate::paper_implementation::{PaperImplementation, PaperProofLibrary, PaperProofSupport};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

fn digest(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }
fn error(detail: impl std::fmt::Display) -> String {
    format!("paper implementation admission: {detail}")
}
fn object<'a>(v: &'a Value, label: &str) -> Result<&'a serde_json::Map<String, Value>, String> {
    v.as_object().ok_or_else(|| error(format!("{label} must be an object")))
}
fn string<'a>(v: &'a Value, field: &str) -> Result<&'a str, String> {
    v.get(field).and_then(Value::as_str).filter(|s| !s.is_empty() && s.trim() == *s)
        .ok_or_else(|| error(format!("{field} must be a nonempty canonical string")))
}
fn array<'a>(v: &'a Value, field: &str) -> Result<&'a Vec<Value>, String> {
    v.get(field).and_then(Value::as_array).ok_or_else(|| error(format!("{field} must be an array")))
}
fn path_set(v: &Value, field: &str, optional: bool) -> Result<BTreeSet<String>, String> {
    if optional && v.get(field).is_none() { return Ok(BTreeSet::new()); }
    let mut result = BTreeSet::new();
    for p in array(v, field)? {
        let p = p.as_str().filter(|s| !s.is_empty()).ok_or_else(|| error(format!("{field} contains a non-path")))?;
        if !result.insert(p.to_string()) { return Err(error(format!("duplicate {field} path: {p}"))); }
    }
    Ok(result)
}
fn known_keys(v: &Value, allowed: &[&str], label: &str) -> Result<(), String> {
    for key in object(v, label)?.keys() {
        if !allowed.contains(&key.as_str()) { return Err(error(format!("unsupported {label} field: {key}"))); }
    }
    Ok(())
}
/// Refuse symlinks at every component, traversal, aliases and out-of-tree paths.
/// Files are checked again before acceptance; a digest is never an approval.
pub fn subject_file(repo: &Path, rel: &str) -> Result<PathBuf, String> {
    let path = Path::new(rel);
    if rel.is_empty() || rel.contains('\\') || rel.split('/').any(|p| p.is_empty() || p == "." || p == "..")
        || path.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(error(format!("subject path must be canonical and repo-relative: {rel}")));
    }
    let mut current = repo.to_path_buf();
    for component in path.components() {
        current.push(component);
        let meta = fs::symlink_metadata(&current).map_err(|e| error(format!("{}: {e}", current.display())))?;
        if meta.file_type().is_symlink() { return Err(error(format!("symlink subject path: {rel}"))); }
    }
    if !current.is_file() { return Err(error(format!("subject is not a regular file: {rel}"))); }
    Ok(current)
}
fn read_json(path: &Path) -> Result<Value, String> {
    serde_json::from_slice(&fs::read(path).map_err(error)?).map_err(error)
}
fn check_no_new_assumptions(repo: &Path, files: &serde_json::Map<String, Value>) -> Result<(), String> {
    if crate::assumptions_registry::has_worker_authored_staged_assumption(repo)?
        || !crate::assumptions_registry::lane_passed_pending_axioms(repo)?.is_empty() {
        return Err(error("v1 excludes staged/pending under-model assumptions; use the existing unchanged trust basis"));
    }
    // Policy files cannot be created after intake or changed without changing
    // the subject. This is stricter scope, not a new allowlist mechanism.
    for name in ["APPROVED_AXIOMS.json", "tcb_manifest.json"] {
        if repo.join(name).exists() && !files.contains_key(name) {
            return Err(error(format!("existing trust policy must be pinned in subject files: {name}")));
        }
    }
    Ok(())
}
fn semantic_config(config: &Value) -> Value {
    json!({"paper_implementation": config.get("paper_implementation"),
        "workflow": {"paper_tex_path": config.pointer("/workflow/paper_tex_path"),
            "main_result_targets": config.pointer("/workflow/main_result_targets"),
            "main_result_labels": config.pointer("/workflow/main_result_labels"),
            "main_result_envs": config.pointer("/workflow/main_result_envs"),
            "default_target": config.pointer("/workflow/default_target"),
            "approved_axioms_path": config.pointer("/workflow/approved_axioms_path"),
            "allowed_import_prefixes": config.pointer("/workflow/allowed_import_prefixes"),
            "forbidden_keyword_allowlist": config.pointer("/workflow/forbidden_keyword_allowlist")},
        "pv_tablet": config.get("pv_tablet"), "modeling": config.get("modeling"),
        "trust_base": config.get("trust_base")})
}

const PROOF_INVENTORY: &str = "paper-implementation/proof-dependencies.json";
const MATHLIB_LOCK: &str = "paper-implementation/mathlib-lake-manifest.json";
const NATIVE_LAKEFILE: &str = "paper-implementation/native/lakefile.toml";
const NATIVE_LOCK: &str = "paper-implementation/native/lake-manifest.json";
const MATHLIB_URL: &str = "https://github.com/leanprover-community/mathlib4.git";

fn empty_lean_lock() -> Value {
    json!({"version":"1.2.0", "packagesDir":".lake/packages", "packages":[],
        "name":"paper_implementation", "lakeDir":".lake", "fixedToolchain":false})
}

fn full_git_revision(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn approved_proof_package(name: &str, url: &str) -> bool {
    let upstream = match name {
        "mathlib" => "leanprover-community/mathlib4",
        "batteries" => "leanprover-community/batteries",
        "Qq" => "leanprover-community/quote4",
        "aesop" => "leanprover-community/aesop",
        "proofwidgets" => "leanprover-community/ProofWidgets4",
        "importGraph" => "leanprover-community/import-graph",
        "LeanSearchClient" => "leanprover-community/LeanSearchClient",
        "plausible" => "leanprover-community/plausible",
        "Cli" => "leanprover/lean4-cli",
        _ => return false,
    };
    url == if name == "mathlib" { MATHLIB_URL.to_string() } else { format!("https://github.com/{upstream}") }
}

fn validate_proof_package(package: &Value) -> Result<String, String> {
    known_keys(package, &["name", "url", "type", "subDir", "scope", "rev", "manifestFile", "inputRev", "inherited", "configFile"], "proof package")?;
    let name = string(package, "name")?;
    if package["type"] != "git" || !approved_proof_package(name, string(package, "url")?)
        || !full_git_revision(string(package, "rev")?) || !package["subDir"].is_null()
        || package["manifestFile"] != "lake-manifest.json"
        || !["lakefile.lean", "lakefile.toml"].contains(&string(package, "configFile")?)
        || !package["inherited"].is_boolean() || !package["scope"].is_string() || !package["inputRev"].is_string() {
        return Err(error(format!("unsupported or unpinned Mathlib proof package: {name}")));
    }
    Ok(name.to_owned())
}

/// Source pins supplement the existing trusted, host-prewarmed package/cache
/// boundary. They do not authenticate arbitrary cache executables or admit
/// library axioms. The ordinary checker still owns import and axiom policy.
fn proof_source_inventory(repo: &Path, package: &str) -> Result<BTreeMap<String, String>, String> {
    let base = repo.join(".lake/packages").join(package);
    let mut pending = vec![base.clone()];
    let mut inventory = BTreeMap::new();
    while let Some(directory) = pending.pop() {
        if fs::symlink_metadata(&directory).map_err(error)?.file_type().is_symlink() {
            return Err(error("proof package source directories may not be aliases"));
        }
        for entry in fs::read_dir(&directory).map_err(error)? {
            let entry = entry.map_err(error)?;
            let name = entry.file_name().into_string().map_err(|_| error("non-UTF8 proof source path"))?;
            if name == ".git" || name == ".lake" { continue; }
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(error)?;
            if metadata.is_dir() { pending.push(path); continue; }
            let included = name.ends_with(".lean") || ["lean-toolchain", "lake-manifest.json", "lakefile.toml"].contains(&name.as_str());
            if metadata.file_type().is_symlink() {
                if included || path.is_dir() { return Err(error("proof package source may not be aliased")); }
                continue;
            }
            if included {
                let relative = path.strip_prefix(&base).map_err(error)?.to_str().ok_or_else(|| error("non-UTF8 proof source path"))?.to_owned();
                let bound = subject_file(repo, &format!(".lake/packages/{package}/{relative}"))?;
                inventory.insert(relative, digest(&fs::read(bound).map_err(error)?));
            }
        }
    }
    Ok(inventory)
}

fn load_proof_support(repo: &Path, manifest: &Value, build_files: &BTreeSet<String>) -> Result<Option<PaperProofSupport>, String> {
    let Some(profile) = manifest.get("proof_project") else { return Ok(None); };
    if manifest["route"] != "lean_native" { return Err(error("optional Mathlib proof project is supported only for Lean-native")); }
    known_keys(profile, &["kind", "mathlib_rev", "dependency_inventory"], "proof_project")?;
    if profile["kind"] != "mathlib" || profile["dependency_inventory"] != PROOF_INVENTORY
        || !full_git_revision(string(profile, "mathlib_rev")?) {
        return Err(error("unsupported Mathlib proof project identity"));
    }
    for name in [PROOF_INVENTORY, MATHLIB_LOCK, NATIVE_LAKEFILE, NATIVE_LOCK] {
        if !build_files.contains(name) { return Err(error(format!("proof/native assembly input is not pinned: {name}"))); }
    }
    if fs::symlink_metadata(repo.join(".lake/package-overrides.json")).is_ok() { return Err(error("Mathlib proof package overrides are unsupported")); }
    let inventory_bytes = fs::read(subject_file(repo, PROOF_INVENTORY)?).map_err(error)?;
    let inventory: Value = serde_json::from_slice(&inventory_bytes).map_err(error)?;
    known_keys(&inventory, &["schema", "toolchain", "packages"], "proof source inventory")?;
    let toolchain = fs::read_to_string(subject_file(repo, "lean-toolchain")?).map_err(error)?;
    if inventory["schema"] != "trellis-mathlib-proof-dependencies/v1"
        || inventory["toolchain"].as_str() != Some(toolchain.trim()) {
        return Err(error("proof inventory does not bind the shared Lean toolchain"));
    }
    let upstream_lock = read_json(&subject_file(repo, MATHLIB_LOCK)?)?;
    known_keys(&upstream_lock, &["version", "packagesDir", "packages", "name", "lakeDir", "fixedToolchain"], "Mathlib lock")?;
    if upstream_lock["packagesDir"] != ".lake/packages" || upstream_lock["name"] != "mathlib"
        || upstream_lock["version"] != "1.2.0" || upstream_lock["lakeDir"] != ".lake"
        || upstream_lock["fixedToolchain"] != false {
        return Err(error("unsupported upstream Mathlib dependency closure"));
    }
    let rev = string(profile, "mathlib_rev")?;
    let mathlib = json!({"url":MATHLIB_URL,"type":"git","subDir":null,"scope":"","rev":rev,"name":"mathlib",
        "manifestFile":"lake-manifest.json","inputRev":rev,"inherited":false,"configFile":"lakefile.lean"});
    let mut dependencies = BTreeMap::new();
    for package in array(&upstream_lock, "packages")? {
        let name = validate_proof_package(package)?;
        if name == "mathlib" || dependencies.contains_key(&name) { return Err(error("duplicate/self Mathlib dependency")); }
        let mut inherited = package.clone(); inherited["inherited"] = json!(true);
        dependencies.insert(name, inherited);
    }
    let mut packages = vec![mathlib];
    packages.extend(dependencies.values().cloned());
    let mut expected_lock = empty_lean_lock(); expected_lock["packages"] = json!(packages);
    let root_lock_bytes = fs::read(subject_file(repo, "lake-manifest.json")?).map_err(error)?;
    if serde_json::from_slice::<Value>(&root_lock_bytes).map_err(error)? != expected_lock {
        return Err(error("proof lock must be exactly Mathlib and its pinned upstream dependency closure"));
    }
    let pinned_packages = object(&inventory["packages"], "proof package source inventory")?;
    let expected_names: BTreeSet<_> = packages.iter().map(|p| p["name"].as_str().unwrap().to_string()).collect();
    if pinned_packages.keys().cloned().collect::<BTreeSet<_>>() != expected_names { return Err(error("proof source inventory differs from the pinned lock closure")); }
    let installed_names = fs::read_dir(repo.join(".lake/packages")).map_err(error)?
        .map(|entry| entry.map_err(error).and_then(|entry| entry.file_name().into_string()
            .map_err(|_| error("non-UTF8 package name")))).collect::<Result<BTreeSet<_>,_>>()?;
    if installed_names != expected_names { return Err(error("proof project contains an unselected dependency package")); }
    for package in &packages {
        let name = package["name"].as_str().unwrap();
        let record = &pinned_packages[name];
        known_keys(record, &["url", "rev", "source_files"], "proof package source record")?;
        if record["url"] != package["url"] || record["rev"] != package["rev"] { return Err(error("proof source record differs from lock revision")); }
        let actual = proof_source_inventory(repo, name)?;
        if actual.is_empty() || serde_json::to_value(actual).map_err(error)? != record["source_files"] {
            return Err(error(format!("pinned proof package sources changed: {name}")));
        }
    }
    if fs::read(subject_file(repo, ".lake/packages/mathlib/lake-manifest.json")?).map_err(error)?
        != fs::read(subject_file(repo, MATHLIB_LOCK)?).map_err(error)?
        || fs::read_to_string(subject_file(repo, ".lake/packages/mathlib/lean-toolchain")?).map_err(error)?.trim() != toolchain.trim() {
        return Err(error("Mathlib source/toolchain differs from its frozen upstream evidence"));
    }
    if read_json(&subject_file(repo, NATIVE_LOCK)?)? != empty_lean_lock() {
        return Err(error("executable assembly must retain its canonical empty dependency lock"));
    }
    Ok(Some(PaperProofSupport {kind:PaperProofLibrary::Mathlib, mathlib_rev:rev.to_string(),
        dependency_inventory_sha256:digest(&inventory_bytes), lake_manifest_sha256:digest(&root_lock_bytes)}))
}

/// Load a completely pinned native subject, or return None for ordinary math/PV.
/// No GOAL file is generated or read. A fresh preparation generation is needed
/// for changed source/build inputs; live runs never silently rebaseline them.
pub fn load_subject(repo: &Path, config_path: &Path) -> Result<Option<PaperImplementation>, String> {
    let config = read_json(config_path)?;
    let Some(block) = config.get("paper_implementation") else { return Ok(None); };
    known_keys(block, &["version", "route", "manifest"], "paper_implementation")?;
    if block["version"].as_u64() != Some(1) { return Err(error("unsupported paper implementation version")); }
    let route = string(block, "route")?;
    if !["rust_aeneas", "lean_native"].contains(&route) { return Err(error("unknown implementation route")); }
    if config.get("goal_file").is_some() { return Err(error("paper mode has no prose GOAL referent")); }
    if config.pointer("/workflow/default_target").and_then(Value::as_str).is_some_and(|v| v != "lean") {
        return Err(error("paper implementation v1 requires the existing Lean backend"));
    }
    if config.pointer("/workflow/forbidden_keyword_allowlist").and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
        return Err(error("paper implementation cannot relax the ordinary keyword policy"));
    }
    if config.pointer("/workflow/challenge_targets_path").is_some_and(|v| !v.is_null()) {
        return Err(error("paper implementation targets must come from the original paper"));
    }
    // Custom import policy is not an escape hatch for executable source.
    if let Some(imports) = config.pointer("/workflow/allowed_import_prefixes") {
        if imports != &json!(["Mathlib"]) { return Err(error("paper mode cannot widen the ordinary import policy")); }
    }
    if route == "lean_native" && config.pointer("/modeling/enabled").and_then(Value::as_bool) == Some(true) {
        return Err(error("Lean-native cannot enter the Rust Modeling route"));
    }
    if route == "lean_native" && config.get("pv_tablet").is_some() {
        return Err(error("Lean-native must not acquire Rust extraction roles"));
    }
    if route == "lean_native" && config.get("trust_base").is_some_and(|value| !value.is_null()) {
        return Err(error("Lean-native does not support the PV-only required_v1 trust profile"));
    }
    if route == "rust_aeneas" {
        let pv = config.get("pv_tablet").ok_or_else(|| error("native Rust intake requires successful Aeneas extraction"))?;
        if pv.get("verification_targets").and_then(Value::as_array).is_some_and(|v| !v.is_empty())
            || pv.get("goal_mode").is_some() || pv.get("extraction_not_recorded").is_some_and(|v| !v.as_array().is_some_and(|a| a.is_empty())) {
            return Err(error("paper Rust intake cannot seed prose targets or bypass recorded extraction"));
        }
        if pv.get("extraction_models").and_then(Value::as_array).is_none_or(|v| v.is_empty()) {
            return Err(error("Rust paper mode requires actual extracted implementation models"));
        }
        if config.pointer("/modeling/enabled").and_then(Value::as_bool) != Some(true)
            || config.pointer("/modeling/preservation").and_then(Value::as_bool) != Some(true) {
            return Err(error("Rust paper mode requires existing Modeling preservation checks"));
        }
    }
    let manifest_path = string(block, "manifest")?;
    let bytes = fs::read(subject_file(repo, manifest_path)?).map_err(error)?;
    let manifest: Value = serde_json::from_slice(&bytes).map_err(error)?;
    known_keys(&manifest, &["schema", "route", "paper_path", "paper_files", "files", "targets", "entry_points",
        "implementation_roots", "modeling_choices", "source_files", "build_files", "evidence_files", "proof_project"], "manifest")?;
    if manifest["schema"] != "trellis-paper-implementation/v1" || manifest["route"] != route {
        return Err(error("manifest schema/route does not match config"));
    }
    let paper_path = string(&manifest, "paper_path")?;
    if config.pointer("/workflow/paper_tex_path").and_then(Value::as_str) != Some(paper_path) {
        return Err(error("workflow paper path must be the original manifest paper"));
    }
    let files = object(&manifest["files"], "files")?;
    let paper_files = path_set(&manifest, "paper_files", false)?;
    let source_files = path_set(&manifest, "source_files", false)?;
    let build_files = path_set(&manifest, "build_files", false)?;
    if !paper_files.contains(paper_path) || source_files.is_empty() || build_files.is_empty() {
        return Err(error("paper, source and build file inventories must identify the real subject"));
    }
    let mut classified: BTreeSet<String> = paper_files.union(&source_files).cloned().collect();
    classified.extend(build_files.clone());
    let evidence_files = path_set(&manifest, "evidence_files", true)?;
    classified.extend(evidence_files.clone());
    if paper_files.len() + source_files.len() + build_files.len() + evidence_files.len() != classified.len() {
        return Err(error("each subject file requires exactly one classification"));
    }
    for choice in array(&manifest, "modeling_choices")? {
        if !evidence_files.contains(string(choice, "evidence_path")?) { return Err(error("manual choice requires pinned evidence")); }
    }
    if classified != files.keys().cloned().collect() || files.contains_key(manifest_path) {
        return Err(error("files must exactly match classified subject inputs, excluding the manifest itself"));
    }
    for (rel, expected) in files {
        let expected = expected.as_str().ok_or_else(|| error(format!("invalid digest for {rel}")))?;
        if expected.len() != 64 || !expected.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
            return Err(error(format!("invalid SHA-256 for {rel}")));
        }
        if digest(&fs::read(subject_file(repo, rel)?).map_err(error)?) != expected {
            return Err(error(format!("pinned input changed: {rel}; restore it or prepare a new generation")));
        }
    }
    check_no_new_assumptions(repo, files)?;
    let proof_support = load_proof_support(repo, &manifest, &build_files)?;
    if route == "rust_aeneas" && repo.join("Tablet/model-support.json").exists() {
        let support = read_json(&subject_file(repo, "Tablet/model-support.json")?)?;
        let choices: BTreeSet<_> = array(&manifest, "modeling_choices")?.iter()
            .map(|c| string(c, "node").map(str::to_string)).collect::<Result<_,_>>()?;
        let mut support_nodes = path_set(&support, "imports", false)?;
        for binding in array(&support, "bindings")? { support_nodes.insert(string(binding, "node")?.to_string()); }
        if !support_nodes.is_subset(&choices) {
            return Err(error("actual model-support bindings/imports contain unreviewed, uninventoried modeling choices"));
        }
    }
    let paper_inventory: BTreeMap<&String, &Value> = paper_files.iter().map(|p| (p, &files[p])).collect();
    let paper_sha256 = digest(&serde_json::to_vec(&paper_inventory).map_err(error)?);
    let mut targets = BTreeSet::new();
    let mut excluded = BTreeMap::<String, Vec<String>>::new();
    for target in array(&manifest, "targets")? {
        known_keys(target, &["id", "source_path", "start_line", "end_line", "selected_text", "excluded_clauses", "kind"], "paper target")?;
        if target.get("kind").and_then(Value::as_str).is_some_and(|v| v != "semantic") { return Err(error("cost/running-time targets are outside v1")); }
        let id = string(target, "id")?;
        if !targets.insert(id.to_string()) { return Err(error(format!("duplicate paper target: {id}"))); }
        let source = string(target, "source_path")?;
        if !paper_files.contains(source) { return Err(error("target evidence must belong to the original paper bundle")); }
        let start = target["start_line"].as_u64().filter(|n| *n > 0).ok_or_else(|| error("invalid target start_line"))? as usize;
        let end = target["end_line"].as_u64().filter(|n| *n >= start as u64).ok_or_else(|| error("invalid target end_line"))? as usize;
        let source_text = fs::read_to_string(subject_file(repo, source)?).map_err(error)?;
        let lines: Vec<_> = source_text.lines().collect();
        if end > lines.len() { return Err(error("paper evidence range is out of bounds")); }
        let excerpt = lines[start - 1..end].join("\n");
        if !excerpt.contains(string(target, "selected_text")?) { return Err(error("selected clause is not verbatim original paper evidence")); }
        let mut omissions = Vec::new();
        for clause in array(target, "excluded_clauses")? {
            known_keys(clause, &["text", "reason"], "excluded clause")?;
            let text = string(clause, "text")?;
            if !excerpt.contains(text) || string(clause, "reason")?.is_empty() {
                return Err(error("excluded clause lacks original paper evidence and rationale"));
            }
            omissions.push(format!("{} — {}", text, string(clause, "reason")?));
        }
        excluded.insert(id.to_string(), omissions);
    }
    if targets.is_empty() { return Err(error("at least one semantic paper target is required")); }
    let configured = config.pointer("/workflow/main_result_targets").and_then(Value::as_array)
        .ok_or_else(|| error("resolved original paper targets are required in workflow.main_result_targets"))?;
    let mut configured_ids = BTreeSet::new();
    for target in configured {
        let start = target["start_line"].as_u64().filter(|n| *n > 0).ok_or_else(|| error("configured paper target has invalid range"))?;
        let end = target["end_line"].as_u64().filter(|n| *n >= start).ok_or_else(|| error("configured paper target has invalid range"))?;
        let id = target.get("tex_label").and_then(Value::as_str).filter(|s| !s.is_empty())
            .map(str::to_string).unwrap_or_else(|| format!("lines:{start}-{end}"));
        if !configured_ids.insert(id.clone()) { return Err(error("duplicate configured paper target")); }
        let selected = array(&manifest, "targets")?.iter().find(|t| t["id"].as_str() == Some(&id))
            .ok_or_else(|| error("configured paper target is absent from manifest"))?;
        if selected["source_path"].as_str() != Some(paper_path)
            || selected["start_line"].as_u64() != Some(start) || selected["end_line"].as_u64() != Some(end) {
            return Err(error("selected clause locator differs from its configured original paper target"));
        }
    }
    if targets != configured_ids { return Err(error("manifest target IDs differ from the configured original paper targets")); }
    if route == "lean_native" {
        for name in ["lakefile.lean", "Main.lean", "Mathlib.lean", "Mathlib", "Init", "Lean", "Lake", "Aeneas"] {
            if repo.join(name).exists() { return Err(error(format!("unsupported Lean project runtime/build override: {name}"))); }
        }
        if !build_files.contains("lakefile.toml") || !build_files.contains("lean-toolchain") || !build_files.contains("lake-manifest.json") {
            return Err(error("Lean executable assembly and toolchain must be pinned"));
        }
        if proof_support.is_none() && read_json(&subject_file(repo, "lake-manifest.json")?)? != empty_lean_lock() {
            return Err(error("Lean-native requires the canonical empty package resolution"));
        }
        for name in &source_files {
            if !name.starts_with("Tablet/") || !name.ends_with(".lean") { return Err(error("Lean project code must use ordinary Tablet definition files")); }
            let text = fs::read_to_string(subject_file(repo, name)?).map_err(error)?;
            for token in crate::backend::LEAN_FORBIDDEN_KEYWORDS.iter().chain(["csimp", "compiler_extern", "export", "__dir__", "__file__", "__line__"].iter()) {
                if text.split(|c: char| !c.is_alphanumeric() && c != '_').any(|part| part == *token) {
                    return Err(error(format!("unsupported Lean-native source token: {token}")));
                }
            }
            // Imports must have conventional syntax and name an inventoried
            // project body. The actual ordinary checker still owns elaboration.
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("import") && !trimmed.starts_with("import ") {
                    return Err(error("noncanonical Lean implementation import"));
                }
                if let Some(modules) = trimmed.strip_prefix("import ") {
                    for module in modules.split_whitespace() {
                        let dependency = format!("{}.lean", module.replace('.', "/"));
                        if !source_files.contains(&dependency) { return Err(error(format!("unbound Lean implementation import: {module}"))); }
                    }
                }
            }
        }
    }
    let mut entries = BTreeSet::new();
    for entry in array(&manifest, "entry_points")? {
        known_keys(entry, &["node_id", "declaration", "source_path"], "entry point")?;
        let node = string(entry, "node_id")?;
        let source = string(entry, "source_path")?;
        let declaration = string(entry, "declaration")?;
        if !entries.insert(node.to_string()) || !source_files.contains(source) { return Err(error("entry point must bind a unique node to inventoried source")); }
        if route == "rust_aeneas" {
            let models = config.pointer("/pv_tablet/extraction_models").and_then(Value::as_array).ok_or_else(|| error("missing extracted models"))?;
            let matches: Vec<_> = models.iter().filter(|model| model.get("node").or_else(|| model.get("name")).and_then(Value::as_str) == Some(node)).collect();
            if matches.len() != 1 { return Err(error("Rust implementation root must be an actual unique ExtractionModel")); }
            let model = matches[0];
            let source_file = model.pointer("/provenance/source_file").and_then(Value::as_str).ok_or_else(|| error("extracted root has no source locator"))?;
            if source != format!("crate/{source_file}") { return Err(error("Rust entry source does not match the recorded extracted crate locator")); }
            let source_digest = crate::trust_base::source_tree_digest(&repo.join("crate")).map_err(error)?.to_string();
            if model.pointer("/provenance/source_sha256").and_then(Value::as_str) != Some(source_digest.as_str()) {
                return Err(error("Rust extraction provenance does not bind the current complete crate tree"));
            }
            let text = fs::read_to_string(subject_file(repo, &format!("Tablet/{node}.lean"))?).map_err(error)?;
            if crate::filespec::exact_principal_name(&text, node)? != declaration { return Err(error("Rust entry declaration does not match its extracted principal")); }
            let mut spec: crate::ChallengeTargetSpec = serde_json::from_value(model.clone()).map_err(error)?;
            spec.kind = crate::ChallengeTargetKind::Def;
            let target = crate::ChallengeTargetId::from(string(model, "id")?);
            let binding_errors = crate::worker_normalization::bound_challenge_content_errors(&crate::NodeId::from(node), &target, &spec, &text);
            if !binding_errors.is_empty() { return Err(error(binding_errors.join("; "))); }
        }
        if route == "lean_native" {
            if source != format!("Tablet/{node}.lean") { return Err(error("Lean-native entry source must be the actual ordinary Tablet definition")); }
            let text = fs::read_to_string(subject_file(repo, source)?).map_err(error)?;
            let actual = crate::filespec::exact_principal_name(&text, node)?;
            if actual != declaration { return Err(error(format!("entry point {node} declaration mismatch"))); }
            for message in crate::filespec::validate_lean_node_shape(&text, node) { return Err(error(message)); }
            // A scope screen, not a substitute for ordinary checks. Deliberately
            // conservative even in comments, so it cannot authorize escapes.
            for token in crate::backend::LEAN_FORBIDDEN_KEYWORDS.iter().chain(["csimp"].iter()) {
                if text.split(|c: char| !c.is_alphanumeric() && c != '_').any(|part| part == *token) {
                    return Err(error(format!("unsupported Lean-native source token: {token}")));
                }
            }
        }
    }
    if route == "lean_native" {
        let roots = entries.iter().map(|n| serde_json::to_string(&format!("Tablet.{n}")).unwrap()).collect::<Vec<_>>().join(", ");
        let expected = format!("name = \"paper_implementation\"\nversion = \"0.1.0\"\ndefaultTargets = [\"Tablet\"]\n\n[[lean_lib]]\nname = \"Tablet\"\nroots = [{roots}]\nglobs = [\"Tablet.+\"]\n");
        let proof_lakefile = if let Some(proof) = &proof_support {
            if fs::read_to_string(subject_file(repo, NATIVE_LAKEFILE)?).map_err(error)? != expected {
                return Err(error("native executable assembly differs from the exact dependency-free implementation assembly"));
            }
            format!("{expected}\n[[require]]\nname = \"mathlib\"\ngit = \"{MATHLIB_URL}\"\nrev = \"{}\"\n", proof.mathlib_rev)
        } else { expected };
        if fs::read_to_string(subject_file(repo, "lakefile.toml")?).map_err(error)? != proof_lakefile {
            return Err(error("Lean implementation requires the exact supported Lake assembly"));
        }
    }
    let roots = object(&manifest["implementation_roots"], "implementation_roots")?;
    if roots.keys().cloned().collect::<BTreeSet<_>>() != targets { return Err(error("each selected paper target needs implementation roots")); }
    for (id, nodes) in roots {
        let nodes = nodes.as_array().filter(|a| !a.is_empty()).ok_or_else(|| error(format!("empty implementation roots: {id}")))?;
        let mut seen = BTreeSet::new();
        for node in nodes {
            let node = node.as_str().ok_or_else(|| error("implementation root is not a node ID"))?;
            if !entries.contains(node) || !seen.insert(node) { return Err(error("implementation roots must identify actual unique entry points")); }
        }
    }
    let value = json!({"version": 1, "route": route, "manifest_path": manifest_path,
        "subject_sha256": digest(&bytes), "configuration_sha256": digest(&serde_json::to_vec(&semantic_config(&config)).map_err(error)?),
        "paper_path": paper_path, "paper_sha256": paper_sha256,
        "proof_support": proof_support,
        "implementation_roots": manifest["implementation_roots"], "modeling_choices": manifest["modeling_choices"],
        "excluded_complexity_clauses": excluded});
    let subject: PaperImplementation = serde_json::from_value(value).map_err(error)?;
    crate::paper_implementation::validate_subject(&subject)?;
    Ok(Some(subject))
}

pub fn verify_subject(repo: &Path, config_path: &Path, state: &ProtocolState) -> Result<(), String> {
    let current = load_subject(repo, config_path)?;
    if current != state.paper_implementation {
        return Err(error("mode, paper, implementation or build configuration changed after intake; prepare a new generation instead of inheriting prior evidence"));
    }
    if let Some(subject) = current {
        if state.configured_targets != subject.implementation_roots.keys().cloned().collect::<BTreeSet<TargetId>>() {
            return Err(error("live paper target registry differs from the pinned subject"));
        }
        if subject.route == crate::paper_implementation::PaperImplementationRoute::LeanNative {
            for node in subject.implementation_roots.values().flatten() {
                if state.node_role.contains_key(node) {
                    return Err(error(format!("Lean-native root {} must use ordinary checking", node.as_str())));
                }
            }
        }
    }
    Ok(())
}

/// Source-facing bodies shown at the gate, including absence of Preamble.
/// Ordinary proof-node bodies remain editable under existing protection.
fn modeling_files(repo: &Path, state: &ProtocolState) -> Result<Value, String> {
    let Some(subject) = &state.paper_implementation else { return Ok(Value::Null); };
    let mut nodes = state.node_kinds.iter().filter(|(_, kind)| matches!(kind,
        crate::NodeKind::Definition | crate::NodeKind::Preamble))
        .map(|(node, _)| node.clone()).collect::<BTreeSet<_>>();
    nodes.extend(subject.modeling_choices.iter().map(|choice| choice.node.clone()));
    nodes.insert(crate::NodeId::from("Preamble"));
    let mut files = BTreeMap::new();
    for node in nodes {
        for extension in ["lean", "tex"] {
            let name = format!("Tablet/{}.{extension}", node.as_str());
            if !repo.join(&name).exists() && (node.as_str() == "Preamble" || (extension == "tex" && state.pv_review_suppressed(&node))) {
                files.insert(name, Value::Null);
                continue;
            }
            let bytes = fs::read(subject_file(repo, &name)?).map_err(error)?;
            files.insert(name, json!({"sha256":digest(&bytes),"utf8":String::from_utf8(bytes).map_err(error)?}));
        }
    }
    // Support bindings choose which handwritten definition supplies each raw
    // extracted external. Changing that map changes source/model meaning even
    // when all definition bodies are unchanged.
    let name = "Tablet/model-support.json";
    let binding = if repo.join(name).exists() {
        let bytes = fs::read(subject_file(repo, name)?).map_err(error)?;
        json!({"sha256":digest(&bytes),"utf8":String::from_utf8(bytes).map_err(error)?})
    } else { Value::Null };
    files.insert(name.to_string(), binding);
    Ok(serde_json::to_value(files).map_err(error)?)
}

pub const NATIVE_BUILD_REPORT: &str = ".trellis/scratch/paper-native-build-report.json";
fn native_build_evidence(repo: &Path, state: &ProtocolState) -> Result<Value, String> {
    let subject = state.paper_implementation.as_ref().ok_or_else(|| error("missing subject"))?;
    if subject.route != crate::paper_implementation::PaperImplementationRoute::LeanNative { return Ok(Value::Null); }
    let report = read_json(&subject_file(repo, NATIVE_BUILD_REPORT)?)?;
    if report["schema"] != "trellis-paper-native-build/v1" || report["subject_sha256"] != subject.subject_sha256
        || report["paper_sha256"] != subject.paper_sha256 {
        return Err(error("native build report is not bound to the current subject"));
    }
    if let Some(proof) = &subject.proof_support {
        let manifest = read_json(&subject_file(repo, &subject.manifest_path)?)?;
        let sources: BTreeMap<_,_> = path_set(&manifest, "source_files", false)?.into_iter()
            .map(|name| { let hash=manifest["files"][&name].clone(); (name,hash) }).collect();
        let native_inputs = json!({
            "lakefile.toml":{"source_path":NATIVE_LAKEFILE,"sha256":manifest["files"][NATIVE_LAKEFILE]},
            "lake-manifest.json":{"source_path":NATIVE_LOCK,"sha256":manifest["files"][NATIVE_LOCK]},
            "lean-toolchain":{"source_path":"lean-toolchain","sha256":manifest["files"]["lean-toolchain"]}
        });
        if report["proof_support"] != serde_json::to_value(proof).map_err(error)?
            || report["source_files"] != serde_json::to_value(sources).map_err(error)?
            || report["native_build_inputs"] != native_inputs {
            return Err(error("native build must bind the identical implementation sources and separate core-only assembly, not the Mathlib proof project"));
        }
    }
    let artifact = subject_file(repo, string(&report, "compiled_library")?)?;
    if digest(&fs::read(artifact).map_err(error)?) != string(&report, "compiled_library_sha256")? {
        return Err(error("native implementation artifact changed after compilation"));
    }
    Ok(report)
}

/// Verify exact human evidence and source bodies, without creating an approval.
pub fn verify_review_receipt(repo: &Path, state: &ProtocolState, sha: Option<&str>) -> Result<(), String> {
    if state.paper_implementation_approved_sha256.is_none() { return Ok(()); }
    let sha = sha.filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| error("paper approval has no exact archived human instrument"))?;
    let bytes = fs::read(subject_file(repo, &format!("paper-implementation/reviews/{sha}.json"))?).map_err(error)?;
    if digest(&bytes) != sha { return Err(error("archived paper human instrument changed")); }
    let instrument: Value = serde_json::from_slice(&bytes).map_err(error)?;
    let section = instrument["live_disclosure"].as_array().and_then(|rows| rows.iter()
        .find(|row| row["section"] == "paper_implementation")).ok_or_else(|| error("human instrument lacks paper evidence"))?;
    let pinned = object(&section["reviewed_modeling_files"], "reviewed modeling files")?;
    for (name, expected) in pinned {
        if expected.is_null() {
            if repo.join(name).exists() { return Err(error(format!("human-reviewed absence changed: {name}"))); }
        } else {
            let bytes = fs::read(subject_file(repo, name)?).map_err(error)?;
            if Some(digest(&bytes).as_str()) != expected["sha256"].as_str() {
                return Err(error(format!("human-reviewed modeling semantics changed: {name}; prepare a new generation")));
            }
        }
    }
    if section["native_build"] != native_build_evidence(repo, state)? {
        return Err(error("human-reviewed compiled implementation evidence changed"));
    }
    Ok(())
}

/// Readable evidence for the existing single Advance instrument. Includes the
/// original bytes and lane state; neither this projection nor its hash approves
/// a model or changes the selected logical policy.
pub fn review_section(repo: &Path, state: &ProtocolState) -> Result<Value, String> {
    let Some(subject) = &state.paper_implementation else { return Ok(Value::Null); };
    let raw = fs::read(subject_file(repo, &subject.manifest_path)?).map_err(error)?;
    if digest(&raw) != subject.subject_sha256 { return Err(error("manifest changed before human presentation")); }
    let manifest: Value = serde_json::from_slice(&raw).map_err(error)?;
    let mut contents = BTreeMap::new();
    for (name, sha) in object(&manifest["files"], "files")? {
        let bytes = fs::read(subject_file(repo, name)?).map_err(error)?;
        if Some(digest(&bytes).as_str()) != sha.as_str() { return Err(error(format!("human evidence changed: {name}"))); }
        contents.insert(name.clone(), json!({"sha256": sha, "bytes": bytes.len(), "utf8": String::from_utf8(bytes).ok()}));
    }
    Ok(json!({"section": "paper_implementation", "review": crate::paper_implementation::review_evidence(state),
        "manifest": manifest, "original_evidence": contents,
        "performance": match contents.get("paper-implementation/performance/review.json") {
            Some(evidence) => json!({"status":"preparation_report_attached", "review":evidence,
                "evidence_prefix":"paper-implementation/performance/",
                "authority":"Empirical observations and agent assessment; algorithm correspondence is reviewed separately."}),
            None => json!({"status":"not_evaluated_by_preparation_performance_role"}),
        },
        "reviewed_modeling_files": modeling_files(repo, state)?,
        "native_build": native_build_evidence(repo, state)?,
        "modeling_status": state.live.present_nodes.iter().filter(|node| state.preservation_owner(node)
            || subject.modeling_choices.iter().any(|choice| &choice.node == *node))
            .map(|node| (node.clone(), json!({"correspondence": format!("{:?}", state.current_corr_state(node)),
                "soundness": format!("{:?}", state.current_sound_state(node)), "preservation_required": state.preservation_owner(node)})))
            .collect::<BTreeMap<_,_>>(),
        "decision": "Review algorithm fidelity, Rust modeling exceptions and paper/formal correspondence."}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        fs::create_dir_all(repo.join("paper-implementation")).unwrap();
        fs::create_dir_all(repo.join("paper")).unwrap();
        fs::create_dir_all(repo.join("Tablet")).unwrap();
        fs::write(repo.join("paper/main.tex"), "\\begin{theorem}\\label{main}\nAlgorithm returns its input. It takes linear time.\n\\end{theorem}\n").unwrap();
        fs::write(repo.join("Tablet/Algorithm.lean"), "-- [TABLET NODE: Algorithm]\ndef Algorithm (n : Nat) : Nat :=\n-- BODY\nn\n").unwrap();
        fs::write(repo.join("lean-toolchain"), "leanprover/lean4:v4.30.0-rc1\n").unwrap();
        fs::write(repo.join("lakefile.toml"), "name = \"paper_implementation\"\nversion = \"0.1.0\"\ndefaultTargets = [\"Tablet\"]\n\n[[lean_lib]]\nname = \"Tablet\"\nroots = [\"Tablet.Algorithm\"]\nglobs = [\"Tablet.+\"]\n").unwrap();
        fs::write(repo.join("lake-manifest.json"), serde_json::to_vec(&json!({"version":"1.2.0", "packagesDir":".lake/packages", "packages":[],
            "name":"paper_implementation", "lakeDir":".lake", "fixedToolchain":false})).unwrap()).unwrap();
        let files: BTreeMap<_, _> = ["paper/main.tex", "Tablet/Algorithm.lean", "lean-toolchain", "lakefile.toml", "lake-manifest.json"].iter()
            .map(|name| (name.to_string(), digest(&fs::read(repo.join(name)).unwrap()))).collect();
        let manifest = json!({"schema":"trellis-paper-implementation/v1", "route":"lean_native", "paper_path":"paper/main.tex",
            "paper_files":["paper/main.tex"], "source_files":["Tablet/Algorithm.lean"], "build_files":["lean-toolchain","lakefile.toml","lake-manifest.json"], "evidence_files":[],
            "files":files, "targets":[{"id":"main","source_path":"paper/main.tex","start_line":1,"end_line":3,"selected_text":"Algorithm returns its input.",
                "excluded_clauses":[{"text":"It takes linear time.","reason":"complexity deferred beyond v1"}]}],
            "entry_points":[{"node_id":"Algorithm","declaration":"Algorithm","source_path":"Tablet/Algorithm.lean"}],
            "implementation_roots":{"main":["Algorithm"]},"modeling_choices":[]});
        fs::write(repo.join("paper-implementation/manifest.json"), serde_json::to_vec(&manifest).unwrap()).unwrap();
        let config = json!({"paper_implementation":{"version":1,"route":"lean_native","manifest":"paper-implementation/manifest.json"},
            "workflow":{"paper_tex_path":"paper/main.tex","main_result_targets":[{"tex_label":"main","start_line":1,"end_line":3}]}});
        let config_path = repo.join("trellis.config.json");
        fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
        (dir, config_path)
    }
    fn fixture_build(repo: &Path, state: &ProtocolState) {
        let subject = state.paper_implementation.as_ref().unwrap();
        fs::create_dir_all(repo.join(".trellis/scratch")).unwrap();
        fs::write(repo.join(".trellis/scratch/implementation.a"), b"fixture artifact").unwrap();
        fs::write(repo.join(NATIVE_BUILD_REPORT), serde_json::to_vec(&json!({
            "schema":"trellis-paper-native-build/v1", "subject_sha256":subject.subject_sha256,
            "paper_sha256":subject.paper_sha256, "compiled_library":".trellis/scratch/implementation.a",
            "compiled_library_sha256":digest(b"fixture artifact")
        })).unwrap()).unwrap();
    }
    fn archive_review(repo: &Path, state: &ProtocolState) -> String {
        let bytes = serde_json::to_vec(&json!({"live_disclosure":[review_section(repo, state).unwrap()]})).unwrap();
        let sha = digest(&bytes);
        fs::create_dir_all(repo.join("paper-implementation/reviews")).unwrap();
        fs::write(repo.join(format!("paper-implementation/reviews/{sha}.json")), bytes).unwrap();
        sha
    }
    #[test]
    fn paper_implementation_io_gate_requires_compiled_artifact() {
        let (dir, config) = fixture();
        let mut state = ProtocolState::default();
        state.paper_implementation = load_subject(dir.path(), &config).unwrap();
        assert!(review_section(dir.path(), &state).is_err());
        fixture_build(dir.path(), &state);
        review_section(dir.path(), &state).unwrap();
        fs::write(dir.path().join(".trellis/scratch/implementation.a"), "changed").unwrap();
        assert!(review_section(dir.path(), &state).unwrap_err().contains("artifact changed"));
    }
    #[test]
    fn paper_implementation_io_references_bind_paper_review_and_invalidation() {
        let (dir, config) = fixture();
        let repo = dir.path();
        let manifest_path = repo.join("paper-implementation/manifest.json");
        let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        let references = [
            ("paper/references/catalogue/table.csv", "id,weight\n1,2\n"),
            ("paper/references/check.py", "raise RuntimeError('supporting reference')\n"),
            ("paper/references/receipt.json", "{\"success\":true}\n"),
            ("paper/TRELLIS_REFERENCES.json", "{\"reference_note\":\"The finite bound requires complete enumeration, circuit propagation semantics and checked finite properties. Regeneration carries the same obligations. The general theorem is independent of these files. Scripts and receipts are reproducibility aids.\"}\n"),
        ];
        for (name, bytes) in &references {
            fs::create_dir_all(repo.join(name).parent().unwrap()).unwrap();
            fs::write(repo.join(name), bytes).unwrap();
            manifest["paper_files"].as_array_mut().unwrap().push(json!(name));
            manifest["files"][name] = json!(digest(bytes.as_bytes()));
        }
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let mut state = ProtocolState::default();
        state.paper_implementation = load_subject(repo, &config).unwrap();
        let original = state.paper_implementation.clone().unwrap();
        let paper_verifier_basis = crate::paper_implementation::observed_subject_basis(repo).unwrap().unwrap();
        assert_eq!(paper_verifier_basis, crate::paper_implementation::approval_basis(&original));
        fixture_build(repo, &state);
        let review = review_section(repo, &state).unwrap();
        assert!(review["review"]["human_review_topics"].as_array().unwrap().iter()
            .any(|topic| topic.as_str().unwrap().contains("computational evidence dependencies")));
        for (name, bytes) in &references {
            assert_eq!(review["original_evidence"][name]["utf8"], *bytes);
            assert!(!review["manifest"]["source_files"].as_array().unwrap().contains(&json!(name)));
        }
        assert!(state.paper_implementation_approved_sha256.is_none());
        let path = references[0].0;
        fs::write(repo.join(path), "changed catalogue").unwrap();
        assert!(load_subject(repo, &config).is_err());
        assert!(crate::paper_implementation::observed_subject_basis(repo).is_err());
        assert!(review_section(repo, &state).is_err());
        manifest["files"][path] = json!(digest(b"changed catalogue"));
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let changed = load_subject(repo, &config).unwrap().unwrap();
        assert_ne!(changed.paper_sha256, original.paper_sha256);
        assert_ne!(changed.subject_sha256, original.subject_sha256);
        assert_eq!(manifest["targets"], review["manifest"]["targets"]);
        assert_eq!(changed.implementation_roots, original.implementation_roots);
        assert_ne!(crate::paper_implementation::observed_subject_basis(repo).unwrap().unwrap(), paper_verifier_basis);
        assert_ne!(crate::paper_implementation::approval_basis(&changed), crate::paper_implementation::approval_basis(&original));
        assert!(review_section(repo, &state).is_err());
    }
    #[test]
    fn paper_implementation_io_discloses_empirical_evidence_without_inventing_review() {
        let (dir, config) = fixture();
        let repo = dir.path();
        let mut state = ProtocolState::default();
        state.paper_implementation = load_subject(repo, &config).unwrap();
        fixture_build(repo, &state);
        assert_eq!(review_section(repo, &state).unwrap()["performance"]["status"],
                   "not_evaluated_by_preparation_performance_role");
        let name = "paper-implementation/performance/review.json";
        fs::create_dir_all(repo.join("paper-implementation/performance")).unwrap();
        fs::write(repo.join(name), "{\"decision\":\"accept\",\"reason\":\"fixture agent assessment\"}").unwrap();
        let manifest = repo.join("paper-implementation/manifest.json");
        let mut value: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
        value["evidence_files"].as_array_mut().unwrap().push(json!(name));
        value["files"][name] = json!(digest(&fs::read(repo.join(name)).unwrap()));
        fs::write(manifest, serde_json::to_vec(&value).unwrap()).unwrap();
        state.paper_implementation = load_subject(repo, &config).unwrap();
        fixture_build(repo, &state);
        let section = review_section(repo, &state).unwrap();
        assert_eq!(section["performance"]["status"], "preparation_report_attached");
        assert!(section["original_evidence"][name]["utf8"].as_str().unwrap().contains("fixture agent assessment"));
        fs::write(repo.join(name), "changed assessment").unwrap();
        assert!(review_section(repo, &state).unwrap_err().contains("human evidence changed"));
    }
    #[test]
    fn paper_implementation_io_review_receipt_freezes_models_but_not_proofs() {
        let (dir, config) = fixture();
        let repo = dir.path();
        let mut state = ProtocolState::default();
        state.paper_implementation = load_subject(repo, &config).unwrap();
        state.node_kinds.insert("Manual".into(), crate::NodeKind::Definition);
        fs::write(repo.join("Tablet/Manual.lean"), "def Manual := 1").unwrap();
        fs::write(repo.join("Tablet/Manual.tex"), "The model is one.").unwrap();
        fixture_build(repo, &state);
        let sha = archive_review(repo, &state);
        state.paper_implementation_approved_sha256 = Some(crate::paper_implementation::approval_basis(state.paper_implementation.as_ref().unwrap()));
        verify_review_receipt(repo, &state, Some(&sha)).unwrap();
        state.configured_targets.insert("main".into());
        state.phase = crate::Phase::ProofFormalization;
        let paths = crate::runtime::RuntimePaths::new(repo.join("runtime"));
        crate::runtime::SupervisorRuntime::initialize_with_metadata(paths.clone(), state.clone(), crate::runtime::RuntimeMetadata {
            repo_path: Some(repo.to_path_buf()), config_path: Some(config.clone()),
            paper_implementation_review_sha256: Some(sha.clone()), ..Default::default()
        }).unwrap();
        let loaded = crate::runtime::SupervisorRuntime::load(paths.clone()).unwrap();
        assert_eq!(loaded.metadata().paper_implementation_review_sha256.as_deref(), Some(sha.as_str()));
        fs::write(repo.join("Tablet/Proof.lean"), "theorem Proof : True := by trivial").unwrap();
        verify_review_receipt(repo, &state, Some(&sha)).unwrap();
        fs::write(repo.join("Tablet/Manual.lean"), "def Manual := 2").unwrap();
        assert!(verify_review_receipt(repo, &state, Some(&sha)).unwrap_err().contains("modeling semantics changed"));
        assert!(crate::runtime::SupervisorRuntime::load(paths).is_err());
        fs::write(repo.join("Tablet/Manual.lean"), "def Manual := 1").unwrap();
        fs::write(repo.join("Tablet/Preamble.lean"), "-- new semantics").unwrap();
        assert!(verify_review_receipt(repo, &state, Some(&sha)).unwrap_err().contains("absence changed"));
        fs::remove_file(repo.join("Tablet/Preamble.lean")).unwrap();
        fs::write(repo.join("Tablet/model-support.json"), "{}").unwrap();
        assert!(verify_review_receipt(repo, &state, Some(&sha)).unwrap_err().contains("absence changed"));
        fs::remove_file(repo.join("Tablet/model-support.json")).unwrap();
        fs::write(repo.join(format!("paper-implementation/reviews/{sha}.json")), "{}").unwrap();
        assert!(verify_review_receipt(repo, &state, Some(&sha)).unwrap_err().contains("instrument changed"));
        assert!(verify_review_receipt(repo, &state, None).is_err());
    }
    fn repin(repo: &Path, name: &str, text: &str) {
        fs::write(repo.join(name), text).unwrap();
        let path = repo.join("paper-implementation/manifest.json");
        let mut m = read_json(&path).unwrap();
        m["files"][name] = json!(digest(text.as_bytes()));
        fs::write(path, serde_json::to_vec(&m).unwrap()).unwrap();
    }
    // Structural package fixture only: no claim of trusted setup/authenticity,
    // native compilation, or kernel proof is made by these small test files.
    fn mathlib_fixture() -> (tempfile::TempDir, PathBuf) {
        let (dir, config) = fixture();
        let repo = dir.path();
        fs::create_dir_all(repo.join("paper-implementation/native")).unwrap();
        fs::copy(repo.join("lakefile.toml"), repo.join(NATIVE_LAKEFILE)).unwrap();
        fs::copy(repo.join("lake-manifest.json"), repo.join(NATIVE_LOCK)).unwrap();
        let package = repo.join(".lake/packages/mathlib");
        fs::create_dir_all(package.join("Mathlib")).unwrap();
        fs::write(package.join("Mathlib/Fixture.lean"), "theorem fixture : True := by trivial\n").unwrap();
        fs::write(package.join("lakefile.lean"), "-- trusted fixture setup, not executed\n").unwrap();
        fs::copy(repo.join("lean-toolchain"), package.join("lean-toolchain")).unwrap();
        let mut upstream = empty_lean_lock(); upstream["name"] = json!("mathlib");
        let upstream_bytes = serde_json::to_vec(&upstream).unwrap();
        fs::write(repo.join(MATHLIB_LOCK), &upstream_bytes).unwrap();
        fs::write(package.join("lake-manifest.json"), &upstream_bytes).unwrap();
        let revision = "a".repeat(40);
        let mut lock = empty_lean_lock();
        lock["packages"] = json!([{"url":MATHLIB_URL,"type":"git","subDir":null,"scope":"","rev":revision,"name":"mathlib",
            "manifestFile":"lake-manifest.json","inputRev":revision,"inherited":false,"configFile":"lakefile.lean"}]);
        fs::write(repo.join("lake-manifest.json"), serde_json::to_vec(&lock).unwrap()).unwrap();
        let native_lakefile = fs::read_to_string(repo.join(NATIVE_LAKEFILE)).unwrap();
        fs::write(repo.join("lakefile.toml"), format!("{native_lakefile}\n[[require]]\nname = \"mathlib\"\ngit = \"{MATHLIB_URL}\"\nrev = \"{revision}\"\n")).unwrap();
        let inventory = json!({"schema":"trellis-mathlib-proof-dependencies/v1", "toolchain":fs::read_to_string(repo.join("lean-toolchain")).unwrap().trim(),
            "packages":{"mathlib":{"url":MATHLIB_URL,"rev":revision,"source_files":proof_source_inventory(repo,"mathlib").unwrap()}}});
        fs::write(repo.join(PROOF_INVENTORY),serde_json::to_vec(&inventory).unwrap()).unwrap();
        let path = repo.join("paper-implementation/manifest.json");
        let mut manifest = read_json(&path).unwrap();
        manifest["proof_project"] = json!({"kind":"mathlib","mathlib_rev":revision,"dependency_inventory":PROOF_INVENTORY});
        for name in [PROOF_INVENTORY, MATHLIB_LOCK, NATIVE_LAKEFILE, NATIVE_LOCK] {
            manifest["build_files"].as_array_mut().unwrap().push(json!(name));
        }
        for name in path_set(&manifest,"build_files",false).unwrap() {
            manifest["files"][&name] = json!(digest(&fs::read(repo.join(&name)).unwrap()));
        }
        fs::write(path,serde_json::to_vec(&manifest).unwrap()).unwrap();
        (dir,config)
    }

    #[test]
    fn paper_implementation_mathlib_admission_derives_identity_and_preserves_source() {
        let (dir, config) = mathlib_fixture();
        let repo = dir.path();
        let source = fs::read(repo.join("Tablet/Algorithm.lean")).unwrap();
        let subject = load_subject(repo,&config).unwrap().unwrap();
        let proof = subject.proof_support.as_ref().unwrap();
        assert_eq!(proof.kind, PaperProofLibrary::Mathlib);
        assert_eq!(proof.dependency_inventory_sha256,digest(&fs::read(repo.join(PROOF_INVENTORY)).unwrap()));
        assert_eq!(fs::read(repo.join("Tablet/Algorithm.lean")).unwrap(),source);
        assert!(!repo.join("APPROVED_AXIOMS.json").exists());
        let mut rust = subject.clone(); rust.route=crate::paper_implementation::PaperImplementationRoute::RustAeneas;
        assert!(crate::paper_implementation::validate_subject(&rust).is_err());
        let mut without=subject.clone(); without.proof_support=None;
        assert_ne!(crate::paper_implementation::approval_basis(&subject),crate::paper_implementation::approval_basis(&without));
    }

    #[test]
    fn paper_implementation_mathlib_source_drift_and_added_packages_fail_closed() {
        let (dir,config)=mathlib_fixture();
        fs::write(dir.path().join(".lake/packages/mathlib/Mathlib/Fixture.lean"),"axiom altered : False").unwrap();
        assert!(load_subject(dir.path(),&config).unwrap_err().contains("sources changed"));
        let (dir,config)=mathlib_fixture();
        fs::write(dir.path().join(".lake/packages/mathlib/Mathlib/Unlisted.lean"),"def unlisted := 1").unwrap();
        assert!(load_subject(dir.path(),&config).unwrap_err().contains("sources changed"));
        let (dir,config)=mathlib_fixture();
        fs::create_dir(dir.path().join(".lake/packages/foreign")).unwrap();
        assert!(load_subject(dir.path(),&config).unwrap_err().contains("unselected"));
        let (dir,config)=mathlib_fixture();
        fs::write(dir.path().join(".lake/package-overrides.json"),"{}").unwrap();
        assert!(load_subject(dir.path(),&config).unwrap_err().contains("overrides"));
        #[cfg(unix)] {
            let (dir,config)=mathlib_fixture();
            std::os::unix::fs::symlink("missing-overrides",dir.path().join(".lake/package-overrides.json")).unwrap();
            assert!(load_subject(dir.path(),&config).unwrap_err().contains("overrides"));
        }
    }

    #[test]
    fn paper_implementation_mathlib_never_enters_executable_or_expands_assumptions() {
        let (dir,config)=mathlib_fixture();
        repin(dir.path(),"Tablet/Algorithm.lean","-- [TABLET NODE: Algorithm]\nimport Mathlib.Fixture\ndef Algorithm (n : Nat) : Nat :=\n-- BODY\nn\n");
        assert!(load_subject(dir.path(),&config).unwrap_err().contains("unbound Lean implementation import"));
        let (dir,config)=mathlib_fixture();
        let root=fs::read_to_string(dir.path().join("lakefile.toml")).unwrap();
        repin(dir.path(),NATIVE_LAKEFILE,&root);
        assert!(load_subject(dir.path(),&config).unwrap_err().contains("dependency-free"));
        let (dir,config)=mathlib_fixture();
        fs::write(dir.path().join("PROPOSED_ASSUMPTIONS.json"),r#"{"assumptions":[{"id":"x","axiom_name":"extra","status":"pending"}]}"#).unwrap();
        assert!(load_subject(dir.path(),&config).unwrap_err().contains("staged/pending"));
    }

    #[test]
    fn paper_implementation_mathlib_rejects_nonstandard_closure_and_toolchain() {
        for package in [
            json!({"name":"batteries","url":"https://example.invalid/batteries","type":"git","rev":"a".repeat(40),"subDir":null,
                "scope":"","inputRev":"main","inherited":false,"manifestFile":"lake-manifest.json","configFile":"lakefile.toml"}),
            json!({"name":"foreign","url":"https://github.com/leanprover-community/batteries","type":"git","rev":"a".repeat(40),"subDir":null,
                "scope":"","inputRev":"main","inherited":false,"manifestFile":"lake-manifest.json","configFile":"lakefile.toml"}),
            json!({"name":"batteries","url":"https://github.com/leanprover-community/batteries","type":"path","rev":"a".repeat(40),"subDir":null,
                "scope":"","inputRev":"main","inherited":false,"manifestFile":"lake-manifest.json","configFile":"lakefile.toml"})] {
            let (dir,config)=mathlib_fixture();
            let mut lock=read_json(&dir.path().join(MATHLIB_LOCK)).unwrap(); lock["packages"]=json!([package]);
            repin(dir.path(),MATHLIB_LOCK,&serde_json::to_string(&lock).unwrap());
            assert!(load_subject(dir.path(),&config).unwrap_err().contains("unsupported or unpinned"));
        }
        let (dir,config)=mathlib_fixture();
        repin(dir.path(),"lean-toolchain","leanprover/lean4:v0.0.0\n");
        assert!(load_subject(dir.path(),&config).unwrap_err().contains("shared Lean toolchain"));
    }

    #[test]
    fn paper_implementation_mathlib_native_report_binds_separate_assembly() {
        let (dir,config)=mathlib_fixture(); let repo=dir.path();
        let mut state=ProtocolState::default(); state.paper_implementation=load_subject(repo,&config).unwrap();
        fixture_build(repo,&state);
        assert!(review_section(repo,&state).unwrap_err().contains("separate core-only assembly"));
        let manifest=read_json(&repo.join("paper-implementation/manifest.json")).unwrap();
        let mut report=read_json(&repo.join(NATIVE_BUILD_REPORT)).unwrap();
        report["proof_support"]=json!(state.paper_implementation.as_ref().unwrap().proof_support);
        report["source_files"]=json!({"Tablet/Algorithm.lean":manifest["files"]["Tablet/Algorithm.lean"]});
        report["native_build_inputs"]=json!({
            "lakefile.toml":{"source_path":NATIVE_LAKEFILE,"sha256":manifest["files"][NATIVE_LAKEFILE]},
            "lake-manifest.json":{"source_path":NATIVE_LOCK,"sha256":manifest["files"][NATIVE_LOCK]},
            "lean-toolchain":{"source_path":"lean-toolchain","sha256":manifest["files"]["lean-toolchain"]}});
        fs::write(repo.join(NATIVE_BUILD_REPORT),serde_json::to_vec(&report).unwrap()).unwrap();
        review_section(repo,&state).unwrap();
        report["native_build_inputs"]["lakefile.toml"]["source_path"]=json!("lakefile.toml");
        fs::write(repo.join(NATIVE_BUILD_REPORT),serde_json::to_vec(&report).unwrap()).unwrap();
        assert!(review_section(repo,&state).unwrap_err().contains("separate core-only assembly"));
    }
    #[test]
    fn paper_implementation_io_original_paper_and_exclusions_without_goal() {
        let (dir, config) = fixture();
        let subject = load_subject(dir.path(), &config).unwrap().unwrap();
        assert!(!dir.path().join("GOAL.md").exists());
        assert_eq!(subject.implementation_roots.len(), 1);
        assert!(subject.excluded_complexity_clauses[&TargetId::from("main")][0].contains("linear time"));
        let mut state = ProtocolState::default();
        state.configured_targets.insert(TargetId::from("main"));
        state.paper_implementation = Some(subject);
        verify_subject(dir.path(), &config, &state).unwrap();
        fixture_build(dir.path(), &state);
        let review = review_section(dir.path(), &state).unwrap();
        assert!(review["original_evidence"]["paper/main.tex"]["utf8"].as_str().unwrap().contains("returns its input"));
        assert_eq!(review["review"]["human_review_topics"].as_array().unwrap().len(), 3);
    }
    #[test]
    fn paper_implementation_io_source_and_config_changes_invalidate() {
        let (dir, config) = fixture();
        let mut state = ProtocolState::default();
        state.configured_targets.insert(TargetId::from("main"));
        state.paper_implementation = load_subject(dir.path(), &config).unwrap();
        repin(dir.path(), "Tablet/Algorithm.lean", "-- [TABLET NODE: Algorithm]\ndef Algorithm (n : Nat) : Nat :=\n-- BODY\nn + 1\n");
        assert!(verify_subject(dir.path(), &config, &state).unwrap_err().contains("changed after intake"));
        let (dir, config) = fixture();
        state.paper_implementation = load_subject(dir.path(), &config).unwrap();
        let mut c = read_json(&config).unwrap();
        c["trust_base"] = json!({"protocol":"trellis-trust-v1"});
        fs::write(&config, serde_json::to_vec(&c).unwrap()).unwrap();
        assert!(verify_subject(dir.path(), &config, &state).is_err());
    }
    #[test]
    fn paper_implementation_io_rejects_lean_pv_only_trust_profile() {
        let (dir, config) = fixture();
        let mut c = read_json(&config).unwrap();
        c["trust_base"] = json!({"protocol":"trellis-trust-v1"});
        fs::write(&config, serde_json::to_vec(&c).unwrap()).unwrap();
        assert!(load_subject(dir.path(), &config).unwrap_err().contains("PV-only required_v1"));
        assert_eq!(read_json(&config).unwrap()["trust_base"]["protocol"], "trellis-trust-v1");
    }
    #[test]
    fn paper_implementation_io_rejects_runtime_escapes_and_unpinned_trust() {
        for keyword in ["partial", "unsafe", "extern", "implemented_by", "axiom", "csimp"] {
            let (dir, config) = fixture();
            let text = format!("-- [TABLET NODE: Algorithm]\n{keyword} def Algorithm (n : Nat) : Nat := n\n");
            repin(dir.path(), "Tablet/Algorithm.lean", &text);
            assert!(load_subject(dir.path(), &config).is_err(), "{keyword}");
        }
        let (dir, config) = fixture();
        fs::write(dir.path().join("APPROVED_AXIOMS.json"), "[\"unsound\"]").unwrap();
        assert!(load_subject(dir.path(), &config).unwrap_err().contains("trust policy must be pinned"));
    }
    #[test]
    fn paper_implementation_io_rejects_external_package_resolution() {
        let (dir, config) = fixture();
        let mut packages = read_json(&dir.path().join("lake-manifest.json")).unwrap();
        packages["packages"] = json!([{"name":"replacement", "type":"path", "dir":"../foreign"}]);
        repin(dir.path(), "lake-manifest.json", &serde_json::to_string(&packages).unwrap());
        assert!(load_subject(dir.path(), &config).unwrap_err().contains("empty package resolution"));
    }
    #[test]
    fn paper_implementation_io_reuses_pending_assumption_rejection() {
        let (dir, config) = fixture();
        fs::write(dir.path().join("PROPOSED_ASSUMPTIONS.json"), r#"{"assumptions":[{"id":"x","axiom_name":"extra","status":"pending"}]}"#).unwrap();
        assert!(load_subject(dir.path(), &config).unwrap_err().contains("staged/pending"));
    }
    #[test]
    fn paper_implementation_io_rejects_cost_targets_and_mismatched_paper_locator() {
        let (dir, config) = fixture();
        let path = dir.path().join("paper-implementation/manifest.json");
        let mut m = read_json(&path).unwrap();
        m["targets"][0]["kind"] = json!("running_time");
        fs::write(&path, serde_json::to_vec(&m).unwrap()).unwrap();
        assert!(load_subject(dir.path(), &config).unwrap_err().contains("outside v1"));
        m["targets"][0].as_object_mut().unwrap().remove("kind");
        m["targets"][0]["start_line"] = json!(2);
        fs::write(&path, serde_json::to_vec(&m).unwrap()).unwrap();
        assert!(load_subject(dir.path(), &config).unwrap_err().contains("locator differs"));
    }
    #[test]
    fn paper_implementation_io_math_config_is_unchanged() {
        let (dir, config) = fixture();
        fs::write(&config, r#"{"workflow":{"paper_tex_path":"paper/main.tex"}}"#).unwrap();
        assert!(load_subject(dir.path(), &config).unwrap().is_none());
    }
}
