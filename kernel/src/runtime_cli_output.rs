//! Preserve the pretty JSON wire format without one pipe write per line.
use serde::Serialize;
use std::io::{self, BufWriter, Write};

pub fn write_response(output: impl Write, response: &impl Serialize) -> io::Result<()> {
    let mut output = BufWriter::with_capacity(64 * 1024, output);
    serde_json::to_writer_pretty(&mut output, response).map_err(io::Error::other)?;
    output.write_all(b"\n")?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_exact_pretty_response_bytes() {
        for value in [serde_json::json!({"status":"ok","text":"α\nβ","empty":null}),
            serde_json::json!({"status":"error","message":"failed"}),
            serde_json::json!([]), serde_json::json!(vec!["α\nβ"; 20_000])] {
            let mut expected = serde_json::to_vec_pretty(&value).unwrap();
            expected.push(b'\n');
            let mut actual = Vec::new();
            write_response(&mut actual, &value).unwrap();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn flush_failure_is_reported() {
        struct RefusesFlush;
        impl Write for RefusesFlush {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> { Ok(bytes.len()) }
            fn flush(&mut self) -> io::Result<()> { Err(io::Error::other("flush failed")) }
        }
        assert!(write_response(RefusesFlush, &serde_json::json!({"ok":true})).is_err());
    }
}
