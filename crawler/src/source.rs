//! Subscription sources and the harvest step (mirrors `freebox/sources.py`).
//!
//! A [`Source`] describes one subscription endpoint: its name, URL, fetch
//! timeout, resolution kind ([`SourceKind`]), and whether it should be fetched
//! through the local mihomo SOCKS5 proxy (`via_proxy`).
//!
//! ## Source kinds
//! - [`SourceKind::Direct`]: fetch the URL and decode the body (base64 blob or
//!   plain URL-per-line text) into proxy URLs.
//! - [`SourceKind::Indirect`]: a page-listing source — fetch a listing page,
//!   follow a chain of [`Step`]s (each finding the first URL matching a
//!   [`Predicate`]), and decode the final page's content.  This generalizes
//!   the original `nodefree.me` two-step pattern (index → article → sub) to
//!   any number of hops and any predicate, so new blog-style sources can be
//!   added declaratively without writing custom Rust filter functions.
//!
//! ## Configuration
//! Sources can be defined in [`config/sources.yaml`](crate::SOURCES_CONFIG)
//! (declarative, no recompile needed).  If that file is missing or invalid the
//! hardcoded [`default_sources`] are used as a fallback.

use crate::parse::decode_subscription;
use crate::parse::extract_urls;
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

/// Re-export so callers can build a fetcher with proxy auto-detection.
pub use crate::fetch::{auto_detect_proxy, SubFetcher};

/// A URL-matching predicate for indirect source discovery.
///
/// Predicates are composable: [`Predicate::All`] matches when every inner
/// predicate matches, [`Predicate::Any`] matches when at least one does.
#[derive(Debug, Clone)]
pub enum Predicate {
    /// The URL contains this substring.
    Contains(String),
    /// The URL ends with this suffix.
    EndsWith(String),
    /// The URL starts with this prefix.
    StartsWith(String),
    /// All of the inner predicates must match.
    All(Vec<Predicate>),
    /// At least one of the inner predicates must match.
    Any(Vec<Predicate>),
}

impl Predicate {
    /// Returns `true` when `url` satisfies this predicate.
    pub fn matches(&self, url: &str) -> bool {
        match self {
            Predicate::Contains(s) => url.contains(s),
            Predicate::EndsWith(s) => url.ends_with(s),
            Predicate::StartsWith(s) => url.starts_with(s),
            Predicate::All(preds) => preds.iter().all(|p| p.matches(url)),
            Predicate::Any(preds) => preds.iter().any(|p| p.matches(url)),
        }
    }
}

/// One hop in an indirect source's page chain: extract all URLs from the
/// current page, keep the first one that matches [`Step::predicate`], and
/// fetch that URL next.
#[derive(Debug, Clone)]
pub struct Step {
    pub predicate: Predicate,
    /// Human-readable description (for log messages).
    pub label: String,
}

impl Step {
    pub fn new(predicate: Predicate, label: &str) -> Self {
        Step {
            predicate,
            label: label.to_string(),
        }
    }
}

/// How a source's subscription content is resolved.
#[derive(Debug, Clone)]
pub enum SourceKind {
    /// Direct subscription URL — fetch the URL and decode the response body.
    Direct,
    /// Indirect, page-listing source: fetch `url` (a blog index page), find the
    /// first link matching step 1's predicate, fetch that page, find the first
    /// link matching step 2's predicate, and so on — the final page's content
    /// is the subscription body that gets decoded.  e.g.
    /// `https://nodefree.me/` → article → subscription URL → decode.
    Indirect {
        steps: Vec<Step>,
    },
}

/// A subscription source.
#[derive(Debug, Clone)]
pub struct Source {
    pub name: String,
    pub url: String,
    pub timeout: u64,
    pub kind: SourceKind,
    /// When `true`, fetch this source through the local mihomo SOCKS5 proxy,
    /// skipping the direct attempt.  Use this for sources known to be
    /// GFW-blocked.  When `false`, the [`SubFetcher`] still falls back to the
    /// proxy if a direct fetch fails and a proxy is available.
    pub via_proxy: bool,
}

