//! Address resolution (PLAN.md §4).
//!
//! Accepts `host:port`, `[v6]:port`, or a bare host/IP (to which the network's
//! default port is appended). Returns the first resolved `SocketAddr`.

use std::net::{SocketAddr, ToSocketAddrs};

use anyhow::{anyhow, Context, Result};

/// Resolve a peer specification to a single socket address.
///
/// `default_port` is applied when the spec carries no port. Resolution (for
/// hostnames) uses the system resolver and returns the first result.
pub fn resolve(spec: &str, default_port: u16) -> Result<SocketAddr> {
    let with_port = ensure_port(spec, default_port)?;
    with_port
        .to_socket_addrs()
        .with_context(|| format!("resolving {with_port:?}"))?
        .next()
        .ok_or_else(|| anyhow!("no address found for {spec:?}"))
}

/// Append `default_port` unless the spec already specifies one. Handles bracketed
/// IPv6 (`[::1]:8333`) and bare IPv6 (`::1`, which must be bracketed to carry a port).
fn ensure_port(spec: &str, default_port: u16) -> Result<String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(anyhow!("empty peer address"));
    }

    // Bracketed IPv6, with or without a port.
    if let Some(rest) = spec.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((inner, "")) => Ok(format!("[{inner}]:{default_port}")),
            Some((_, tail)) if tail.starts_with(':') => Ok(spec.to_string()),
            _ => Err(anyhow!("malformed bracketed address: {spec:?}")),
        };
    }

    // A bare IPv6 literal (two or more colons) has no port; it must be bracketed.
    if spec.matches(':').count() >= 2 {
        return Ok(format!("[{spec}]:{default_port}"));
    }

    // IPv4 or hostname: a single colon means a port is present.
    if spec.contains(':') {
        Ok(spec.to_string())
    } else {
        Ok(format!("{spec}:{default_port}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_default_port() {
        assert_eq!(ensure_port("127.0.0.1", 8333).unwrap(), "127.0.0.1:8333");
        assert_eq!(ensure_port("node.example", 18444).unwrap(), "node.example:18444");
    }

    #[test]
    fn keeps_explicit_port() {
        assert_eq!(ensure_port("127.0.0.1:1234", 8333).unwrap(), "127.0.0.1:1234");
    }

    #[test]
    fn bracketed_v6() {
        assert_eq!(ensure_port("[::1]", 8333).unwrap(), "[::1]:8333");
        assert_eq!(ensure_port("[::1]:9", 8333).unwrap(), "[::1]:9");
        assert_eq!(ensure_port("::1", 8333).unwrap(), "[::1]:8333");
    }

    #[test]
    fn resolves_localhost_v4() {
        let a = resolve("127.0.0.1:18444", 8333).unwrap();
        assert_eq!(a.port(), 18444);
        assert!(a.is_ipv4());
    }
}
