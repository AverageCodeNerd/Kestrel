//! A minimal DNS resolver: A records over UDP, no caching.

use alloc::vec::Vec;

use crate::net::{Ipv4, udp};

const PORT: u16 = 53;
const TYPE_A: u16 = 1;
const CLASS_IN: u16 = 1;

/// Build a query for `host`.
///
/// Names are encoded as a sequence of length-prefixed labels, terminated by a
/// zero length — "www.example.com" becomes 3www7example3com0.
fn build_query(host: &str, id: u16) -> Vec<u8> {
    let mut query = Vec::new();

    query.extend_from_slice(&id.to_be_bytes());
    query.extend_from_slice(&0x0100u16.to_be_bytes()); // standard query, recursion desired
    query.extend_from_slice(&1u16.to_be_bytes()); // one question
    query.extend_from_slice(&0u16.to_be_bytes()); // no answers
    query.extend_from_slice(&0u16.to_be_bytes()); // no authority records
    query.extend_from_slice(&0u16.to_be_bytes()); // no additional records

    for label in host.split('.').filter(|l| !l.is_empty()) {
        query.push(label.len() as u8);
        query.extend_from_slice(label.as_bytes());
    }
    query.push(0);

    query.extend_from_slice(&TYPE_A.to_be_bytes());
    query.extend_from_slice(&CLASS_IN.to_be_bytes());

    query
}

/// Step over a name, which may end in a pointer to earlier in the message.
fn skip_name(message: &[u8], mut offset: usize) -> Option<usize> {
    loop {
        let length = *message.get(offset)? as usize;

        // The top two bits set marks a compression pointer, which is always
        // the last thing in a name.
        if length & 0xC0 == 0xC0 {
            return Some(offset + 2);
        }
        if length == 0 {
            return Some(offset + 1);
        }

        offset += 1 + length;
    }
}

fn parse_answer(message: &[u8], want: u16) -> Option<Ipv4> {
    if message.len() < 12 {
        return None;
    }

    let id = u16::from_be_bytes([message[0], message[1]]);
    if id != want {
        return None;
    }

    // Low four bits of the flags are the response code; non-zero is an error.
    if message[3] & 0x0F != 0 {
        return None;
    }

    let questions = u16::from_be_bytes([message[4], message[5]]);
    let answers = u16::from_be_bytes([message[6], message[7]]);

    let mut offset = 12;
    for _ in 0..questions {
        offset = skip_name(message, offset)?;
        offset += 4; // type and class
    }

    for _ in 0..answers {
        offset = skip_name(message, offset)?;
        if offset + 10 > message.len() {
            return None;
        }

        let record_type = u16::from_be_bytes([message[offset], message[offset + 1]]);
        let length = u16::from_be_bytes([message[offset + 8], message[offset + 9]]) as usize;
        offset += 10;

        if offset + length > message.len() {
            return None;
        }

        // Skip CNAMEs and anything else; only an address answers the question.
        if record_type == TYPE_A && length == 4 {
            return Some(Ipv4::new(
                message[offset],
                message[offset + 1],
                message[offset + 2],
                message[offset + 3],
            ));
        }

        offset += length;
    }

    None
}

/// Resolve a hostname to an address.
pub fn resolve(host: &str, timeout_ms: u64) -> Result<Ipv4, &'static str> {
    // QEMU's user-mode network answers DNS at 10.0.2.3.
    let server = Ipv4::new(10, 0, 2, 3);

    let id = (crate::apic::ticks() as u16) | 1;
    let local_port = 40000 + (crate::apic::ticks() as u16 % 4096);
    let query = build_query(host, id);

    udp::send(server, local_port, PORT, &query)?;

    let deadline = crate::apic::ticks() + timeout_ms / 10;
    while crate::apic::ticks() < deadline {
        crate::net::poll();

        if let Some(datagram) = udp::take(local_port) {
            if let Some(address) = parse_answer(&datagram.payload, id) {
                return Ok(address);
            }
        }

        core::hint::spin_loop();
    }

    Err("no answer from the name server")
}
