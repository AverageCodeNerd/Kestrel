//! IPv4, and the ICMP echo that makes the machine answer ping.

use alloc::vec::Vec;

use crate::net::{ETHERTYPE_IPV4, Ipv4, arp};

pub const PROTOCOL_ICMP: u8 = 1;
pub const PROTOCOL_TCP: u8 = 6;
pub const PROTOCOL_UDP: u8 = 17;

/// A header with no options, which is all this stack produces.
const HEADER_SIZE: usize = 20;

/// The one's complement sum used by IP, ICMP, UDP and TCP alike.
pub fn checksum(parts: &[&[u8]]) -> u16 {
    let mut sum: u32 = 0;
    let mut odd: Option<u8> = None;

    for part in parts {
        let mut bytes = *part;

        // A leftover byte from the previous part pairs with the first of this
        // one; the sum is over 16-bit words, not over each slice separately.
        if let Some(high) = odd.take() {
            if let Some((&low, rest)) = bytes.split_first() {
                sum += u16::from_be_bytes([high, low]) as u32;
                bytes = rest;
            } else {
                odd = Some(high);
                continue;
            }
        }

        let mut chunks = bytes.chunks_exact(2);
        for chunk in &mut chunks {
            sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
        }
        if let [last] = chunks.remainder() {
            odd = Some(*last);
        }
    }

    if let Some(high) = odd {
        sum += u16::from_be_bytes([high, 0]) as u32;
    }

    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }

    !(sum as u16)
}

static NEXT_ID: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(1);

/// Wrap `payload` in an IPv4 header and send it to `destination`.
pub fn send(destination: Ipv4, protocol: u8, payload: &[u8]) -> Result<(), &'static str> {
    let total = HEADER_SIZE + payload.len();
    let mut packet = alloc::vec![0u8; total];

    packet[0] = 0x45; // version 4, 5 words of header
    packet[1] = 0; // no differentiated services
    packet[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    packet[4..6].copy_from_slice(
        &NEXT_ID
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed)
            .to_be_bytes(),
    );
    packet[6..8].copy_from_slice(&0x4000u16.to_be_bytes()); // don't fragment
    packet[8] = 64; // time to live
    packet[9] = protocol;
    packet[12..16].copy_from_slice(&crate::net::our_ip().octets());
    packet[16..20].copy_from_slice(&destination.octets());

    // The header checksum covers the header only, and is computed with its own
    // field zeroed — which it already is.
    let sum = checksum(&[&packet[..HEADER_SIZE]]);
    packet[10..12].copy_from_slice(&sum.to_be_bytes());

    packet[HEADER_SIZE..].copy_from_slice(payload);

    // Anything off this subnet goes via the gateway.
    let (gateway, netmask) = crate::net::config(|c| (c.gateway, c.netmask)).unwrap_or((
        Ipv4::UNSPECIFIED,
        Ipv4::new(255, 255, 255, 0),
    ));

    let next_hop = if same_subnet(destination, crate::net::our_ip(), netmask) {
        destination
    } else {
        gateway
    };

    // Resolving on demand, rather than failing and asking the caller to try
    // again: the first packet to any host would otherwise always be lost.
    let mac = arp::resolve(next_hop, 1000).ok_or("could not resolve the next hop")?;

    crate::net::send(mac, ETHERTYPE_IPV4, &packet)
}

fn same_subnet(a: Ipv4, b: Ipv4, mask: Ipv4) -> bool {
    a.octets()
        .iter()
        .zip(b.octets())
        .zip(mask.octets())
        .all(|((&x, y), m)| (x & m) == (y & m))
}

/// Handle an incoming IPv4 packet.
pub fn receive(packet: &[u8]) {
    if packet.len() < HEADER_SIZE || packet[0] >> 4 != 4 {
        return;
    }

    let header_length = (packet[0] & 0x0F) as usize * 4;
    if packet.len() < header_length {
        return;
    }

    let total = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    if total > packet.len() || total < header_length {
        return;
    }

    let protocol = packet[9];
    let source = Ipv4::new(packet[12], packet[13], packet[14], packet[15]);
    let destination = Ipv4::new(packet[16], packet[17], packet[18], packet[19]);

    // Not addressed to us, and nothing here forwards.
    if destination != crate::net::our_ip() {
        return;
    }

    let payload = &packet[header_length..total];

    match protocol {
        PROTOCOL_ICMP => icmp_receive(source, payload),
        PROTOCOL_UDP => crate::net::udp::receive(source, payload),
        PROTOCOL_TCP => crate::net::tcp::receive(source, payload),
        _ => {}
    }
}

const ICMP_ECHO_REPLY: u8 = 0;
const ICMP_ECHO_REQUEST: u8 = 8;

fn icmp_receive(source: Ipv4, payload: &[u8]) {
    if payload.len() < 8 {
        return;
    }

    match payload[0] {
        ICMP_ECHO_REQUEST => {
            // Echo the payload back unchanged, only the type differs.
            let mut reply: Vec<u8> = payload.to_vec();
            reply[0] = ICMP_ECHO_REPLY;
            reply[2] = 0;
            reply[3] = 0;

            let sum = checksum(&[&reply]);
            reply[2..4].copy_from_slice(&sum.to_be_bytes());

            send(source, PROTOCOL_ICMP, &reply).ok();
        }
        ICMP_ECHO_REPLY => {
            let identifier = u16::from_be_bytes([payload[4], payload[5]]);
            let sequence = u16::from_be_bytes([payload[6], payload[7]]);
            crate::net::ping::on_reply(source, identifier, sequence);
        }
        _ => {}
    }
}

/// Send an ICMP echo request.
pub fn ping(destination: Ipv4, identifier: u16, sequence: u16) -> Result<(), &'static str> {
    let mut packet = alloc::vec![0u8; 16];
    packet[0] = ICMP_ECHO_REQUEST;
    packet[4..6].copy_from_slice(&identifier.to_be_bytes());
    packet[6..8].copy_from_slice(&sequence.to_be_bytes());
    for (index, byte) in packet[8..].iter_mut().enumerate() {
        *byte = index as u8;
    }

    let sum = checksum(&[&packet]);
    packet[2..4].copy_from_slice(&sum.to_be_bytes());

    send(destination, PROTOCOL_ICMP, &packet)
}
