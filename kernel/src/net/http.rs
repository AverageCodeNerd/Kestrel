//! Fetching a page over HTTP.
//!
//! This is a client, not a browser: it retrieves a document and hands back the
//! bytes. Nothing here parses HTML or lays anything out.

use alloc::string::String;
use alloc::vec::Vec;

use crate::net::{Ipv4, dns, tcp};

pub struct Response {
    pub address: Ipv4,
    pub status: String,
    pub body: Vec<u8>,
    pub total: usize,
}

/// Split a URL into host, port and path. Only http:// is understood, since
/// there is no TLS.
///
/// The port matters more than it looks: an update server or anything else run
/// locally is almost never on 80, and without this every such URL resolved the
/// whole `host:port` as a name and timed out looking it up.
pub fn split_url(url: &str) -> Result<(&str, u16, &str), &'static str> {
    let rest = url.strip_prefix("http://").unwrap_or(url);

    if rest.starts_with("https://") || url.starts_with("https://") {
        return Err("https needs TLS, which this stack does not have");
    }

    let (authority, path) = match rest.find('/') {
        Some(slash) => (&rest[..slash], &rest[slash..]),
        None => (rest, "/"),
    };

    // Split on the last colon so this keeps working if a host form with
    // colons in it ever turns up.
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse().map_err(|_| "that is not a port number")?,
        ),
        None => (authority, 80),
    };

    if host.is_empty() {
        return Err("the url has no host");
    }
    Ok((host, port, path))
}

/// Fetch a URL and return the response.
pub fn get(url: &str, timeout_ms: u64) -> Result<Response, &'static str> {
    let (host, port, path) = split_url(url)?;

    // A bare address skips the name server.
    let address = match parse_ipv4(host) {
        Some(address) => address,
        None => dns::resolve(host, timeout_ms)?,
    };

    tcp::connect(address, port, timeout_ms)?;

    // The Host header carries the port too when it is not the default, which
    // is what name-based virtual hosts expect.
    let authority = if port == 80 {
        alloc::string::String::from(host)
    } else {
        alloc::format!("{host}:{port}")
    };

    // HTTP/1.0 so the server closes when it is done, which is how the read
    // below knows the body is complete without parsing Content-Length.
    let request = alloc::format!(
        "GET {path} HTTP/1.0\r\nHost: {authority}\r\nUser-Agent: Kestrel\r\nConnection: close\r\n\r\n"
    );
    tcp::send(request.as_bytes())?;

    let raw = tcp::read_to_end(timeout_ms);
    tcp::close();

    if raw.is_empty() {
        return Err("no response");
    }

    let total = raw.len();

    // Headers end at the first blank line.
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .unwrap_or(raw.len());

    let headers = String::from_utf8_lossy(&raw[..split]).into_owned();
    let status = headers.lines().next().unwrap_or("").trim().into();

    Ok(Response {
        address,
        status,
        body: raw[split..].to_vec(),
        total,
    })
}

fn parse_ipv4(text: &str) -> Option<Ipv4> {
    let mut octets = [0u8; 4];
    let mut parts = text.split('.');

    for octet in &mut octets {
        *octet = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }

    Some(Ipv4::new(octets[0], octets[1], octets[2], octets[3]))
}
