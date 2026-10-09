//! `open-model-gateway healthcheck`: probe the gateway's own readiness endpoint
//! without a shell or curl (the runtime image is distroless).
//!
//! A deliberately tiny HTTP/1.1 client: one `GET`, plain `http://` only, no
//! proxy (proxy environment variables are never read), no redirects, no
//! retries, loopback destinations only unless `--allow-non-loopback` is given.
//! Healthy means a `2xx` status within the timeout; anything else, including a
//! redirect, is unhealthy. Reads no configuration, secrets or `.env` files.

use std::{
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail, ensure};

pub const READY_PATH: &str = "/health/ready";
pub const DEFAULT_TIMEOUT: &str = "3s";
const MAX_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_STATUS_LINE: usize = 1024;

/// Where to probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    host: String,
    port: u16,
    path: String,
}

impl Target {
    /// Parse a plain `http://host[:port][/path]` URL.
    pub fn parse(url: &str) -> Result<Self> {
        let rest = url.strip_prefix("http://").ok_or_else(|| {
            anyhow!("only http:// URLs are supported (probe the gateway's own listener)")
        })?;
        ensure!(
            rest.bytes().all(|b| b.is_ascii_graphic()),
            "the URL must not contain spaces or control characters"
        );
        ensure!(!rest.contains('#'), "the URL must not contain a fragment");
        let (authority, path) = match rest.find(['/', '?']) {
            Some(index) => rest.split_at(index),
            None => (rest, "/"),
        };
        let path = if path.starts_with('?') {
            format!("/{path}")
        } else {
            path.to_owned()
        };
        ensure!(
            !authority.contains('@'),
            "the URL must not contain credentials"
        );
        let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
            let (host, after) = bracketed
                .split_once(']')
                .ok_or_else(|| anyhow!("invalid IPv6 address in the URL"))?;
            host.parse::<Ipv6Addr>()
                .map_err(|_| anyhow!("invalid IPv6 address in the URL"))?;
            match after {
                "" => (host, None),
                port => (
                    host,
                    Some(
                        port.strip_prefix(':')
                            .ok_or_else(|| anyhow!("invalid URL authority"))?,
                    ),
                ),
            }
        } else {
            match authority.split_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority, None),
            }
        };
        ensure!(!host.is_empty(), "the URL needs a host");
        let port = match port {
            None => 80,
            Some(port) => port
                .parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| anyhow!("invalid port in the URL"))?,
        };
        Ok(Self {
            host: host.to_owned(),
            port,
            path,
        })
    }

    /// The default target: `/health/ready` on the port of `GATEWAY_LISTEN`
    /// (default `127.0.0.1:8080`), via loopback when it listens on all
    /// addresses.
    pub fn from_listen(listen: Option<&str>) -> Result<Self> {
        let listen: SocketAddr = listen
            .unwrap_or("127.0.0.1:8080")
            .parse()
            .context("GATEWAY_LISTEN must be an IP address and port")?;
        let ip = match listen.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
            ip if ip.is_loopback() => ip,
            _ => bail!(
                "GATEWAY_LISTEN is neither loopback nor all addresses; pass --url (and --allow-non-loopback)"
            ),
        };
        Ok(Self {
            host: ip.to_string(),
            port: listen.port(),
            path: READY_PATH.to_owned(),
        })
    }

    fn host_header(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    fn addresses(&self, allow_non_loopback: bool) -> Result<Vec<SocketAddr>> {
        let addresses: Vec<SocketAddr> = match self.host.parse::<IpAddr>() {
            Ok(ip) => vec![SocketAddr::new(ip, self.port)],
            Err(_) => (self.host.as_str(), self.port)
                .to_socket_addrs()
                .context("could not resolve the health check host")?
                .collect(),
        };
        ensure!(
            !addresses.is_empty(),
            "the health check host has no address"
        );
        ensure!(
            allow_non_loopback || addresses.iter().all(|a| a.ip().is_loopback()),
            "refusing a non-loopback health check destination without --allow-non-loopback"
        );
        Ok(addresses)
    }
}

