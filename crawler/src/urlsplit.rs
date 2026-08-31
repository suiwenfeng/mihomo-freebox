//! Minimal, CPython-`urllib.parse.urlsplit`-compatible URL splitter.
//!
//! We only need the subset the proxy parsers exercise: `scheme`, `netloc`,
//! `path`, `query`, `fragment`, plus the derived `username` / `password` /
//! `hostname` / `port`.  A tiny hand-rolled splitter (rather than the `url`
//! crate) keeps behaviour bit-for-bit aligned with the Python crawler, which
//! matters because some free-node URLs are unusually shaped (`ss://` with a
//! base64 userinfo, `vmess://` whose body lives in the netloc, `vless://` with
//! a UUID as userinfo).

/// A single URL split into its components, mirroring `urllib.parse.SplitResult`.
/// `path` / `password` are parsed for parity with Python's `SplitResult` but are
/// not currently read by the proxy parsers.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct UrlSplit {
    pub scheme: String,
    pub netloc: String,
    pub path: String,
    pub query: String,
    pub fragment: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub hostname: Option<String>,
    /// Port substring exactly as it appears (may be empty / non-numeric).
    pub port_str: String,
    /// Parsed port, or `None` (also `None` when non-numeric — mirrors the
    /// Python parsers, where a non-numeric port raises and the node is dropped).
    pub port: Option<u16>,
}

fn is_scheme_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.'
}

/// Split `host:port`, `[ipv6]:port`, or bare `host` into (hostname, port_str).
/// Hostname is lower-cased and de-bracketed, matching `SplitResult.hostname`.
fn split_host_port(hostinfo: &str) -> (Option<String>, String) {
    if hostinfo.is_empty() {
        return (None, String::new());
    }
    if let Some(stripped) = hostinfo.strip_prefix('[') {
        // [ipv6]:port
        if let Some(close) = stripped.find(']') {
            let ip = stripped[..close].to_ascii_lowercase();
            let rest = &stripped[close + 1..];
            if rest.starts_with(':') {
                return (Some(ip), rest[1..].to_string());
            }
            return (Some(ip), String::new());
        }
        return (Some(hostinfo.to_ascii_lowercase()), String::new());
    }
    // plain host:port
    match hostinfo.rfind(':') {
        Some(i) => {
            let host = hostinfo[..i].to_ascii_lowercase();
            let port = hostinfo[i + 1..].to_string();
            (Some(host), port)
        }
        None => (Some(hostinfo.to_ascii_lowercase()), String::new()),
    }
}

/// Parse an absolute URL into a [`UrlSplit`].
pub fn parse_url(input: &str) -> UrlSplit {
    let s = input.trim();

    // 1) fragment (everything after the first '#') — mirrors CPython.
    let (base, fragment) = match s.find('#') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None => (s, ""),
    };

    // 2) scheme — leading [a-zA-Z][a-zA-Z0-9+.-]* before the first ':'
    let (scheme, rest) = match base.find(':') {
        Some(i) => {
            let cand = &base[..i];
            if !cand.is_empty() && cand.bytes().all(is_scheme_char) {
                (cand.to_ascii_lowercase(), &base[i + 1..])
            } else {
                (String::new(), base)
            }
        }
        None => (String::new(), base),
    };

    // 3) netloc — only when the scheme is followed by "//"
    let (netloc, rem) = if rest.starts_with("//") {
        let after = &rest[2..];
        let end = after
            .find(|c| c == '/' || c == '?' || c == '#')
            .unwrap_or(after.len());
        (after[..end].to_string(), &after[end..])
    } else {
        (String::new(), rest)
    };

    // 4) path & query (no '#' remains in `rem` — fragment was stripped first)
    let (path, query) = match rem.find('?') {
        Some(i) => (rem[..i].to_string(), rem[i + 1..].to_string()),
        None => (rem.to_string(), String::new()),
    };

    // 5) userinfo / host / port from netloc.
    let (userinfo, hostinfo) = match netloc.rfind('@') {
        Some(at) => (Some(&netloc[..at]), netloc[at + 1..].to_string()),
        None => (None, netloc.clone()),
    };
    let (username, password) = match userinfo {
        Some(ui) => match ui.split_once(':') {
            Some((u, p)) => (Some(u.to_string()), Some(p.to_string())),
            None => (Some(ui.to_string()), None),
        },
        None => (None, None),
    };

    let (hostname, port_str) = split_host_port(&hostinfo);
    let port = if port_str.is_empty() {
        None
    } else {
        port_str.parse::<u16>().ok()
    };

    UrlSplit {
        scheme,
        netloc,
        path,
        query,
        fragment: fragment.to_string(),
        username,
        password,
        hostname,
        port_str,
        port,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> UrlSplit {
        parse_url(s)
    }

    #[test]
    fn https_with_query_and_fragment() {
        let x = u("https://193.176.84.16:9002?sni=193.176.84.16&allowInsecure=1#RO%20-%20zhuhai.uk");
        assert_eq!(x.scheme, "https");
        assert_eq!(x.netloc, "193.176.84.16:9002");
        assert_eq!(x.hostname.as_deref(), Some("193.176.84.16"));
        assert_eq!(x.port, Some(9002));
        assert_eq!(x.query, "sni=193.176.84.16&allowInsecure=1");
        assert_eq!(x.fragment, "RO%20-%20zhuhai.uk");
        assert_eq!(x.username, None);
    }

    #[test]
    fn http_with_null_userinfo() {
        let x = u("https://null:None@84.17.47.146:9002?sni=84.17.47.146&allowInsecure=1#NL");
        assert_eq!(x.username.as_deref(), Some("null"));
        assert_eq!(x.password.as_deref(), Some("None"));
        assert_eq!(x.hostname.as_deref(), Some("84.17.47.146"));
        assert_eq!(x.port, Some(9002));
    }

    #[test]
    fn vless_uuid_userinfo() {
        let x = u("vless://8fb65472-5957-4f64-ba2d-b5812b2f155a@157.137.235.114:62145?type=tcp&security=none#CO%20-%20zhuhai.uk");
        assert_eq!(x.scheme, "vless");
        assert_eq!(x.username.as_deref(), Some("8fb65472-5957-4f64-ba2d-b5812b2f155a"));
        assert_eq!(x.hostname.as_deref(), Some("157.137.235.114"));
        assert_eq!(x.port, Some(62145));
        assert_eq!(x.query, "type=tcp&security=none");
    }

    #[test]
    fn ss_base64_userinfo() {
        let x = u("ss://YWVzLTEyOC1nY206NjZiMTgzMDgtMTI4Yi00ODE3LThhZGMtMGFkZmIwM2YzMTI0@k6p9tz.5tencent.asia:40029#KR");
        assert_eq!(x.netloc, "YWVzLTEyOC1nY206NjZiMTgzMDgtMTI4Yi00ODE3LThhZGMtMGFkZmIwM2YzMTI0@k6p9tz.5tencent.asia:40029");
        assert_eq!(x.username.as_deref(), Some("YWVzLTEyOC1nY206NjZiMTgzMDgtMTI4Yi00ODE3LThhZGMtMGFkZmIwM2YzMTI0"));
    }

    #[test]
    fn port_invalid_drops() {
        // "0" parses to port 0 (valid, like Python urlsplit.port -> 0).
        assert_eq!(u("https://host:0").port, Some(0));
        assert_eq!(u("https://host:0").port_str.as_str(), "0");
        // non-numeric port is None here; parse_node drops such nodes.
        assert_eq!(u("https://host:abc").port, None);
    }
}
