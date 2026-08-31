//! Proxy-URL -> Clash.Meta `proxies` parsers.
//!
//! A faithful port of the Python crawler's `freebox/parse.py`. Function names
//! and semantics are kept identical so the Rust and Python crawlers emit the
//! same provider file from the same input.
use base64::engine::general_purpose;
use base64::Engine;
use linked_hash_map::LinkedHashMap;
use std::collections::{HashMap, HashSet};
use yaml_rust::Yaml;

use crate::urlsplit::UrlSplit;

/// A single proxy entry as an insertion-ordered YAML mapping. Key order matches
/// the Python crawler (mihomo reads keys by name, so order is cosmetic, but
/// keeping it avoids gratuitous diff churn).
pub type Proxy = LinkedHashMap<Yaml, Yaml>;

fn s(v: &str) -> Yaml {
    Yaml::String(v.to_string())
}
fn b(v: bool) -> Yaml {
    Yaml::Boolean(v)
}
fn n(v: i64) -> Yaml {
    Yaml::Integer(v)
}
fn seq(list: Vec<String>) -> Yaml {
    Yaml::Array(list.into_iter().map(|x| Yaml::String(x)).collect())
}

/// Look up a string field in a proxy mapping.
pub fn get<'a>(proxy: &'a Proxy, key: &str) -> Option<&'a str> {
    proxy.get(&Yaml::String(key.to_string()))?.as_str()
}

/// Look up an integer field (e.g. `port`) in a proxy mapping.
pub fn get_i64(proxy: &Proxy, key: &str) -> Option<i64> {
    proxy.get(&Yaml::String(key.to_string()))?.as_i64()
}

// ---------------------------------------------------------------------------
// base64 / percent-decoding / name cleaning
// ---------------------------------------------------------------------------

/// Decode base64 that may be missing padding / using a URL-safe alphabet,
/// discarding characters outside the alphabet (mirrors Python
/// `base64.b64decode(..., validate=False)`), then retrying URL-safe.
pub fn b64decode_safe(raw: &str) -> Option<Vec<u8>> {
    let s = raw.trim().trim_end_matches('=');
    let pad = (4 - (s.len() % 4)) % 4;
    let mut padded = String::with_capacity(s.len() + pad);
    padded.push_str(s);
    padded.push_str(&"=".repeat(pad));

    let std_f: String = padded
        .chars()
        .filter(|c| matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '+' | '/' | '='))
        .collect();
    if let Ok(out) = general_purpose::STANDARD.decode(&std_f) {
        return Some(out);
    }

    let url_f: String = padded
        .chars()
        .filter(|c| matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '='))
        .collect();
    if let Ok(out) = general_purpose::URL_SAFE.decode(&url_f) {
        return Some(out);
    }

    None
}

/// Percent-decode a string (Python `urllib.parse.unquote`).
pub fn unquote(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = bytes[i + 1];
            let lo = bytes[i + 2];
            if let (Some(h), Some(l)) = (hexval(hi), hexval(lo)) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hexval(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Trim + collapse internal whitespace to single spaces (Python `_clean_name`).
pub fn clean_name(raw: &str) -> String {
    let n = unquote(raw).trim().to_string();
    let n = n.split_whitespace().collect::<Vec<_>>().join(" ");
    if n.is_empty() {
        "node".to_string()
    } else {
        n
    }
}

// ---------------------------------------------------------------------------
// Subscription / URL extraction
// ---------------------------------------------------------------------------

const SCHEMES: &[&str] = &["vmess", "vless", "trojan", "socks5", "ss", "http", "https"];

/// Find `scheme://` URLs in free text. Stops at the same delimiter set as the
/// Python regex (`[^\s"'<>\`]+`) and rstrips trailing commas.
pub fn extract_urls(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        if let Some(scheme) = match_scheme_at(&text[i..]) {
            let mut j = i + scheme.len() + 3; // past "scheme://"
            while j < n {
                let c = bytes[j];
                if is_url_stop(c) {
                    break;
                }
                j += 1;
            }
            let mut m = text[i..j].to_string();
            while m.ends_with(',') {
                m.pop();
            }
            out.push(m);
            i = j;
        } else {
            // Advance by one *character* (not one byte) so we always land on a
            // UTF-8 char boundary.  This matters for text containing multi-byte
            // characters — e.g. the CJK HTML on `nodefree.me` pages — where a
            // naive `i += 1` would split a multibyte sequence and make the next
            // `text[i..]` slice panic.
            let step = text[i..]
                .chars()
                .next()
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            i += step;
        }
    }
    out
}

fn match_scheme_at(s: &str) -> Option<&'static str> {
    for scheme in SCHEMES {
        if let Some(rest) = s.strip_prefix(*scheme) {
            if rest.starts_with("://") {
                return Some(scheme);
            }
        }
    }
    None
}

