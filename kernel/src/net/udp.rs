//! UDP, and the pseudo-header checksum it shares with TCP.

use alloc::vec::Vec;
use spin::Mutex;

use crate::net::{Ipv4, ip};

const HEADER_SIZE: usize = 8;

/// A datagram waiting to be collected by whoever bound the port.
pub struct Datagram {
    pub source: Ipv4,
    pub source_port: u16,
    pub destination_port: u16,
    pub payload: Vec<u8>,
}

static INBOX: Mutex<Vec<Datagram>> = Mutex::new(Vec::new());

/// The checksum for UDP and TCP covers a pseudo-header of addresses and
/// length as well as the segment itself, which is what ties a segment to the
/// IP header that carried it.
pub fn pseudo_header(source: Ipv4, destination: Ipv4, protocol: u8, length: u16) -> [u8; 12] {
    let mut header = [0u8; 12];
    header[0..4].copy_from_slice(&source.octets());
    header[4..8].copy_from_slice(&destination.octets());
    header[8] = 0;
    header[9] = protocol;
    header[10..12].copy_from_slice(&length.to_be_bytes());
    header
}

pub fn send(
    destination: Ipv4,
    source_port: u16,
    destination_port: u16,
    payload: &[u8],
) -> Result<(), &'static str> {
    let length = HEADER_SIZE + payload.len();
    let mut datagram = alloc::vec![0u8; length];

    datagram[0..2].copy_from_slice(&source_port.to_be_bytes());
    datagram[2..4].copy_from_slice(&destination_port.to_be_bytes());
    datagram[4..6].copy_from_slice(&(length as u16).to_be_bytes());

    datagram[HEADER_SIZE..].copy_from_slice(payload);

    let pseudo = pseudo_header(
        crate::net::our_ip(),
        destination,
        ip::PROTOCOL_UDP,
        length as u16,
    );
    let sum = ip::checksum(&[&pseudo, &datagram]);
    // A zero checksum means "not computed", so it is transmitted as all ones.
    let sum = if sum == 0 { 0xFFFF } else { sum };
    datagram[6..8].copy_from_slice(&sum.to_be_bytes());

    ip::send(destination, ip::PROTOCOL_UDP, &datagram)
}

pub fn receive(source: Ipv4, datagram: &[u8]) {
    if datagram.len() < HEADER_SIZE {
        return;
    }

    let source_port = u16::from_be_bytes([datagram[0], datagram[1]]);
    let destination_port = u16::from_be_bytes([datagram[2], datagram[3]]);
    let length = u16::from_be_bytes([datagram[4], datagram[5]]) as usize;

    if length < HEADER_SIZE || length > datagram.len() {
        return;
    }

    INBOX.lock().push(Datagram {
        source,
        source_port,
        destination_port,
        payload: datagram[HEADER_SIZE..length].to_vec(),
    });
}

/// Take the first datagram addressed to `port`, if one has arrived.
pub fn take(port: u16) -> Option<Datagram> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut inbox = INBOX.lock();
        let index = inbox
            .iter()
            .position(|datagram| datagram.destination_port == port)?;
        Some(inbox.remove(index))
    })
}