impl Source {
    /// Fetch + decode this source into a list of raw proxy URLs.
    pub fn fetch(&self, fetcher: &SubFetcher) -> Vec<String> {
        match &self.kind {
            SourceKind::Direct => {
                // If the URL contains a {date} placeholder, try today first
                // then fall back to the previous few days (for date-stamped
                // subscription files like aiboboxx/v2rayfree/v20260907).
                if self.url.contains("{date}") {
                    self.fetch_with_date_fallback(fetcher)
                } else {
                    match fetcher_get(fetcher, &self.url, self.timeout, self.via_proxy) {
                        Ok(text) => decode_subscription(&text),
                        Err(e) => {
                            eprintln!("[crawler] {} fetch failed: {}", self.name, e);
                            Vec::new()
                        }
                    }
                }
            }
            SourceKind::Indirect { steps } => {
                self.fetch_indirect(fetcher, steps)
            }
        }
    }

    /// For date-stamped direct sources: try `{date}` = today, then the previous
    /// 3 days.  Returns the first successful decode.
    fn fetch_with_date_fallback(&self, fetcher: &SubFetcher) -> Vec<String> {
        for offset in 0..=3 {
            let date_str = date_str_days(offset);
            let url = self.url.replace("{date}", &date_str);
            match fetcher_get(fetcher, &url, self.timeout, self.via_proxy) {
                Ok(text) => {
                    let urls = decode_subscription(&text);
                    if !urls.is_empty() {
                        return urls;
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[crawler] {} (date={}) fetch failed: {}",
                        self.name, date_str, e
                    );
                }
            }
        }
        Vec::new()
    }


    /// Multi-hop resolution for page-listing sources.
    fn fetch_indirect(&self, fetcher: &SubFetcher, steps: &[Step]) -> Vec<String> {
        // 1) Fetch the listing page.
        let mut current_url = self.url.clone();
        let mut content = match fetcher_get(fetcher, &current_url, self.timeout, self.via_proxy) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[crawler] {} page fetch failed: {}", self.name, e);
                return Vec::new();
            }
        };

        // 2) Walk each discovery step: find a matching URL, fetch it.
        for step in steps {
            let next_url = extract_urls(&content)
                .into_iter()
                .find(|u| step.predicate.matches(u));

            match next_url {
                Some(u) => {
                    current_url = u;
                    eprintln!("[crawler] {} {}: {}", self.name, step.label, current_url);
                    match fetcher_get(fetcher, &current_url, self.timeout, self.via_proxy) {
                        Ok(c) => content = c,
                        Err(e) => {
                            eprintln!(
                                "[crawler] {} {} \"{}\" fetch failed: {}",
                                self.name, step.label, current_url, e
                            );
                            return Vec::new();
                        }
                    }
                }
                None => {
                    eprintln!(
                        "[crawler] {} no link matching '{}' found",
                        self.name, step.label
                    );
                    return Vec::new();
                }
            }
        }

        // 3) The final page's content is the subscription body.
        decode_subscription(&content)
    }
}

/// Internal helper: fetch a URL with the appropriate proxy strategy.
fn fetcher_get(
    fetcher: &SubFetcher,
    url: &str,
    timeout: u64,
    via_proxy: bool,
) -> Result<String, String> {
    if via_proxy {
        fetcher.get_via_proxy(url, timeout)
    } else {
        fetcher.get(url, timeout)
    }
}

/// Parse the `EXTRA_SUBS` environment variable (comma- or newline-separated).
pub fn parse_extra_subs(env: &str) -> Vec<String> {
    env.replace(',', "\n")
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// Date helpers (no chrono dependency — pure stdlib, mirrors main.rs)
// ---------------------------------------------------------------------------

/// Howard Hinnant's `days_from_civil` inverse: civil date from days since
/// 1970-01-01. Returns (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// Today's date as `YYYYMMDD` (UTC).
#[allow(dead_code)]
pub fn date_str() -> String {
    date_str_days(0)
}

/// Date string `YYYYMMDD` for `days_ago` days before today (UTC).
pub fn date_str_days(days_ago: u64) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86400;
    let (y, m, d) = civil_from_days((days as i64) - (days_ago as i64));
    format!("{y:04}{m:02}{d:02}")
}

// ---------------------------------------------------------------------------
// nodefree.me predicates (kept as named constants for the default sources)
// ---------------------------------------------------------------------------

/// On the nodefree.me index page, article links look like
/// `https://nodefree.me/p/3678.html` (listed newest-first).
#[allow(dead_code)]
pub fn nodefree_article_filter(url: &str) -> bool {
    url.contains("nodefree.me/p/") && url.ends_with(".html")
}

