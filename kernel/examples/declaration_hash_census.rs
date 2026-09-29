//! Read-only census of production declaration hashes for a Lean tablet.
//!
//! Build this example from two kernel revisions and run both binaries against
//! the same repository.  The output is deterministic JSON and does not invoke
//! Lean, Git, or any project hook.

use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;

fn whole_file_hash(content: &str) -> String {
    format!("whole-file-v1:{:x}", Sha256::digest(content.as_bytes()))
}

fn run() -> Result<(), String> {
    let mut arguments = env::args_os().skip(1);
    let repo_path = PathBuf::from(
        arguments
            .next()
            .ok_or_else(|| "usage: declaration_hash_census REPO".to_string())?,
    );
    if arguments.next().is_some() {
        return Err("usage: declaration_hash_census REPO".to_string());
    }
    let tablet_path = repo_path.join("Tablet");
    let mut source_paths: Vec<PathBuf> = fs::read_dir(&tablet_path)
        .map_err(|error| format!("cannot read {}: {error}", tablet_path.display()))?
        .map(|entry| entry.map(|value| value.path()))
        .collect::<Result<_, _>>()
        .map_err(|error| format!("cannot enumerate {}: {error}", tablet_path.display()))?;
    source_paths.retain(|path| path.extension().and_then(|value| value.to_str()) == Some("lean"));
    source_paths.sort();

    let mut hashes = BTreeMap::new();
    let mut errors = BTreeMap::new();
    for source_path in source_paths {
        let Some(node) = source_path.file_stem().and_then(|value| value.to_str()) else {
            continue;
        };
        let content = match fs::read_to_string(&source_path) {
            Ok(content) => content,
            Err(error) => {
                errors.insert(node.to_string(), format!("read failed: {error}"));
                continue;
            }
        };
        let result = if node == "Preamble" || node == "Axioms" {
            Ok(whole_file_hash(&content))
        } else {
            trellis_kernel::filespec_split::declaration_hash_strict(&repo_path, &content, node)
        };
        match result {
            Ok(hash) => {
                hashes.insert(node.to_string(), hash);
            }
            Err(error) => {
                errors.insert(node.to_string(), error);
            }
        }
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "repo": repo_path,
            "hashes": hashes,
            "errors": errors,
        }))
        .map_err(|error| format!("cannot serialize result: {error}"))?
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(2);
    }
}
