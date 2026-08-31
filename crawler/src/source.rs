//! Subscription sources and the harvest step (mirrors `freebox/sources.py`).
use crate::fetch::http_get;
use crate::parse::decode_subscription;
use crate::parse::extract_urls;
use std::collections::HashSet;

/// How a source's URL is resolved before its subscription body is decoded.
#[derive(Clone, Copy)]
pub enum SourceKind {
    /// Direct subscription URL — fetch the URL and decode the response body
    /// (a base64 blob or plain URL-per-line text).
    Direct,
    /// Indirect, page-listing source: fetch `url` (a blog index page), find the
    /// first link matching `article_filter`, fetch that article page, find the
    /// first link matching `sub_filter` (the actual subscription URL), then
    /// fetch and decode it.  e.g. `https://nodefree.me/` → latest article →
    /// v2ray `.txt` subscription.
    Indirect {
        article_filter: fn(&str) -> bool,
        sub_filter: fn(&str) -> bool,
    },
}

/// A subscription source: name, URL, per-source fetch timeout (seconds), and
/// resolution [`SourceKind`].
pub struct Source {
    pub name: String,
    pub url: String,
    pub timeout: u64,
    pub kind: SourceKind,
}

impl Source {
    /// Fetch + decode this source into a list of raw proxy URLs.
    pub fn fetch(&self) -> Vec<String> {
        match self.kind {
            SourceKind::Direct => match http_get(&self.url, self.timeout) {
                Ok(text) => decode_subscription(&text),
                Err(e) => {
                    eprintln!("[crawler] {} fetch failed: {}", self.name, e);
                    Vec::new()
                }
            },
            SourceKind::Indirect {
                article_filter,
                sub_filter,
            } => self.fetch_indirect(article_filter, sub_filter),
        }
    }

    /// Two-stage resolution for page-listing sources (e.g. nodefree.me):
    /// index page → article page → subscription URL → decode.
    fn fetch_indirect(
        &self,
        article_filter: fn(&str) -> bool,
        sub_filter: fn(&str) -> bool,
    ) -> Vec<String> {
        // 1) Fetch the listing page (e.g. https://nodefree.me/).
        let page = match http_get(&self.url, self.timeout) {
            Ok(html) => html,
            Err(e) => {
                eprintln!("[crawler] {} page fetch failed: {}", self.name, e);
                return Vec::new();
            }
        };

        // 2) Find the first article link on the listing page.
        let article_url = match extract_urls(&page)
            .into_iter()
            .find(|u| article_filter(u))
        {
            Some(u) => u,
            None => {
                eprintln!(
                    "[crawler] {} no article link found on page",
                    self.name
                );
                return Vec::new();
            }
        };
        eprintln!("[crawler] {} article: {}", self.name, article_url);

        // 3) Fetch the article page.
        let article = match http_get(&article_url, self.timeout) {
            Ok(html) => html,
            Err(e) => {
                eprintln!(
                    "[crawler] {} article fetch failed: {}",
                    self.name, e
                );
                return Vec::new();
            }
        };

        // 4) Find the first subscription link on the article page.
        let sub_url = match extract_urls(&article)
            .into_iter()
            .find(|u| sub_filter(u))
        {
            Some(u) => u,
            None => {
                eprintln!(
                    "[crawler] {} no subscription link found on article",
                    self.name
                );
                return Vec::new();
            }
        };
        eprintln!("[crawler] {} resolved sub: {}", self.name, sub_url);

        // 5) Fetch and decode the subscription.
        match http_get(&sub_url, self.timeout) {
            Ok(text) => decode_subscription(&text),
            Err(e) => {
                eprintln!(
                    "[crawler] {} subscription fetch failed: {}",
                    self.name, e
                );
                Vec::new()
            }
        }
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
// nodefree.me
// ---------------------------------------------------------------------------

/// On the nodefree.me index page, article links look like
/// `https://nodefree.me/p/3678.html` (listed newest-first).  Match the path
/// segment `/p/` followed by digits and `.html`, which naturally excludes feed
/// URLs (`…/p/3678.html/feed`) and query-string variants (`…/?p=3678`).
///
/// The first match is the latest article.
pub fn nodefree_article_filter(url: &str) -> bool {
    url.contains("nodefree.me/p/") && url.ends_with(".html")
}

/// On the article page, the actual subscription links point to the
/// `node.nodefree.me` sub-domain (v2ray `.txt`, Clash/Mihomo `.yaml`).
/// The first match is the v2ray `.txt` subscription, which
/// `decode_subscription` can decode into raw proxy URLs.
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
            article_filter: nodefree_article_filter,
            sub_filter: nodefree_sub_filter,
        },
    }
}

/// The default sources.  We list **two** URLs for the same content — the
/// jsDelivr CDN mirror (reachable from behind the GFW) and the canonical
/// `raw.githubusercontent.com` (reachable from CI / GitHub Pages).  `harvest`
/// de-duplicates globally, so listing both only adds a fallback: if the CDN
/// is down the raw URL still yields the nodes, and vice-versa.
///
/// `nodefree.me` is an **indirect** page-listing source: its index page links
/// to the latest article, which links to a subscription URL that is fetched
/// and decoded on every crawl.
pub fn default_sources() -> Vec<Source> {
    vec![
        Source {
            name: "zhuhai-cdn".to_string(),
            url: "https://cdn.jsdelivr.net/gh/zhuhaiuk/free-nodes@main/nodes.txt"
                .to_string(),
            timeout: 25,
            kind: SourceKind::Direct,
        },
        Source {
            name: "zhuhai-raw".to_string(),
            url: "https://raw.githubusercontent.com/zhuhaiuk/free-nodes/main/nodes.txt"
                .to_string(),
            timeout: 25,
            kind: SourceKind::Direct,
        },
        nodefree_source(),
    ]
}

/// Defaults + any `EXTRA_SUBS` overrides.
pub fn load_sources(extra_subs: &[String]) -> Vec<Source> {
    let mut sources = default_sources();
    for (i, sub) in extra_subs.iter().enumerate() {
        sources.push(Source {
            name: format!("extra-{i}"),
            url: sub.clone(),
            timeout: 25,
            kind: SourceKind::Direct,
        });
    }
    sources
}

/// Fetch every source, de-duplicating URLs globally while preserving order.
pub fn harvest(sources: &[Source]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for src in sources {
        let urls = src.fetch();
        eprintln!("[crawler] {}: {} raw URLs", src.name, urls.len());
        for u in urls {
            if seen.insert(u.clone()) {
                out.push(u);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!nodefree_article_filter("https://node.nodefree.me/2026/08/20260830.txt"));
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
        assert!(!nodefree_sub_filter("https://gravatar.loli.net/avatar/abc"));
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
}
