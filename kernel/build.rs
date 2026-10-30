//! Emit the set of prompt fragments that have an `_isabelle` sibling.
//!
//! Backend variants used to be selected by a hand-written conditional at each
//! call site, so adding a variant and forgetting the conditional silently sent
//! Lean text to an Isabelle run. Scanning at build time makes the variant FILE
//! the only thing anyone has to add: the routing table is regenerated from disk
//! on every build, and `kernel/tests/isabelle_prompt_decontamination.rs` fails
//! if a Lean variant is ever emitted while its sibling exists.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use sha2::{Digest, Sha256};

fn acceptance_sources() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).parent().unwrap().to_path_buf();
    let spec: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join("acceptance-sources.json")).expect("acceptance source manifest")).unwrap();
    let strings = |key: &str| -> Vec<String> { spec[key].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect() };
    let schema = spec["schema"].as_str().unwrap();
    assert_eq!(schema, "trellis-worker-acceptance-v2");
    let ignored_dirs = strings("exclude_directories");
    let prefixes = strings("exclude_prefixes");
    let suffixes = strings("exclude_suffixes");
    let extensions = strings("extensions");
    let mut names: std::collections::BTreeSet<String> = strings("files").into_iter().collect();
    let mut pending: Vec<PathBuf> = strings("trees").iter().map(|s| root.join(s)).collect();
    while let Some(dir) = pending.pop() {
        println!("cargo:rerun-if-changed={}", dir.display());
        for entry in std::fs::read_dir(&dir).expect("acceptance source directory") {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_str().expect("UTF-8 source path");
            if ignored_dirs.iter().any(|n| n == name) || prefixes.iter().any(|p| name.starts_with(p)) || suffixes.iter().any(|s| name.ends_with(s)) { continue; }
            if path.is_dir() {
                // Match Python os.walk: directory symlinks are not traversed.
                if !path.is_symlink() { pending.push(path); }
            }
            else if path.extension().and_then(|s| s.to_str()).is_some_and(|s| extensions.iter().any(|e| e == s)) {
                names.insert(path.strip_prefix(&root).unwrap().to_str().unwrap().replace('\\', "/"));
            }
        }
    }
    let mut generated = String::from("pub const ACCEPTANCE_LOGIC_SOURCES: &[(&str, &str)] = &[\n");
    let mut sources = Vec::new();
    for name in names {
        let path = root.join(&name);
        println!("cargo:rerun-if-changed={}", path.display());
        let hash = format!("{:x}", Sha256::digest(std::fs::read(path).expect("acceptance source input")));
        generated.push_str(&format!("({name:?}, {hash:?}),\n"));
        sources.push(serde_json::json!({"path": name, "sha256": hash}));
    }
    generated.push_str("];\n");
    let compiler = std::process::Command::new(std::env::var("RUSTC").unwrap()).args(["--version", "--verbose"]).output().expect("compiler identity");
    let mut features: Vec<String> = std::env::vars().filter_map(|(k, _)| k.strip_prefix("CARGO_FEATURE_").map(str::to_string)).collect();
    features.sort();
    let build = serde_json::json!({"schema": schema, "sources": sources,
        "compiler": String::from_utf8(compiler.stdout).unwrap(), "target": std::env::var("TARGET").unwrap(),
        "profile": std::env::var("PROFILE").unwrap(), "features": features});
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::write(out.join("acceptance_sources.rs"), generated).unwrap();
    std::fs::write(out.join("acceptance_build.json"), serde_json::to_string(&build).unwrap()).unwrap();
}

fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, root, out);
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        let Some(stem) = name.strip_suffix("_isabelle.md") else { continue };
        let lean = path.with_file_name(format!("{stem}.md"));
        if !lean.is_file() {
            continue;
        }
        let rel = |p: &Path| {
            p.strip_prefix(root)
                .ok()
                .and_then(|r| r.to_str())
                .map(|s| s.replace('\\', "/"))
        };
        if let (Some(k), Some(v)) = (rel(&lean), rel(&path)) {
            out.insert(k, v);
        }
    }
}

fn main() {
    acceptance_sources();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("trellis")
        .join("prompt_fragments");
    println!("cargo:rerun-if-changed={}", root.display());

    let mut twins = BTreeMap::new();
    walk(&root, &root, &mut twins);

    let mut src = String::from(
        "// @generated by build.rs — Lean fragment id -> its `_isabelle` sibling.\n\
         pub const ISABELLE_FRAGMENT_TWINS: &[(&str, &str)] = &[\n",
    );
    for (lean, isa) in &twins {
        src.push_str(&format!("    ({lean:?}, {isa:?}),\n"));
    }
    src.push_str("];\n");

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("isabelle_fragment_twins.rs");
    std::fs::write(&out, src).expect("write isabelle_fragment_twins.rs");
}
