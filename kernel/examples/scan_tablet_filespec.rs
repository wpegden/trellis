//! One-off scanner: run `validate_filespec` on every `*.lean` file
//! under `<repo>/Tablet/` and report violations.
//!
//! Usage:
//!   cargo run --manifest-path kernel/Cargo.toml \
//!     --example scan_tablet_filespec -- <repo_path>
//!
//! Exit code 0 if every ordinary node passes, 1 otherwise.
use std::path::PathBuf;
use std::process::ExitCode;

use trellis_kernel::filespec_split::validate_filespec;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: scan_tablet_filespec <repo_path>");
        return ExitCode::from(2);
    }
    let repo = PathBuf::from(&args[1]);
    let tablet = repo.join("Tablet");
    let entries = match std::fs::read_dir(&tablet) {
        Ok(it) => it,
        Err(e) => {
            eprintln!("read_dir {}: {e}", tablet.display());
            return ExitCode::from(2);
        }
    };
    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("lean"))
        .filter(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s != "Preamble")
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    let mut offenders: Vec<(String, String)> = Vec::new();
    for path in &files {
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                offenders.push((stem, format!("read failed: {e}")));
                continue;
            }
        };
        if let Err(reason) = validate_filespec(&content, &stem) {
            offenders.push((stem, reason));
        }
    }
    println!("Scanned: {} ordinary tablet .lean files", files.len());
    println!("Offenders: {}", offenders.len());
    for (name, reason) in &offenders {
        println!("  {name}: {reason}");
    }
    if offenders.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
