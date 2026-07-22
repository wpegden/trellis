//! One-off helper: print the kernel's `prescribed_region(include_body)` for a
//! single Tablet node `.lean` file, byte-for-byte, to stdout (no trailing
//! newline added). The PV extraction-pipeline gate (`scripts/extract_pv_model.py`
//! gate (c)) uses this to assert the emitted config `lean` text equals exactly
//! what the kernel byte-pin will compare against — i.e. that the config's
//! prescription and the on-disk Tablet file agree under the real kernel slice
//! logic, with no Python re-implementation of `prescribed_region`.
//!
//! Usage:
//!   cargo run --manifest-path kernel/Cargo.toml \
//!     --example print_prescribed_region -- <file.lean> [--head-only]
//!
//! Default emits the Def-kind region (include_body=true). `--head-only` emits
//! the Theorem-kind region (include_body=false). Exit 0 on success; 2 on usage
//! or read error; 1 if the region cannot be computed (malformed FILESPEC).
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use trellis_kernel::filespec_split::prescribed_region;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args.len() > 3 {
        eprintln!("usage: print_prescribed_region <file.lean> [--head-only]");
        return ExitCode::from(2);
    }
    let path = PathBuf::from(&args[1]);
    let include_body = match args.get(2).map(String::as_str) {
        None => true,
        Some("--head-only") => false,
        Some(other) => {
            eprintln!("unknown flag {other:?}; expected --head-only");
            return ExitCode::from(2);
        }
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("read {}: {e}", path.display());
            return ExitCode::from(2);
        }
    };
    match prescribed_region(&content, include_body) {
        Ok(region) => {
            // Emit verbatim, no added trailing newline — the caller compares
            // byte-for-byte against the config `lean` string.
            let mut out = std::io::stdout().lock();
            if out.write_all(region.as_bytes()).is_err() {
                return ExitCode::from(2);
            }
            ExitCode::SUCCESS
        }
        Err(reason) => {
            eprintln!("prescribed_region {}: {reason}", path.display());
            ExitCode::from(1)
        }
    }
}
