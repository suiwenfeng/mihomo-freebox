//! HTTP fetches via the `curl` binary — with optional SOCKS5 proxy support.
//!
//! Mirrors `freebox/fetch.py`: a few retries with backoff and a generous
//! per-request timeout.  Using curl (already in the image / runners) avoids a
//! TLS crate and its C/openssl toolchain, keeping the crawler a tiny,
//! statically-linkable no_std-friendly Rust binary.
//!
//! When a local mihomo proxy is running (e.g. the freebox container's own
//! mixed-port on `127.0.0.1:7890`) it can be used as a SOCKS5 upstream so the
//! crawler can reach subscription URLs that are blocked from direct access
//! (the very "翻墙" scenario the GFW creates for many blog-style sources).
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

/// A SOCKS5/HTTP proxy endpoint (host:port).
#[derive(Debug, Clone)]
pub struct Proxy {
    pub host: String,
    pub port: u16,
}

impl Proxy {
    pub fn from_host_port(host: &str, port: u16) -> Self {
        Proxy {
            host: host.to_string(),
            port,
        }
    }

    /// Parse `host:port` or `socks5://host:port` into a [`Proxy`].
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s
            .strip_prefix("socks5://")
            .or_else(|| s.strip_prefix("socks5h://"))
            .unwrap_or(s);
        let (host, port) = s.rsplit_once(':')?;
        let port: u16 = port.parse().ok()?;
        Some(Proxy::from_host_port(host, port))
    }

    /// Human-readable `host:port`.
    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// Check whether a TCP proxy endpoint is accepting connections.
#[allow(dead_code)]
pub fn proxy_reachable(proxy: &Proxy) -> bool {
    let addr = format!("{}:{}", proxy.host, proxy.port);
    if let Ok(socket_addr) = addr.parse::<SocketAddr>() {
        TcpStream::connect_timeout(&socket_addr, Duration::from_secs(1)).is_ok()
    } else {
        // Fall back to hostname resolution.
        use std::net::ToSocketAddrs;
        (addr.as_str(), 0)
            .to_socket_addrs()
            .map(|mut i| i.find(|a| TcpStream::connect_timeout(a, Duration::from_secs(1)).is_ok()))
            .map(|_| true)
            .unwrap_or(false)
    }
}

/// Fetch `url` as text using **direct** (no proxy) curl.
///
/// `timeout` is the whole-request ceiling (seconds).  Returns the response body
/// as a UTF-8 string.
pub fn http_get(url: &str, timeout: u64) -> Result<String, String> {
    http_get_with(url, timeout, None)
}