/// Parse `500ms`, `3s` or a bare number of seconds (1ms to 60s).
pub fn parse_timeout(value: &str) -> Result<Duration> {
    let invalid = || anyhow!("--timeout must look like 3s or 500ms (at most 60s)");
    let duration = if let Some(ms) = value.strip_suffix("ms") {
        Duration::from_millis(ms.parse().map_err(|_| invalid())?)
    } else {
        let seconds = value.strip_suffix('s').unwrap_or(value);
        Duration::from_secs(seconds.parse().map_err(|_| invalid())?)
    };
    ensure!(
        !duration.is_zero() && duration <= MAX_TIMEOUT,
        "--timeout must be between 1ms and 60s"
    );
    Ok(duration)
}

/// Probe once. Returns the `2xx` status, or an error describing why the
/// gateway is not healthy.
pub fn probe(target: &Target, timeout: Duration, allow_non_loopback: bool) -> Result<u16> {
    let deadline = Instant::now() + timeout;
    let remaining = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| anyhow!("timed out after {timeout:?}"))
    };
    let mut last_error = None;
    let mut stream = None;
    for address in target.addresses(allow_non_loopback)? {
        match TcpStream::connect_timeout(&address, remaining()?) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }
    let mut stream = stream.ok_or_else(|| {
        anyhow!(
            "could not connect to {}: {}",
            target.host_header(),
            last_error.map_or_else(|| "no address".to_owned(), |e| e.to_string())
        )
    })?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: open-model-gateway-healthcheck\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        target.path,
        target.host_header()
    );
    stream.set_write_timeout(Some(remaining()?))?;
    stream
        .write_all(request.as_bytes())
        .context("could not send the health check request")?;
    let mut response = Vec::with_capacity(256);
    let mut buffer = [0u8; 256];
    while !response.windows(2).any(|w| w == b"\r\n") {
        ensure!(
            response.len() < MAX_STATUS_LINE,
            "invalid HTTP response (status line too long)"
        );
        stream.set_read_timeout(Some(remaining()?))?;
        let read = stream.read(&mut buffer).map_err(|error| {
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) {
                anyhow!("timed out after {timeout:?}")
            } else {
                anyhow!("could not read the health check response: {error}")
            }
        })?;
        ensure!(read > 0, "connection closed before an HTTP status line");
        response.extend_from_slice(&buffer[..read]);
    }
    let status = parse_status_line(&response)?;
    ensure!(
        (200..300).contains(&status),
        "unhealthy: {} returned HTTP {status}",
        target.path
    );
    Ok(status)
}

