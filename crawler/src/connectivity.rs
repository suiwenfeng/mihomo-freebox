//! TCP reachability probe (mirrors `freebox/connectivity.py:tcp_latency`).
//!
//! Opens a TCP connection to (host, port) with a timeout and measures the round
//! trip in milliseconds. Used to discard dead/unreachable free nodes before
//! they reach mihomo.
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// Connect to `host`:`port` within `timeout_secs`; return RTT in ms, or `None`.
/// Resolves hostnames first (like `socket.create_connection`) and tries each
/// resolved address in order until one connects.
pub fn tcp_latency(host: &str, port: u16, timeout_secs: f64) -> Option<f64> {
    if host.is_empty() || port == 0 {
        return None;
    }
    let timeout = Duration::from_secs_f64(timeout_secs);
    let addr = format!("{host}:{port}");
    let addrs = addr.to_socket_addrs().ok()?;
    for a in addrs {
        let start = Instant::now();
        if let Ok(stream) = TcpStream::connect_timeout(&a, timeout) {
            let ms = start.elapsed().as_secs_f64() * 1000.0;
            drop(stream);
            // Python: round(x, 1)
            return Some((ms * 10.0).round() / 10.0);
        }
    }
    None
}