/// Fetch `url` through a SOCKS5 proxy (the mihomo local proxy).
///
/// Uses `--socks5-hostname` so DNS is resolved by the proxy (important for
/// GFW-blocked domains whose DNS may also be poisoned).
pub fn http_get_proxy(url: &str, timeout: u64, proxy: &Proxy) -> Result<String, String> {
    let max_time = (timeout + 5).to_string();
    let connect = timeout.to_string();
    let out = std::process::Command::new("curl")
        .args([
            "-fsSL",
            "--compressed",
            "--retry",
            "1",
            "--retry-delay",
            "2",
            "--retry-all-errors",
            "--connect-timeout",
            &connect,
            "--max-time",
            &max_time,
            "-A",
            USER_AGENT,
            "--socks5-hostname",
            &proxy.endpoint(),
            url,
        ])
        .output()
        .map_err(|e| format!("exec curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "curl exit {} (via {}) for {}",
            out.status.code().unwrap_or(-1),
            proxy.endpoint(),
            url
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("utf8: {e}"))
}

/// Fetch `url` with an optional SOCKS5 proxy.  When `proxy` is `Some`, the
/// request goes through `--socks5-hostname`; otherwise a plain direct fetch.
fn http_get_with(url: &str, timeout: u64, proxy: Option<&Proxy>) -> Result<String, String> {
    let max_time = (timeout + 5).to_string();
    let connect = timeout.to_string();
    let mut cmd = std::process::Command::new("curl");
    cmd.args([
        "-fsSL",
        "--compressed",
        "--retry",
        "2",
        "--retry-delay",
        "1",
        "--retry-all-errors",
        "--connect-timeout",
        &connect,
        "--max-time",
        &max_time,
        "-A",
        USER_AGENT,
    ]);

    let proxy_label;
    if let Some(p) = proxy {
        let endpoint = p.endpoint();
        proxy_label = format!(" via {}", endpoint);
        cmd.arg("--socks5-hostname").arg(&endpoint);
    } else {
        proxy_label = String::new();
    }
    cmd.arg(url);
    let out = cmd
        .output()
        .map_err(|e| format!("exec curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "curl exit {}{}",
            out.status.code().unwrap_or(-1),
            proxy_label,
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("utf8: {e}"))
}

/// A fetcher that tries direct access first, then falls back to a SOCKS5 proxy
/// if a local mihomo proxy is available.
///
/// This is the primary way sources fetch their subscription URLs: it works
/// seamlessly in both environments where the proxy is absent (pure direct
/// access, e.g. CI) and where the proxy is present (the freebox container
/// running its own mihomo on :7890).
pub struct SubFetcher {
    proxy: Option<Proxy>,
}

impl SubFetcher {
    pub fn new(proxy: Option<Proxy>) -> Self {
        SubFetcher { proxy }
    }

    /// Try to get `url` directly; if that fails and a proxy is available,
    /// retry through the proxy.
    pub fn get(&self, url: &str, timeout: u64) -> Result<String, String> {
        // Fast path: direct fetch succeeds.
        if let Ok(text) = http_get(url, timeout) {
            return Ok(text);
        }
        // Fallback: proxy.
        if let Some(ref p) = self.proxy {
            eprintln!("[crawler] retrying {url} via proxy {}", p.endpoint());
            http_get_proxy(url, timeout, p)
        } else {
            Err(format!("fetch failed (no proxy available) for {url}"))
        }
    }

    /// Fetch `url` **only** through the proxy (skip the direct attempt).
    /// Falls back to direct if no proxy is configured.
    pub fn get_via_proxy(&self, url: &str, timeout: u64) -> Result<String, String> {
        if let Some(ref p) = self.proxy {
            http_get_proxy(url, timeout, p)
        } else {
            http_get(url, timeout)
        }
    }

    /// The resolved proxy, if any.
    #[allow(dead_code)]
    pub fn proxy(&self) -> Option<&Proxy> {
        self.proxy.as_ref()
    }
}

/// Auto-detect a local mihomo SOCKS5 proxy.
///
/// Checks (in order):
/// 1. `PROXY_URL` env var (e.g. `socks5://127.0.0.1:7890`)
/// 2. `SOCKS5_PROXY` env var (`host:port`)
/// 3. Well-known local mihomo port `127.0.0.1:7890`
///
/// Returns `Some(proxy)` if a reachable proxy is found, `None` otherwise.
/// Set `SKIP_PROXY=1` to disable auto-detection entirely.
pub fn auto_detect_proxy() -> Option<Proxy> {
    if std::env::var("SKIP_PROXY").as_deref().unwrap_or("") == "1" {
        return None;
    }

    // Explicit override.
    if let Ok(v) = std::env::var("PROXY_URL") {
        if let Some(p) = Proxy::parse(&v) {
            if proxy_reachable(&p) {
                eprintln!("[crawler] using proxy from PROXY_URL: {}", p.endpoint());
                return Some(p);
            }
        }
    }
    if let Ok(v) = std::env::var("SOCKS5_PROXY") {
        if let Some(p) = Proxy::parse(&v) {
            if proxy_reachable(&p) {
                eprintln!("[crawler] using proxy from SOCKS5_PROXY: {}", p.endpoint());
                return Some(p);
            }
        }
    }

    // Well-known local mihomo mixed-port.
    let default = Proxy::from_host_port("127.0.0.1", 7890);
    if proxy_reachable(&default) {
        eprintln!("[crawler] auto-detected local mihomo proxy at {}", default.endpoint());
        Some(default)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_socks5_url() {
        let p = Proxy::parse("socks5://127.0.0.1:7890").unwrap();
        assert_eq!(p.host, "127.0.0.1");
        assert_eq!(p.port, 7890);
    }

    #[test]
    fn parse_host_port() {
        let p = Proxy::parse("127.0.0.1:7890").unwrap();
        assert_eq!(p.host, "127.0.0.1");
        assert_eq!(p.port, 7890);
    }

    #[test]
    fn parse_socks5h_prefix() {
        let p = Proxy::parse("socks5h://10.0.0.1:1080").unwrap();
        assert_eq!(p.host, "10.0.0.1");
        assert_eq!(p.port, 1080);
    }

    #[test]
    fn parse_invalid() {
        assert!(Proxy::parse("not-a-proxy").is_none());
        assert!(Proxy::parse("host:port:extra").is_none());
        assert!(Proxy::parse("").is_none());
    }

    #[test]
    fn endpoint_format() {
        let p = Proxy::from_host_port("127.0.0.1", 7890);
        assert_eq!(p.endpoint(), "127.0.0.1:7890");
    }
}