/// On the article page, the actual subscription links point to the
/// `node.nodefree.me` sub-domain.
#[allow(dead_code)]
pub fn nodefree_sub_filter(url: &str) -> bool {
    url.contains("node.nodefree.me")
}

/// The nodefree.me default source (page-listing → article → subscription).
pub fn nodefree_source() -> Source {
    Source {
        name: "nodefree".to_string(),
        url: "https://nodefree.me/".to_string(),
        timeout: 25,
        kind: SourceKind::Indirect {
            steps: vec![
                Step::new(
                    Predicate::all(vec![
                        Predicate::contains("nodefree.me/p/"),
                        Predicate::ends_with(".html"),
                    ]),
                    "article link",
                ),
                Step::new(
                    Predicate::contains("node.nodefree.me"),
                    "subscription URL",
                ),
            ],
        },
        via_proxy: false,
    }
}

// ---------------------------------------------------------------------------
// Default source definitions
// ---------------------------------------------------------------------------

/// Convenience constructor: `Predicate::All([Contains, EndsWith])`.
impl Predicate {
    /// Helper: `url.contains(s)`
    #[allow(dead_code)]
    pub fn contains(s: &str) -> Self {
        Predicate::Contains(s.to_string())
    }
    /// Helper: `url.ends_with(s)`
    pub fn ends_with(s: &str) -> Self {
        Predicate::EndsWith(s.to_string())
    }
    /// Helper: `url.starts_with(s)`
    #[allow(dead_code)]
    pub fn starts_with(s: &str) -> Self {
        Predicate::StartsWith(s.to_string())
    }
    /// Helper: OR of multiple predicates.
    #[allow(dead_code)]
    pub fn any(preds: Vec<Predicate>) -> Self {
        Predicate::Any(preds)
    }
    /// Helper: AND of multiple predicates.
    pub fn all(preds: Vec<Predicate>) -> Self {
        Predicate::All(preds)
    }
}

/// The default sources.  These mirror the original Python crawler's source
/// set, extended with additional subscriptions discovered through research.
///
/// Sources are listed in priority order: the de-duplication in [`harvest`]
/// keeps the first occurrence of each URL, so earlier sources win.
pub fn default_sources() -> Vec<Source> {
    vec![
        // ── zhuhaiuk/free-nodes (reliable from most environments) ─────────
        Source {
            name: "zhuhai-cdn".to_string(),
            url: "https://cdn.jsdelivr.net/gh/zhuhaiuk/free-nodes@main/nodes.txt".to_string(),
            timeout: 25,
            kind: SourceKind::Direct,
            via_proxy: false,
        },
        Source {
            name: "zhuhai-raw".to_string(),
            url: "https://raw.githubusercontent.com/zhuhaiuk/free-nodes/main/nodes.txt".to_string(),
            timeout: 25,
            kind: SourceKind::Direct,
            via_proxy: false,
        },
        // zhuhai extra nodes (base64-encoded, distinct node set)
        Source {
            name: "zhuhai-extra-cdn".to_string(),
            url: "https://cdn.jsdelivr.net/gh/zhuhaiuk/free-nodes@main/extra_nodes.txt".to_string(),
            timeout: 25,
            kind: SourceKind::Direct,
            via_proxy: false,
        },
        Source {
            name: "zhuhai-extra-raw".to_string(),
            url: "https://raw.githubusercontent.com/zhuhaiuk/free-nodes/main/extra_nodes.txt"
                .to_string(),
            timeout: 25,
            kind: SourceKind::Direct,
            via_proxy: false,
        },
        // ── nodefree.me (indirect: index → article → subscription) ───────
        nodefree_source(),
        // ── shuaidaoya FreeNodes (Gist base64 subscription) ────────────────
        Source {
            name: "shuaidaoya-base64".to_string(),
            url: "https://gist.githubusercontent.com/shuaidaoya/9e5cf2749c0ce79932dd9229d9b4162b/raw/base64.txt".to_string(),
            timeout: 25,
            kind: SourceKind::Direct,
            via_proxy: false,
        },
        // ── Pawdroid Free-servers (base64 subscription) ────────────────────
        Source {
            name: "pawdroid-sub".to_string(),
            url: "https://raw.githubusercontent.com/Pawdroid/Free-servers/master/sub".to_string(),
            timeout: 25,
            kind: SourceKind::Direct,
            via_proxy: false,
        },
        // ── ermaozi get_subscribe (plain URL-per-line) ────────────────────
        Source {
            name: "ermaozi-v2ray".to_string(),
            url: "https://raw.githubusercontent.com/ermaozi/get_subscribe/main/subscribe/v2ray.txt"
                .to_string(),
            timeout: 25,
            kind: SourceKind::Direct,
            via_proxy: false,
        },
        // ── learnhard-cn free proxy ss (base64-encoded lines) ─────────────
        Source {
            name: "learnhard-ss".to_string(),
            url: "https://raw.githubusercontent.com/learnhard-cn/free_proxy_ss/main/free".to_string(),
            timeout: 25,
            kind: SourceKind::Direct,
            via_proxy: false,
        },
        // ── aiboboxx/v2rayfree (date-stamped base64 subscription) ──────────
        // {date} is replaced with today's YYYYMMDD; the crawler tries today
        // then the previous 3 days for robustness.
        Source {
            name: "aiboboxx-v2rayfree".to_string(),
            url: "https://raw.githubusercontent.com/aiboboxx/v2rayfree/main/v{date}".to_string(),
            timeout: 15,
            kind: SourceKind::Direct,
            via_proxy: false,
        },
    ]
}