fn is_url_stop(c: u8) -> bool {
    c.is_ascii_whitespace() || matches!(c, b'"' | b'\'' | b'<' | b'>' | b'`')
}

/// True if `text` contains a recognizable proxy scheme prefix.
fn looks_like_urls(text: &str) -> bool {
    SCHEMES.iter().any(|p| text.contains(&format!("{p}://")))
}

/// Decode a subscription blob that may be one base64 block, or plain
/// URL-per-line text (mirrors `decode_subscription`).
pub fn decode_subscription(text: &str) -> Vec<String> {
    if let Some(decoded) = b64decode_safe(text.trim()) {
        if let Ok(candidate) = String::from_utf8(decoded) {
            if looks_like_urls(&candidate) {
                return extract_urls(&candidate);
            }
        }
    }
    split_proxies_text(text)
}

fn split_proxies_text(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(decoded) = b64decode_safe(line) {
            if let Ok(inner) = String::from_utf8(decoded) {
                if looks_like_urls(&inner) {
                    urls.extend(extract_urls(&inner));
                    continue;
                }
            }
        }
        urls.extend(extract_urls(line));
    }
    urls
}

// ---------------------------------------------------------------------------
// Query string parsing (mirrors freebox.parse._parse_query)
// ---------------------------------------------------------------------------

/// Parse `?a=1&b=2` into an insertion-ordered map with **lowercased** keys
/// and URL-decoded values; later occurrences of a key reuse its first position
/// (last value wins). Empty values are kept (`keep_blank_values=True`).
pub fn parse_qs(query: &str) -> LinkedHashMap<String, String> {
    let mut m: LinkedHashMap<String, String> = LinkedHashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        m.insert(k.to_ascii_lowercase(), unquote(v));
    }
    m
}

