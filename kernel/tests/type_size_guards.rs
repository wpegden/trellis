use std::mem::size_of;

use trellis_kernel::{ProtocolCommand, ProtocolState, WrapperRequest};

/// Keep large request carriers indirect. These ceilings intentionally leave
/// kilobytes of rustc/layout drift; they are tripwires against accidentally
/// embedding another full WrapperRequest, not byte-exact ABI promises.
#[test]
fn protocol_carrier_types_stay_below_stack_safe_ceiling() {
    let state = size_of::<ProtocolState>();
    let request = size_of::<WrapperRequest>();
    let command = size_of::<ProtocolCommand>();
    assert!(
        state <= 12 * 1024,
        "ProtocolState grew to {state} bytes (ceiling: 12288); keep large request carriers boxed"
    );
    assert!(
        request <= 8 * 1024,
        "WrapperRequest grew to {request} bytes (ceiling: 8192); inspect newly embedded payloads"
    );
    assert!(
        command <= 4 * 1024,
        "ProtocolCommand grew to {command} bytes (ceiling: 4096); keep IssueRequest.request boxed"
    );
}