/// Defaults + any `EXTRA_SUBS` overrides + any sources loaded from the
/// config file.
///
/// `extra_subs` are appended as `Direct` (non-proxy) sources.  Pass an empty
/// slice when none are configured.
pub fn load_sources(extra_subs: &[String]) -> Vec<Source> {
    let mut sources = default_sources();
    for (i, sub) in extra_subs.iter().enumerate() {
        sources.push(Source {
            name: format!("extra-{i}"),
            url: sub.clone(),
            timeout: 25,
            kind: SourceKind::Direct,
            via_proxy: false,
        });
    }
    sources
}

/// Fetch every source, de-duplicating URLs globally while preserving order.
pub fn harvest(sources: &[Source], fetcher: &SubFetcher) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for src in sources {
        let urls = src.fetch(fetcher);
        eprintln!("[crawler] {}: {} raw URLs", src.name, urls.len());
        for u in urls {
            if seen.insert(u.clone()) {
                out.push(u);
            }
        }
    }
    out
}

/// Path to the optional declarative sources config file.
pub const SOURCES_CONFIG: &str = "config/sources.yaml";

/// Load sources from the YAML config file at `path`.
///
/// Returns `None` if the file doesn't exist or can't be parsed.
/// When `path` is empty, uses the [`SOURCES_CONFIG`] default.
pub fn load_sources_from_config(path: &str) -> Option<Vec<Source>> {
    let p = if path.is_empty() {
        std::path::Path::new(SOURCES_CONFIG)
    } else {
        std::path::Path::new(path)
    };
    if !p.exists() {
        return None;
    }
    let text = std::fs::read_to_string(p).ok()?;
    parse_sources_yaml(&text).ok()
}

/// Parse a sources YAML document into [`Source`] definitions.
///
/// Expected format:
/// ```yaml
/// proxy: "127.0.0.1:7890"        # optional; overrides proxy auto-detection
/// sources:
///   - name: my-source
///     url: "https://example.com/sub.txt"
///     timeout: 25                # optional, default 25
///     via_proxy: false          # optional, default false
///     kind: direct              # optional, default "direct"
///     # for indirect:
///     discovery:
///       - label: "article"
///         predicate:
///           all:
///             - contains: "nodefree.me/p/"
///             - ends_with: ".html"
///       - label: "subscription"
///         predicate:
///           contains: "node.nodefree.me"
/// ```
pub fn parse_sources_yaml(text: &str) -> Result<Vec<Source>, String> {
    use yaml_rust::YamlLoader;
    let docs = YamlLoader::load_from_str(text).map_err(|e| format!("yaml parse: {e}"))?;
    if docs.is_empty() {
        return Err("empty config".to_string());
    }
    let doc = &docs[docs.len() - 1]; // last doc if multiple
    let sources_yaml = doc["sources"].as_vec().ok_or("missing 'sources' list")?;

    let mut sources = Vec::with_capacity(sources_yaml.len());
    for sy in sources_yaml {
        let name = sy["name"].as_str().ok_or("source missing 'name'")?.to_string();
        let url = sy["url"].as_str().ok_or("source missing 'url'")?.to_string();
        let timeout = sy["timeout"]
            .as_i64()
            .map(|v| v as u64)
            .unwrap_or(25);
        let via_proxy = sy["via_proxy"]
            .as_bool()
            .unwrap_or(false);
        let kind = sy["kind"].as_str().unwrap_or("direct");

        let kind = match kind {
            "direct" => SourceKind::Direct,
            "indirect" => {
                let steps_yaml = sy["discovery"].as_vec().ok_or("indirect source missing 'discovery'")?;
                let mut steps = Vec::with_capacity(steps_yaml.len());
                for sy_step in steps_yaml {
                    let label = sy_step["label"].as_str().unwrap_or("step").to_string();
                    let pred = parse_predicate(&sy_step["predicate"])
                        .ok_or(format!("invalid predicate for step '{label}'"))?;
                    steps.push(Step::new(pred, &label));
                }
                SourceKind::Indirect { steps }
            }
            other => return Err(format!("unknown source kind: {other}")),
        };

        sources.push(Source {
            name,
            url,
            timeout,
            kind,
            via_proxy,
        });
    }
    Ok(sources)
}