/// Borrowed view over a parsed query string.
#[derive(Clone, Copy)]
pub struct Q<'a>(&'a LinkedHashMap<String, String>);
impl<'a> Q<'a> {
    pub fn new(m: &'a LinkedHashMap<String, String>) -> Self {
        Q(m)
    }
    pub fn get(&self, k: &str) -> Option<&str> {
        self.0.get(k).map(|v| v.as_str())
    }
    pub fn has(&self, k: &str) -> bool {
        self.0.contains_key(k)
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// `name, server, port, type:""` — the common head of every proxy.
fn base_proxy(name: &str, url: &UrlSplit) -> Proxy {
    let mut m: Proxy = LinkedHashMap::new();
    m.insert(s("name"), s(&clean_name(name)));
    m.insert(s("server"), s(url.hostname.as_deref().unwrap_or("")));
    m.insert(s("port"), n(url.port.unwrap_or(0) as i64));
    m.insert(s("type"), s(""));
    m
}

fn ss_cipher(c: &str) -> &str {
    match c {
        "chacha20-ietf-poly1305" | "chacha20-ietf" | "aes-256-gcm" | "aes-128-gcm"
        | "rc4-md5" | "aes-256-cfb" | "aes-128-cfb" | "simple-obfs" | "http" => c,
        "http1" => "http",
        _ => c,
    }
}

/// Translate common stream params (type/host/path/alpn) onto a node.
fn stream_opts(q: Q, proxy: &mut Proxy) {
    let net = q.get("type").unwrap_or("tcp");
    match net {
        "ws" => {
            proxy.insert(s("ws"), b(true));
            let mut headers: Proxy = LinkedHashMap::new();
            if let Some(host) = q.get("host").or_else(|| q.get("sni")) {
                headers.insert(s("Host"), s(host));
            }
            let mut opts: Proxy = LinkedHashMap::new();
            opts.insert(s("headers"), Yaml::Hash(headers));
            let path = q.get("path").unwrap_or("/");
            if !path.is_empty() {
                opts.insert(s("path"), s(path));
            }
            proxy.insert(s("ws-opts"), Yaml::Hash(opts));
        }
        "http" => {
            proxy.insert(s("http"), b(true));
            let mut headers: Proxy = LinkedHashMap::new();
            if let Some(host) = q.get("host").or_else(|| q.get("sni")) {
                headers.insert(s("Host"), s(host));
            }
            let mut opts: Proxy = LinkedHashMap::new();
            opts.insert(s("headers"), Yaml::Hash(headers));
            let path = q.get("path").unwrap_or("/");
            if !path.is_empty() {
                opts.insert(s("path"), seq(path.split(',').map(|x| x.to_string()).collect()));
            }
            opts.insert(s("method"), s("GET"));
            proxy.insert(s("http-opts"), Yaml::Hash(opts));
        }
        "grpc" => {
            proxy.insert(s("grpc"), b(true));
            let mut opts: Proxy = LinkedHashMap::new();
            if let Some(sn) = q.get("sni").or_else(|| q.get("host")) {
                opts.insert(s("grpc-service-name"), s(sn));
            }
            if q.has("port") {
                opts.insert(s("grpc-mode"), s("Tcp"));
            }
            proxy.insert(s("grpc-opts"), Yaml::Hash(opts));
        }
        _ => {} // tcp / default
    }
}

/// TLS/Reality + stream options (mirrors `_tls_opts`).
fn tls_stream_opts(q: Q, proxy: &mut Proxy) {
    let security = q.get("security").unwrap_or("none");
    let flow = q.get("flow").filter(|v| !v.is_empty());
    let sni = q.get("sni").or_else(|| q.get("host"));

    if security == "tls" || security == "reality" {
        proxy.insert(s("tls"), b(true));
        let verif = match q.get("allowinsecure") {
            Some("1") => false,
            _ => true,
        };
        proxy.insert(s("tls-verification"), b(verif));
        proxy.insert(s("servername"), s(sni.unwrap_or("")));
        if let Some(alpn) = q.get("alpn") {
            proxy.insert(s("alpn"), seq(alpn.split(',').map(|x| x.to_string()).collect()));
        }
        if let Some(fl) = flow {
            proxy.insert(s("flow"), s(fl));
        }
        if security == "reality" {
            let mut r: Proxy = LinkedHashMap::new();
            // mihomo's *runtime* loader keys the reality public key off the
            // hyphenated `public-key` field (the v1.19.x YAML tag); emitting
            // the historical Clash.Meta spelling `publickey` alone makes mihomo
            // report "'reality-opts' has unset fields: public-key" and drop the
            // ENTIRE file provider (=> 0 nodes). We emit the hyphenated form
            // for mihomo plus the legacy spelling for other consumers of the
            // provider file; mihomo ignores the unrecognized extra key.
            let pbk = q.get("pbk").unwrap_or("");
            r.insert(s("public-key"), s(pbk));
            r.insert(s("publickey"), s(pbk));
            r.insert(s("short-id"), s(q.get("sid").unwrap_or("")));
            r.insert(s("servername"), s(sni.unwrap_or("")));
            proxy.insert(s("reality-opts"), Yaml::Hash(r));
        }
        if let Some(fp) = q.get("fp") {
            proxy.insert(s("client-fingerprint"), s(fp));
        }
    } else {
        proxy.insert(s("tls"), b(false));
    }
    stream_opts(q, proxy);
}

// ---------------------------------------------------------------------------
// Protocol parsers
// ---------------------------------------------------------------------------

fn parse_ss(url: &UrlSplit) -> Option<Proxy> {
    let mut work = url.netloc.clone();
    if !url.netloc.contains('@') {
        let userinfo = url.username.as_deref().unwrap_or("");
        if userinfo.is_empty() {
            let blob = b64decode_safe(&url.netloc)?;
            work = String::from_utf8_lossy(&blob).to_string();
            if !work.contains('@') {
                return None;
            }
        } else {
            return None; // Python: `return None if not userinfo else None`
        }
    }
    let (enc_user, hostinfo) = work.rsplit_once('@')?;
    let user = match b64decode_safe(enc_user) {
        Some(dec) => String::from_utf8_lossy(&dec).into_owned(),
        None => enc_user.to_string(),
    };
    let (cipher_raw, password) = match user.split_once(':') {
        Some((c, p)) => (unquote(c), unquote(p)),
        None => return None,
    };
    let (host, port) = match hostinfo.split_once(':') {
        Some((h, p)) => (h, p),
        None => (hostinfo, ""),
    };

    let mut proxy = base_proxy(&url.fragment, url);
    proxy.insert(s("type"), s("ss"));
    proxy.insert(s("cipher"), s(ss_cipher(&cipher_raw)));
    proxy.insert(s("password"), s(&password));
    proxy.insert(s("server"), s(host));
    let port_val = if port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty() {
        port.parse::<u16>().unwrap_or(0) as i64
    } else {
        0
    };
    proxy.insert(s("port"), n(port_val));

    // Shadowsocks plugins (simple-obfs / obfs-local).
    if cipher_raw == "simple-obfs" || cipher_raw == "obfs-local" {
        proxy.insert(s("plugin"), s("obfs"));
        let mut opts: Proxy = LinkedHashMap::new();
        opts.insert(s("mode"), s(&cipher_raw));
        proxy.insert(s("plugin-opts"), Yaml::Hash(opts));
    }
    Some(proxy)
}

fn parse_socks5(url: &UrlSplit) -> Option<Proxy> {
    let user = url.username.as_deref()?;
    if user.is_empty() {
        return None;
    }
    let mut proxy = base_proxy(&url.fragment, url);
    proxy.insert(s("type"), s("socks5"));
    let (u, p) = match user.split_once(':') {
        Some((u, p)) => (unquote(u), unquote(p)),
        None => (unquote(user), String::new()),
    };
    proxy.insert(s("username"), s(&u));
    proxy.insert(s("password"), s(&p));
    Some(proxy)
}

fn parse_vless(url: &UrlSplit) -> Option<Proxy> {
    let user = url.username.as_deref()?;
    if user.is_empty() {
        return None;
    }
    let q = parse_qs(&url.query);
    let mut proxy = base_proxy(&url.fragment, url);
    proxy.insert(s("type"), s("vless"));
    proxy.insert(s("uuid"), s(user));
    proxy.insert(s("udp"), b(true));
    tls_stream_opts(Q::new(&q), &mut proxy);
    Some(proxy)
}

fn parse_trojan(url: &UrlSplit) -> Option<Proxy> {
    let password = url.username.as_deref()?;
    if password.is_empty() {
        return None;
    }
    let q = parse_qs(&url.query);
    let mut proxy = base_proxy(&url.fragment, url);
    proxy.insert(s("type"), s("trojan"));
    proxy.insert(s("password"), s(&unquote(password)));
    proxy.insert(s("udp"), b(true));
    tls_stream_opts(Q::new(&q), &mut proxy);
    Some(proxy)
}

fn parse_http(url: &UrlSplit) -> Option<Proxy> {
    let userinfo = url.username.clone().unwrap_or_default();
    let parsed_q = parse_qs(&url.query);
    let q = Q::new(&parsed_q);
    let mut proxy = base_proxy(&url.fragment, url);
    proxy.insert(s("type"), s("http"));
    if !userinfo.is_empty() {
        let (u, p) = match userinfo.split_once(':') {
            Some((u, p)) => (unquote(u), unquote(p)),
            None => (unquote(&userinfo), String::new()),
        };
        proxy.insert(s("username"), s(&u));
        proxy.insert(s("password"), s(&p));
    }
    proxy.insert(s("udp"), b(true));
    if url.scheme == "https" {
        proxy.insert(s("tls"), b(true));
        let verif = match q.get("allowinsecure") {
            Some("1") => false,
            _ => true,
        };
        proxy.insert(s("tls-verification"), b(verif));
        proxy.insert(
            s("servername"),
            s(q.get("sni").or_else(|| q.get("host")).unwrap_or("")),
        );
        if let Some(alpn) = q.get("alpn") {
            proxy.insert(s("alpn"), seq(alpn.split(',').map(|x| x.to_string()).collect()));
        }
    }
    stream_opts(q, &mut proxy);
    Some(proxy)
}

/// Minimal JSON string/value lookup for a vmess body. vmess bodies are flat
/// objects with scalar (string/number/bool) values, so a full JSON parser isn't
/// needed — this keeps the dep set C-compiler-free.
fn vmess_get(obj: &str, key: &str) -> String {
    let needle = format!("\"{key}\"");
    let idx = match obj.find(&needle) {
        Some(i) => i,
        None => return String::new(),
    };
    let after = &obj[idx + needle.len()..];
    let colon = match after.find(':') {
        Some(c) => c,
        None => return String::new(),
    };
    let raw = after[colon + 1..].trim_start();
    if let Some(s) = raw.strip_prefix('"') {
        // JSON string value — consume through the closing (unescaped) quote.
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '"' {
                break;
            }
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(match n {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        '"' => '"',
                        '\\' => '\\',
                        '/' => '/',
                        other => other,
                    });
                }
            } else {
                out.push(c);
            }
        }
        out
    } else {
        // number / bool / null token up to the next structural delimiter.
        let end = raw
            .find(|c: char| c.is_whitespace() || matches!(c, ',' | '}' | ']'))
            .unwrap_or(raw.len());
        raw[..end].to_string()
    }
}

