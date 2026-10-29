//! Time dispatch state and event-log loading without changing run state or sources.
//! Usage: profile_dispatch_load STATE_JSON EVENT_LOG_DIR [SEPARATE_CACHE_ROOT]
//! The optional cache directory measures cold and warm event counts.
use std::{fs, path::PathBuf, time::Instant};
use trellis_kernel::{model::{ProtocolState, recompute_local_closure_reverse_indices},
    runtime::{EventLogRecord, event_log_cycle_files}};

fn timed<T>(name: &str, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let value = f();
    eprintln!("{name}: {:.3}s", start.elapsed().as_secs_f64());
    value
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert!((3..=4).contains(&args.len()),
        "usage: profile_dispatch_load STATE_JSON EVENT_LOG_DIR [SEPARATE_CACHE_ROOT]");
    let raw = timed("read state", || fs::read_to_string(&args[1]).unwrap());
    let mut state: ProtocolState = timed("decode state", || serde_json::from_str(&raw).unwrap());
    timed("normalize", || state.normalize_all_structural_state());
    timed("reverse indices", || recompute_local_closure_reverse_indices(&mut state));
    timed("validate", || state.validate().unwrap());
    let status = timed("status projection", || trellis_kernel::dispatch_coordinator::status(&state).unwrap());
    let text = timed("serialize status", || serde_json::to_vec(&status).unwrap());
    eprintln!("status bytes: {}", text.len());
    if let Some(root) = args.get(3) {
        for name in ["cached count cold", "cached count warm"] {
            let count = timed(name, || trellis_kernel::runtime::read_event_count(
                &PathBuf::from(&args[2]), Some(&PathBuf::from(root))).unwrap());
            eprintln!("records: {count}");
        }
        return;
    }
    let mut bytes = 0;
    let mut records = 0;
    let mut invalid = 0;
    timed("decode event log", || {
        for path in event_log_cycle_files(&PathBuf::from(&args[2])).unwrap() {
            let text = fs::read_to_string(path).unwrap();
            bytes += text.len();
            for line in text.lines().filter(|line| !line.trim().is_empty()) {
                records += 1;
                invalid += usize::from(serde_json::from_str::<EventLogRecord>(line).is_err());
            }
        }
    });
    eprintln!("event bytes: {bytes}; records: {records}; invalid: {invalid}");
}
