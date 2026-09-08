# kernel

Pure Rust protocol kernel plus a small persistent runtime shell for the deterministic supervisor protocol.

Current scope:
- theorem-stating
- proof-formalization
- cleanup
- explicit wrapper request/response boundary
- deterministic verifier-lane reconciliation in the kernel
- persistent runtime state, event log, and checkpoint files

Current non-goals:
- agent launching policy
- prompt construction policy
- tmux / port / backend mechanics

Primary entry point:
- `apply_event(state, event) -> Result<TransitionOutcome, TransitionError>`
- `apply_transition_request(request) -> TransitionResponse`
- `SupervisorRuntime::step(adapter)` for one persisted protocol transition

Thin wrapper CLI:
- binary: `trellis_kernel_cli`
- reads a single JSON `TransitionRequest` from stdin
- writes a single JSON `TransitionResponse` to stdout
- exit codes:
  - `0` success transition
  - `1` protocol error transition
  - `2` malformed input / wrapper failure

Persistent runtime CLI:
- binary: `trellis_runtime_cli`
- reads a single JSON request from stdin
- supports:
  - `init`
  - `show`
  - `step`
  - `run`
- persists:
  - `protocol_state.json`
  - `event_log.jsonl`
  - `checkpoint.json`
- can step from:
  - a provided normalized `WrapperResponse`, or
  - an external bridge process via `TRELLIS_RUNTIME_BRIDGE_CMD`
- can execute checkpoint side effects through `TRELLIS_RUNTIME_CHECKPOINT_HOOK`

Bridge boundary:
- Rust owns the narrow `WrapperRequest` payload shape
- Python bridge consumes only:
  - `config_path`
  - `runtime_root`
  - that narrow request
- Python returns only a normalized `WrapperResponse`
- verifier-lane reconciliation stays in Rust

Repo entry points:
- supervisor runtime wrapper:
  - [`scripts/trellis.sh`](../scripts/trellis.sh)
- bridge executable wrapper:
  - [`scripts/trellis_bridge.sh`](../scripts/trellis_bridge.sh)
- git checkpoint hook:
  - [`scripts/trellis_git_checkpoint_hook.sh`](../scripts/trellis_git_checkpoint_hook.sh)
  - [`trellis/runtime/git_checkpoint_hook.py`](../trellis/runtime/git_checkpoint_hook.py)

Example:
```bash
echo '{
  "state": { "stage": "Start" },
  "event": { "event": "start_cycle" }
}' | cargo run --bin trellis_kernel_cli
```

The implementation is intended to track the protocol in:
- [`spec/SupervisorProtocol.tla`](../spec/SupervisorProtocol.tla)

Protocol parity rule:
- parity is bidirectional:
  - changes to kernel protocol state, request/response shape, or transition
    semantics must be reflected in the TLA+ spec in the same change
  - if the TLA+ spec changes to fix or clarify protocol semantics, inspect the
    corresponding Rust path in the same change and either confirm parity or fix
    the kernel too
- neither side is considered complete until TLC passes against the updated spec