/// Parse a ``predicate`` YAML mapping into a [`Predicate`].
fn parse_predicate(node: &yaml_rust::Yaml) -> Option<Predicate> {
    use yaml_rust::Yaml;
    match node {
        Yaml::Hash(h) => {
            // Check for composite predicates first.
            if let Some(all) = h.get(&Yaml::String("all".to_string())).and_then(|v| v.as_vec()) {
                let preds: Vec<Predicate> = all
                    .iter()
                    .filter_map(parse_predicate)
                    .collect();
                if !preds.is_empty() {
                    return Some(Predicate::All(preds));
                }
            }
            if let Some(any) = h.get(&Yaml::String("any".to_string())).and_then(|v| v.as_vec()) {
                let preds: Vec<Predicate> = any
                    .iter()
                    .filter_map(parse_predicate)
                    .collect();
                if !preds.is_empty() {
                    return Some(Predicate::Any(preds));
                }
            }
            // Leaf predicates.
            if let Some(s) = h.get(&Yaml::String("contains".to_string())).and_then(|v| v.as_str()) {
                return Some(Predicate::Contains(s.to_string()));
            }
            if let Some(s) = h.get(&Yaml::String("ends_with".to_string())).and_then(|v| v.as_str())
            {
                return Some(Predicate::EndsWith(s.to_string()));
            }
            if let Some(s) = h.get(&Yaml::String("starts_with".to_string())).and_then(|v| v.as_str())
            {
                return Some(Predicate::StartsWith(s.to_string()));
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Predicate tests
    // -----------------------------------------------------------------------

    #[test]
    fn predicate_contains() {
        assert!(Predicate::contains("node.nodefree.me").matches(
            "https://node.nodefree.me/2026/09/20260905.txt"
        ));
        assert!(!Predicate::contains("node.nodefree.me").matches(
            "https://nodefree.me/p/3672.html"
        ));
    }

    #[test]
    fn predicate_ends_with() {
        assert!(Predicate::ends_with(".html").matches("https://nodefree.me/p/3672.html"));
        assert!(!Predicate::ends_with(".html").matches("https://nodefree.me/p/3672.txt"));
    }

    #[test]
    fn predicate_starts_with() {
        assert!(Predicate::starts_with("https://").matches("https://example.com"));
        assert!(!Predicate::starts_with("https://").matches("http://example.com"));
    }

    #[test]
    fn predicate_all() {
        let p = Predicate::all(vec![
            Predicate::contains("nodefree.me/p/"),
            Predicate::ends_with(".html"),
        ]);
        assert!(p.matches("https://nodefree.me/p/3672.html"));
        // feed URL — should NOT match (doesn't end with .html)
        assert!(!p.matches("https://nodefree.me/p/3672.html/feed"));
        // non-article
        assert!(!p.matches("https://nodefree.me/f/freenode"));
        // wrong domain
        assert!(!p.matches("https://node.nodefree.me/2026/09/20260905.txt"));
    }

    #[test]
    fn predicate_any() {
        let p = Predicate::any(vec![
            Predicate::contains("node.nodefree.me"),
            Predicate::ends_with(".yaml"),
        ]);
        assert!(p.matches("https://node.nodefree.me/2026/09/20260905.txt"));
        assert!(p.matches("https://example.com/sub.yaml"));
        assert!(!p.matches("https://nodefree.me/p/3672.html"));
    }

    #[test]
    fn predicate_nested() {
        // (contains "nodefree.me/p/" AND ends_with ".html") OR (contains "node.nodefree.me")
        let p = Predicate::Any(vec![
            Predicate::All(vec![
                Predicate::contains("nodefree.me/p/"),
                Predicate::ends_with(".html"),
            ]),
            Predicate::contains("node.nodefree.me"),
        ]);
        assert!(p.matches("https://nodefree.me/p/3672.html"));
        assert!(p.matches("https://node.nodefree.me/2026/09/20260905.txt"));
        assert!(!p.matches("https://nodefree.me/p/3672.html/feed"));
    }

    // -----------------------------------------------------------------------
    // Backward-compatible filter tests
    // -----------------------------------------------------------------------

    #[test]
    fn article_filter_matches() {
        assert!(nodefree_article_filter("https://nodefree.me/p/3678.html"));
        assert!(nodefree_article_filter("https://nodefree.me/p/3685.html"));
        // feed URL — should NOT match
        assert!(!nodefree_article_filter("https://nodefree.me/p/3678.html/feed"));
        // query-string variant — should NOT match
        assert!(!nodefree_article_filter("https://nodefree.me/?p=3678"));
        // navigation, not an article
        assert!(!nodefree_article_filter("https://nodefree.me/f/freenode"));
        // the subscription URL lives on the *article* page, not the index
        assert!(!nodefree_article_filter(
            "https://node.nodefree.me/2026/08/20260830.txt"
        ));
    }

    #[test]
    fn sub_filter_matches() {
        assert!(nodefree_sub_filter(
            "https://node.nodefree.me/2026/08/20260830.txt"
        ));
        assert!(nodefree_sub_filter(
            "https://node.nodefree.me/2026/08/20260830.yaml"
        ));
        // site-internal link, not a subscription
        assert!(!nodefree_sub_filter("https://nodefree.me/p/3678.html"));
        // gravatar / other domains
        assert!(!nodefree_sub_filter(
            "https://gravatar.loli.net/avatar/abc"
        ));
    }

    #[test]
    fn extract_urls_finds_links_in_html() {
        // A realistic slice of the nodefree.me index page.
        let html = r#"
            <a href="https://nodefree.me/p/3678.html">article</a>
            <img src="https://nodefree.me/wp-content/uploads/2023/01/30-480x300.jpg">
            <p>https://node.nodefree.me/2026/08/20260830.txt</p>
        "#;
        let urls = extract_urls(html);
        assert!(urls.contains(&"https://nodefree.me/p/3678.html".to_string()));
        // img src also picked up by extract_urls
        assert!(urls
            .iter()
            .any(|u| u.contains("480x300.jpg")));

        // Article filter picks only the article link.
        let article = urls
            .iter()
            .find(|u| nodefree_article_filter(u))
            .cloned();
        assert_eq!(
            article.as_deref(),
            Some("https://nodefree.me/p/3678.html")
        );

        // Sub filter picks only the subscription URL.
        let sub = urls.iter().find(|u| nodefree_sub_filter(u)).cloned();
        assert_eq!(
            sub.as_deref(),
            Some("https://node.nodefree.me/2026/08/20260830.txt")
        );
    }

    // -----------------------------------------------------------------------
    // YAML config parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_direct_source_yaml() {
        let yaml = r#"
sources:
  - name: test-direct
    url: "https://example.com/sub.txt"
    timeout: 30
    via_proxy: true
    kind: direct
"#;
        let sources = parse_sources_yaml(yaml).unwrap();
        assert_eq!(sources.len(), 1);
        let s = &sources[0];
        assert_eq!(s.name, "test-direct");
        assert_eq!(s.url, "https://example.com/sub.txt");
        assert_eq!(s.timeout, 30);
        assert!(s.via_proxy);
        assert!(matches!(s.kind, SourceKind::Direct));
    }

    #[test]
    fn parse_indirect_source_yaml() {
        let yaml = r#"
sources:
  - name: test-indirect
    url: "https://nodefree.me/"
    kind: indirect
    discovery:
      - label: "article"
        predicate:
          all:
            - contains: "nodefree.me/p/"
            - ends_with: ".html"
      - label: "subscription"
        predicate:
          contains: "node.nodefree.me"
"#;
        let sources = parse_sources_yaml(yaml).unwrap();
        assert_eq!(sources.len(), 1);
        let s = &sources[0];
        match &s.kind {
            SourceKind::Indirect { steps } => {
                assert_eq!(steps.len(), 2);
                assert_eq!(steps[0].label, "article");
                assert_eq!(steps[1].label, "subscription");
                // Verify the predicates work
                let article_pred = &steps[0].predicate;
                assert!(article_pred.matches("https://nodefree.me/p/3672.html"));
                assert!(!article_pred.matches("https://nodefree.me/p/3672.html/feed"));
                let sub_pred = &steps[1].predicate;
                assert!(sub_pred.matches("https://node.nodefree.me/2026/09/20260905.txt"));
            }
            _ => panic!("expected Indirect kind"),
        }
    }

    #[test]
    fn parse_predicate_contains() {
        let yaml = r#"
sources:
  - name: test
    url: "https://example.com"
    kind: indirect
    discovery:
      - predicate:
          contains: "foo"
"#;
        let sources = parse_sources_yaml(yaml).unwrap();
        let s = &sources[0];
        match &s.kind {
            SourceKind::Indirect { steps } => {
                assert!(steps[0].predicate.matches("https://foo.example.com"));
                assert!(!steps[0].predicate.matches("https://bar.example.com"));
            }
            _ => panic!("expected Indirect"),
        }
    }

    #[test]
    fn parse_predicate_ends_with() {
        let yaml = r#"
sources:
  - name: test
    url: "https://example.com"
    kind: indirect
    discovery:
      - predicate:
          ends_with: ".txt"
"#;
        let sources = parse_sources_yaml(yaml).unwrap();
        let s = &sources[0];
        match &s.kind {
            SourceKind::Indirect { steps } => {
                assert!(steps[0].predicate.matches("https://example.com/sub.txt"));
                assert!(!steps[0].predicate.matches("https://example.com/sub.yaml"));
            }
            _ => panic!("expected Indirect"),
        }
    }

    #[test]
    fn parse_predicate_any() {
        let yaml = r#"
sources:
  - name: test
    url: "https://example.com"
    kind: indirect
    discovery:
      - predicate:
          any:
            - contains: "foo"
            - ends_with: ".yaml"
"#;
        let sources = parse_sources_yaml(yaml).unwrap();
        let s = &sources[0];
        match &s.kind {
            SourceKind::Indirect { steps } => {
                assert!(steps[0].predicate.matches("https://foo.com"));
                assert!(steps[0].predicate.matches("https://x.com/sub.yaml"));
                assert!(!steps[0].predicate.matches("https://bar.com/sub.txt"));
            }
            _ => panic!("expected Indirect"),
        }
    }

    #[test]
    fn parse_predicate_all() {
        let yaml = r#"
sources:
  - name: test
    url: "https://example.com"
    kind: indirect
    discovery:
      - predicate:
          all:
            - contains: "nodefree.me/p/"
            - ends_with: ".html"
"#;
        let sources = parse_sources_yaml(yaml).unwrap();
        let s = &sources[0];
        match &s.kind {
            SourceKind::Indirect { steps } => {
                assert!(steps[0].predicate.matches("https://nodefree.me/p/3672.html"));
                assert!(!steps[0].predicate.matches("https://nodefree.me/p/3672.html/feed"));
                assert!(!steps[0].predicate.matches("https://nodefree.me/f/freenode"));
            }
            _ => panic!("expected Indirect"),
        }
    }

    #[test]
    fn date_str_format() {
        // Should be YYYYMMDD (8 digits).
        let ds = date_str();
        assert_eq!(ds.len(), 8);
        assert!(ds.chars().all(|c| c.is_ascii_digit()));

        // date_str_days(0) == today, date_str_days(1) == yesterday.
        assert_eq!(date_str_days(0), date_str());
    }

    #[test]
    fn date_url_interpolation() {
        // The aiboboxx URL uses {date} as a placeholder.
        let url = "https://raw.githubusercontent.com/aiboboxx/v2rayfree/main/v{date}";
        let today = date_str();
        let interpolated = url.replace("{date}", &today);
        assert_eq!(
            interpolated,
            format!(
                "https://raw.githubusercontent.com/aiboboxx/v2rayfree/main/v{today}"
            )
        );
        assert!(interpolated.contains("v2026"));
    }
}
