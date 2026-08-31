//! HTTP fetches via the `curl` binary.
//!
//! Mirrors `freebox/fetch.py`: a few retries with backoff and a generous
//! per-request timeout.  Using curl (already in the image / runners) avoids a
//! TLS crate and its C/openssl toolchain, keeping the crawler a tiny,
//! statically-linkable no_std-friendly Rust binary.
use std::process::Command;

const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

/// Fetch `url` as text. `timeout` is the whole-request ceiling (seconds).
pub fn http_get(url: &str, timeout: u64) -> Result<String, String> {
    let max_time = (timeout + 5).to_string();
    let connect = timeout.to_string();
    let out = Command::new("curl")
        .args([
            "-fsSL",
            "--compressed",
            "--retry",
            "3",
            "--retry-delay",
            "1",
            "--retry-all-errors",
            "--connect-timeout",
            &connect,
            "--max-time",
            &max_time,
            "-A",
            USER_AGENT,
            url,
        ])
        .output()
        .map_err(|e| format!("exec curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "curl exit {} for {}",
            out.status.code().unwrap_or(-1),
            url
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("utf8: {e}"))
}
