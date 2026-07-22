use std::io::{self, Read, Write};

use trellis_kernel::{apply_transition_request, TransitionRequest, TransitionResponse};

fn main() {
    let exit_code = match run() {
        Ok(code) => code,
        Err(message) => {
            let _ = writeln!(io::stderr(), "{message}");
            2
        }
    };
    std::process::exit(exit_code);
}

fn run() -> Result<i32, String> {
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .map_err(|err| format!("failed to read stdin: {err}"))?;

    let request: TransitionRequest = serde_json::from_str(&input).map_err(|err| {
        let response = serde_json::json!({
            "status": "invalid_request",
            "message": err.to_string(),
        });
        let _ = serde_json::to_writer_pretty(io::stdout(), &response);
        let _ = writeln!(io::stdout());
        format!("invalid request JSON: {err}")
    })?;

    let response = apply_transition_request(request);
    serde_json::to_writer_pretty(io::stdout(), &response)
        .map_err(|err| format!("failed to write response JSON: {err}"))?;
    writeln!(io::stdout()).map_err(|err| format!("failed to finalize stdout: {err}"))?;

    Ok(match response {
        TransitionResponse::Success { .. } => 0,
        TransitionResponse::Error { .. } => 1,
    })
}