fn parse_vmess(url: &UrlSplit) -> Option<Proxy> {
    let body_src = if url.netloc.is_empty() {
        url.query.clone()
    } else if url.query.is_empty() {
        url.netloc.clone()
    } else {
        format!("{}?{}", url.netloc, url.query)
    };
    let body = b64decode_safe(&body_src)?;
    let data = String::from_utf8_lossy(&body);
    let server = vmess_get(&data, "add");
    let port = vmess_get(&data, "port");
    let uuid = vmess_get(&data, "id");
    let alter = vmess_get(&data, "aid");
    let cipher = vmess_get(&data, "scy");
    let tls_flag = vmess_get(&data, "tls");
    let sni = vmess_get(&data, "sni");
    let host = vmess_get(&data, "host");
    let alpn = vmess_get(&data, "alpn");
    let flow_v = vmess_get(&data, "flow");
    let net = vmess_get(&data, "net");
    let path_v = vmess_get(&data, "path");
    let fp = vmess_get(&data, "fp");

    let mut proxy = base_proxy(&url.fragment, url);
    proxy.insert(s("type"), s("vmess"));
    proxy.insert(s("uuid"), s(&uuid));
    proxy.insert(s("alterId"), n(alter.parse::<i64>().unwrap_or(0)));
    proxy.insert(s("cipher"), s(if cipher.is_empty() { "auto" } else { &cipher }));
    // vmess carries the address in the body: overwrite the (empty) urlsplit values.
    proxy.insert(s("server"), s(&server));
    proxy.insert(s("port"), n(port.parse::<i64>().unwrap_or(0)));
    proxy.insert(s("udp"), b(true));

    if tls_flag == "1" || tls_flag.eq_ignore_ascii_case("true") {
        proxy.insert(s("tls"), b(true));
        proxy.insert(s("servername"), s(if sni.is_empty() { &host } else { &sni }));
        if !alpn.is_empty() {
            proxy.insert(s("alpn"), seq(alpn.split(',').map(|x| x.to_string()).collect()));
        }
        if !fp.is_empty() {
            proxy.insert(s("client-fingerprint"), s(&fp));
        }
    }
    match net.as_str() {
        "ws" => {
            proxy.insert(s("ws"), b(true));
            let mut opts: Proxy = LinkedHashMap::new();
            opts.insert(
                s("headers"),
                Yaml::Hash({
                    let mut h: Proxy = LinkedHashMap::new();
                    h.insert(s("Host"), s(&host));
                    h
                }),
            );
            opts.insert(s("path"), s(if path_v.is_empty() { "/" } else { &path_v }));
            proxy.insert(s("ws-opts"), Yaml::Hash(opts));
        }
        "http" => {
            proxy.insert(s("http"), b(true));
            let mut opts: Proxy = LinkedHashMap::new();
            opts.insert(
                s("headers"),
                Yaml::Hash({
                    let mut h: Proxy = LinkedHashMap::new();
                    h.insert(s("Host"), s(&host));
                    h
                }),
            );
            opts.insert(
                s("path"),
                seq(vec![if path_v.is_empty() { "/".to_string() } else { path_v }]),
            );
            opts.insert(s("method"), s("GET"));
            proxy.insert(s("http-opts"), Yaml::Hash(opts));
        }
        "grpc" => {
            proxy.insert(s("grpc"), b(true));
            let mut opts: Proxy = LinkedHashMap::new();
            opts.insert(s("grpc-service-name"), s(&host));
            proxy.insert(s("grpc-opts"), Yaml::Hash(opts));
        }
        _ => {}
    }
    if !flow_v.is_empty() {
        proxy.insert(s("flow"), s(&flow_v));
    }
    Some(proxy)
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Parse a single proxy URL into a proxy mapping, or `None`.
pub fn parse_node(url: &str) -> Option<Proxy> {
    let parsed = crate::urlsplit::parse_url(url);
    let scheme = &parsed.scheme; // already lower-cased
    if scheme == "vmess" && parsed.netloc.is_empty() {
        return None;
    }
    // A non-numeric port would make urlsplit.port raise in Python -> node dropped.
    if !parsed.port_str.is_empty() && parsed.port.is_none() {
        return None;
    }
    let node = match scheme.as_str() {
        "ss" => parse_ss(&parsed),
        "socks5" => parse_socks5(&parsed),
        "vless" => parse_vless(&parsed),
        "vmess" => parse_vmess(&parsed),
        "trojan" => parse_trojan(&parsed),
        "http" | "https" => parse_http(&parsed),
        _ => None,
    }?;
    let server = get(&node, "server").unwrap_or("");
    if server.is_empty() {
        return None;
    }
    Some(node)
}

/// Parse many URLs, de-duplicating on (type, server, port) in first-seen order.
///
/// NOTE: mihomo's file-provider loader de-duplicates proxies by `name`
/// (keeping only the first occurrence of each name) — it does NOT merge or keep
/// duplicates. Several subscriptions (notably nodefree.me) emit several
/// *distinct* nodes that share a name (e.g. multiple `🇹🇷_TR_圧耷` with
/// different server/port). Left as-is, mihomo silently drops those extra
/// nodes (42 crawled -> 33 loaded, 9 lost). To preserve every distinct node we
/// disambiguate colliding names on first-seen with a ` 02`, ` 03`, ... suffix,
/// mirroring the `name 02` style already used by some upstream sources.
pub fn parse_urls(urls: &[String]) -> Vec<Proxy> {
    let mut proxies: Vec<Proxy> = Vec::new();
    let mut seen: HashSet<(String, String, i64)> = HashSet::new();
    let mut name_counts: HashMap<String, usize> = HashMap::new();
    for url in urls {
        if let Some(mut node) = parse_node(url) {
            let key = (
                get(&node, "type").unwrap_or("").to_string(),
                get(&node, "server").unwrap_or("").to_string(),
                get_i64(&node, "port").unwrap_or(0),
            );
            if seen.insert(key) {
                let base = get(&node, "name").unwrap_or("").to_string();
                let count = name_counts.entry(base.clone()).or_insert(0);
                *count += 1;
                if *count > 1 {
                    let nk = Yaml::String("name".to_string());
                    if let Some(name_yaml) = node.get_mut(&nk) {
                        if let Yaml::String(ref mut s) = *name_yaml {
                            *s = format!("{} {:02}", base, count);
                        }
                    }
                }
                proxies.push(node);
            }
        }
    }
    proxies
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_roundtrip() {
        let raw = b64decode_safe("YWRtaW46cGFzcw==").unwrap();
        assert_eq!(String::from_utf8(raw).unwrap(), "admin:pass");
    }

    #[test]
    fn unquote_basic() {
        assert_eq!(unquote("RO%20-%20zhuhai.uk"), "RO - zhuhai.uk");
    }

    #[test]
    fn extract_urls_http() {
        let u = extract_urls("https://1.2.3.4:9002?sni=1.2.3.4#n\n  extra");
        assert_eq!(u, vec!["https://1.2.3.4:9002?sni=1.2.3.4#n"]);
    }

    #[test]
    fn extract_urls_with_multibyte_text() {
        // CJK text (nodefree.me pages contain lots of it) must not split a
        // multi-byte UTF-8 sequence and cause a slicing panic.
        let html = "免费节点\nhttps://nodefree.me/p/3678.html\n订阅链接";
        let u = extract_urls(html);
        assert_eq!(u, vec!["https://nodefree.me/p/3678.html"]);
    }

    #[test]
    fn parse_http_node() {
        let node = parse_node(
            "https://193.176.84.16:9002?sni=193.176.84.16&allowInSecure=1#RO%20-%20zhuhai.uk",
        )
        .unwrap();
        assert_eq!(get(&node, "type"), Some("http"));
        assert_eq!(get(&node, "server"), Some("193.176.84.16"));
        assert_eq!(get_i64(&node, "port"), Some(9002));
        // tls-verification is a bool, read directly.
        assert_eq!(
            node.get(&Yaml::String("tls-verification".to_string()))
                .and_then(|v| v.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn reality_opts_emits_hyphenated_public_key() {
        // v1.19.x mihomo runtime keys the reality public key off `public-key`
        // (hyphen); the legacy `publickey` spelling is also emitted for other
        // consumers. If only `publickey` is present, mihomo rejects the whole
        // file provider with "unset fields: public-key" -> 0 nodes loaded.
        let node = parse_node(
            "vless://8fb65472-5957-4f64-ba2d-b5812b2f155a@1.2.3.4:443?type=tcp\
             &security=reality&sni=example.com&pbk=MyPubKey123&sid=abcdef0123456789\
             &flow=xtls-rprx-vision#TestNode",
        )
        .unwrap();
        let ropts = node
            .get(&Yaml::String("reality-opts".to_string()))
            .and_then(|v| v.as_hash())
            .expect("reality-opts present");
        let pubkey = ropts
            .get(&Yaml::String("public-key".to_string()))
            .and_then(|v| v.as_str())
            .expect("public-key (hyphenated) present for mihomo");
        assert_eq!(pubkey, "MyPubKey123");
        // legacy spelling preserved for non-mihomo consumers
        let legacy = ropts
            .get(&Yaml::String("publickey".to_string()))
            .and_then(|v| v.as_str());
        assert_eq!(legacy, Some("MyPubKey123"));
    }

    #[test]
    fn duplicate_names_are_disambiguated() {
        // Two distinct nodes (different host:port) that share a name: mihomo
        // loads file providers and dedups by name, so the crawler must make
        // names unique or the later node is silently dropped.
        let urls = vec![
            "https://1.1.1.1:443#dup".to_string(),
            "https://2.2.2.2:443#dup".to_string(),
            "https://3.3.3.3:443#unique".to_string(),
        ];
        let proxies = parse_urls(&urls);
        let names: Vec<&str> = proxies.iter().filter_map(|p| get(p, "name")).collect();
        assert_eq!(names, vec!["dup", "dup 02", "unique"]);
        // all distinct servers survived (none dropped by name collision)
        let servers: Vec<&str> = proxies.iter().filter_map(|p| get(p, "server")).collect();
        assert_eq!(servers, vec!["1.1.1.1", "2.2.2.2", "3.3.3.3"]);
    }
}
