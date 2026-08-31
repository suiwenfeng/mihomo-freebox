//! `freebox` — a minimal, statically-linkable Rust fetcher for free proxy nodes.
//!
//! Port of `crawler/freebox` (Python). Harvests nodes from subscription
//! sources, parses them into a mihomo `proxy-providers` YAML, optionally TCP
//! probes them for reachability, and writes `providers/proxies.yaml`.

mod connectivity;
mod fetch;
mod parse;
mod source;
mod urlsplit;

use linked_hash_map::LinkedHashMap;
use std::env;
use std::fs;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};
use yaml_rust::{Yaml, YamlEmitter};

use parse::{get, get_i64, parse_urls, Proxy};
use source::{harvest, load_sources, parse_extra_subs};

const DEFAULT_OUT: &str = "providers/proxies.yaml";

fn main() -> ExitCode {
    let args = parse_args();
    let extra = parse_extra_subs(&env::var("EXTRA_SUBS").unwrap_or_default());
    let sources = load_sources(&extra);

    let raw = harvest(&sources);
    eprintln!("[crawler] {} raw URLs total", raw.len());
    if raw.is_empty() {
        eprintln!("[crawler] WARNING: no sources returned any raw URLs");
    }

    let proxies = parse_urls(&raw);
    eprintln!("[crawler] parsed {} unique proxies", proxies.len());

    let source_names: Vec<String> = sources.iter().map(|s| s.name.clone()).collect();

    // Score: --no-test keeps everyone at +inf (order-preserving); otherwise probe.
    let mut scored: Vec<(Proxy, f64)> = Vec::new();
    if args.no_test {
        for p in proxies {
            scored.push((p, 1e9));
        }
    } else {
        let total = proxies.len();
        for (i, p) in proxies.into_iter().enumerate() {
            let host = get(&p, "server").unwrap_or("").to_string();
            let port = get_i64(&p, "port").and_then(|v| u16::try_from(v).ok()).unwrap_or(0);
            match connectivity::tcp_latency(&host, port, args.timeout) {
                Some(lat) => {
                    eprintln!(
                        "[crawler] {}/{} {}:{} reachable {:.1}ms",
                        i + 1,
                        total,
                        host,
                        port,
                        lat
                    );
                    scored.push((p, lat));
                }
                None => {
                    eprintln!(
                        "[crawler] {}/{} drop unreachable {}:{:?}",
                        i + 1,
                        total,
                        host,
                        port
                    );
                }
            }
        }
        eprintln!(
            "[crawler] reachable: {}/{}",
            scored.len(),
            total
        );
    }

    // Stable sort by latency (ties keep first-seen order).
    scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut proxies: Vec<Proxy> = scored.into_iter().map(|(p, _)| p).collect();
    if args.max > 0 {
        proxies.truncate(args.max);
    }

    let yaml = emit(&proxies, &source_names.join(", "));
    fs::write(&args.out, &yaml).expect("failed to write output");
    eprintln!(
        "[crawler] wrote {} proxies to {}",
        proxies.len(),
        args.out
    );
    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

struct Args {
    out: String,
    max: usize,
    timeout: f64,
    no_test: bool,
}

fn parse_args() -> Args {
    let argv: Vec<String> = env::args().skip(1).collect();
    let mut out = env::var("OUT_FILE").as_deref().unwrap_or(DEFAULT_OUT).to_string();
    let mut max = env::var("MAX_NODES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut timeout = env::var("PROBE_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(3.5);
    let mut no_test = env::var("SKIP_TEST").as_deref().unwrap_or("") == "1";

    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        let (key, attached) = match arg.find('=') {
            Some(eq) => (arg[..eq].to_string(), Some(arg[eq + 1..].to_string())),
            None => (arg.clone(), None),
        };
        match key.as_str() {
            "--out" => out = take_value(attached, &argv, &mut i),
            "--max" => {
                max = take_value(attached, &argv, &mut i)
                    .parse::<usize>()
                    .unwrap_or(0)
            }
            "--timeout" => {
                timeout = take_value(attached, &argv, &mut i)
                    .parse::<f64>()
                    .unwrap_or(3.5)
            }
            "--no-test" => {
                // bare flag, or --no-test=true/false
                no_test = !matches!(
                    attached.as_deref(),
                    Some("false") | Some("0") | Some("no")
                );
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => {
                eprintln!("[crawler] unknown argument: {other}");
                print_usage();
                std::process::exit(2);
            }
        }
        i += 1;
    }

    Args { out, max, timeout, no_test }
}

/// Value for `--key=value` (attached) or the next argv element (space form).
fn take_value(attached: Option<String>, argv: &[String], i: &mut usize) -> String {
    if let Some(v) = attached {
        v
    } else {
        *i += 1;
        argv.get(*i).cloned().unwrap_or_default()
    }
}

fn print_usage() {
    eprintln!("usage: freebox [--out PATH] [--max N] [--timeout SECS] [--no-test]");
    eprintln!();
    eprintln!("Env: EXTRA_SUBS, MAX_NODES (default 0), PROBE_TIMEOUT (default 3.5),");
    eprintln!("     SKIP_TEST=1, OUT_FILE.");
}

// ---------------------------------------------------------------------------
// YAML emission
// ---------------------------------------------------------------------------

fn emit(proxies: &[Proxy], source_names: &str) -> String {
    let mut seq: Vec<Yaml> = Vec::with_capacity(proxies.len());
    for p in proxies {
        seq.push(Yaml::Hash(p.clone()));
    }
    let mut top: Proxy = LinkedHashMap::new();
    top.insert(Yaml::String("proxies".to_string()), Yaml::Array(seq));

    let mut body = String::new();
    YamlEmitter::new(&mut body)
        .dump(&Yaml::Hash(top))
        .expect("yaml emit failed");

    // yaml-rust prepends a "---" document marker; strip it so the file matches the
    // Python output style (bare `proxies:` list).
    let body = strip_doc_start(&body);

    let mut out = String::from(
        "# Auto-generated by freebox crawler. DO NOT EDIT BY HAND.\n",
    );
    out.push_str(&format!("# Updated: {}\n", now_iso()));
    out.push_str(&format!("# Sources: {}\n", source_names));
    out.push_str(&body);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn strip_doc_start(body: &str) -> &str {
    if let Some(rest) = body.strip_prefix("---") {
        rest.strip_prefix('\n').unwrap_or(rest)
    } else {
        body
    }
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// `2024-01-01T00:00:00Z`-style UTC timestamp, no std-time / chrono dependency.
#[test]
#[ignore] // set RAW_URLS=/abs/path RAW_OUT=/abs/path.yaml to run
fn parse_raw_urls_file() {
    let in_path = std::env::var("RAW_URLS").unwrap_or_default();
    if in_path.is_empty() || !std::path::Path::new(&in_path).exists() {
        return;
    }
    let text = std::fs::read_to_string(&in_path).unwrap();
    let urls: Vec<String> = text
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let proxies = parse_urls(&urls);
    eprintln!("[verify] parsed {} proxies from {}", proxies.len(), in_path);
    let yaml = emit(&proxies, "raw_urls");
    let out_path =
        std::env::var("RAW_OUT").unwrap_or_else(|_| "/tmp/rust_from_raw.yaml".to_string());
    std::fs::write(&out_path, &yaml).unwrap();
    eprintln!("[verify] wrote {} proxies to {}", proxies.len(), out_path);
}

fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86400;
    let t = secs % 86400;
    let (y, m, d) = civil_from_days(days as i64);
    let h = t / 3600;
    let mi = (t % 3600) / 60;
    let se = t % 60;
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{se:02}Z")
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_known_dates() {
        assert_eq!(civil_from_days(19723), (2024, 1, 1)); // 2024-01-01
        assert_eq!(civil_from_days(19782), (2024, 2, 29)); // leap day
        assert_eq!(civil_from_days(19358), (2023, 1, 1)); // 2023-01-01
    }

    #[test]
    fn now_iso_format() {
        let iso = now_iso();
        assert_eq!(iso.len(), 20);
        assert!(iso.ends_with('Z'));
        assert!(iso.chars().nth(4) == Some('-'));
    }
}