fn parse_status_line(response: &[u8]) -> Result<u16> {
    let line = response
        .split(|b| *b == b'\n')
        .next()
        .and_then(|line| std::str::from_utf8(line).ok())
        .map(|line| line.trim_end_matches('\r'))
        .ok_or_else(|| anyhow!("invalid HTTP response"))?;
    let mut parts = line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    let code = parts.next().unwrap_or_default();
    ensure!(
        version.starts_with("HTTP/1.") && code.len() == 3,
        "invalid HTTP response"
    );
    code.parse()
        .map_err(|_| anyhow!("invalid HTTP response status"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn parses_plain_http_urls() {
        assert_eq!(
            Target::parse("http://127.0.0.1:8080/health/ready").unwrap(),
            Target {
                host: "127.0.0.1".into(),
                port: 8080,
                path: "/health/ready".into()
            }
        );
        let v6 = Target::parse("http://[::1]:9000").unwrap();
        assert_eq!(
            (v6.host.as_str(), v6.port, v6.path.as_str()),
            ("::1", 9000, "/")
        );
        assert_eq!(v6.host_header(), "[::1]:9000");
        assert_eq!(Target::parse("http://localhost").unwrap().port, 80);
        assert_eq!(Target::parse("http://localhost?x=1").unwrap().path, "/?x=1");
        for invalid in [
            "https://127.0.0.1/health/ready",
            "127.0.0.1:8080",
            "http://user:pass@127.0.0.1/",
            "http://127.0.0.1:0/",
            "http://127.0.0.1:99999/",
            "http:///health",
            "http://127.0.0.1/a b",
            "http://127.0.0.1/a\r\nX-Injected: 1",
            "http://127.0.0.1/#frag",
            "http://[::1/",
            "http://[nothost]:80/",
        ] {
            assert!(Target::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn default_target_follows_gateway_listen_over_loopback() {
        let target = Target::from_listen(None).unwrap();
        assert_eq!(target.host_header(), "127.0.0.1:8080");
        assert_eq!(target.path, READY_PATH);
        assert_eq!(
            Target::from_listen(Some("0.0.0.0:9090"))
                .unwrap()
                .host_header(),
            "127.0.0.1:9090"
        );
        assert_eq!(
            Target::from_listen(Some("[::]:8080"))
                .unwrap()
                .host_header(),
            "[::1]:8080"
        );
        assert!(Target::from_listen(Some("10.0.0.5:8080")).is_err());
        assert!(Target::from_listen(Some("nonsense")).is_err());
    }

    #[test]
    fn refuses_non_loopback_destinations_by_default() {
        let target = Target::parse("http://192.0.2.1:8080/health/ready").unwrap();
        let error = probe(&target, Duration::from_millis(200), false).unwrap_err();
        assert!(error.to_string().contains("non-loopback"), "{error}");
    }

    #[test]
    fn timeouts_are_bounded() {
        assert_eq!(parse_timeout("3s").unwrap(), Duration::from_secs(3));
        assert_eq!(parse_timeout("3").unwrap(), Duration::from_secs(3));
        assert_eq!(parse_timeout("250ms").unwrap(), Duration::from_millis(250));
        for invalid in ["0s", "0ms", "61s", "-1s", "3m", "", "s", "1.5s"] {
            assert!(parse_timeout(invalid).is_err(), "{invalid}");
        }
    }

    fn serve_once(response: &'static [u8]) -> (Target, std::thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 512];
            while !request.ends_with(b"\r\n\r\n") {
                let n = stream.read(&mut buffer).unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..n]);
            }
            if !response.is_empty() {
                stream.write_all(response).unwrap();
            }
            request
        });
        let target = Target::parse(&format!("http://127.0.0.1:{port}/health/ready")).unwrap();
        (target, handle)
    }

    #[test]
    fn healthy_on_2xx_and_sends_a_minimal_request() {
        let (target, server) = serve_once(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok");
        assert_eq!(probe(&target, Duration::from_secs(2), false).unwrap(), 200);
        let request = String::from_utf8(server.join().unwrap()).unwrap();
        assert!(request.starts_with("GET /health/ready HTTP/1.1\r\nHost: 127.0.0.1:"));
        assert!(request.contains("Connection: close\r\n"));
    }

    #[test]
    fn unhealthy_on_errors_redirects_and_garbage() {
        for response in [
            &b"HTTP/1.1 503 Service Unavailable\r\n\r\n"[..],
            b"HTTP/1.1 302 Found\r\nlocation: http://example.com/\r\n\r\n",
            b"SSH-2.0-OpenSSH\r\n",
        ] {
            let (target, server) = serve_once(response);
            assert!(probe(&target, Duration::from_secs(2), false).is_err());
            server.join().unwrap();
        }
    }

    #[test]
    fn unhealthy_when_nothing_listens_or_the_server_stalls() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let target = Target::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        assert!(probe(&target, Duration::from_secs(2), false).is_err());

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let target = Target::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let started = Instant::now();
        let error = probe(&target, Duration::from_millis(300), false).unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(listener);
    }
}
