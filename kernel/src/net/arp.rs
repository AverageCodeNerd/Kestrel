//! Address Resolution Protocol: turning an IPv4 address into a MAC address.
//!
//! Nothing can be sent to a machine on the local network until its hardware
//! address is known, so this sits underneath everything except ARP itself.

use alloc::vec::Vec;
use spin::Mutex;

use crate::net::{BROADCAST, ETHERTYPE_ARP, Ipv4, MacAddress};

const HARDWARE_ETHERNET: u16 = 1;
const PROTOCOL_IPV4: u16 = 0x0800;

const OPERATION_REQUEST: u16 = 1;
const OPERATION_REPLY: u16 = 2;

/// An ARP packet for IPv4 over Ethernet is always this long.
const PACKET_SIZE: usize = 28;

pub struct Entry {
    pub ip: Ipv4,
    pub mac: MacAddress,
}

static CACHE: Mutex<Vec<Entry>> = Mutex::new(Vec::new());

pub fn lookup(ip: Ipv4) -> Option<MacAddress> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        CACHE
            .lock()
            .iter()
            .find(|entry| entry.ip == ip)
            .map(|entry| entry.mac)
    })
}

fn remember(ip: Ipv4, mac: MacAddress) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut cache = CACHE.lock();
        match cache.iter_mut().find(|entry| entry.ip == ip) {
            Some(entry) => entry.mac = mac,
            None => cache.push(Entry { ip, mac }),
        }
    });
}

pub fn entries() -> Vec<(Ipv4, MacAddress)> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        CACHE
            .lock()
            .iter()
            .map(|entry| (entry.ip, entry.mac))
            .collect()
    })
}

fn build(operation: u16, target_mac: MacAddress, target_ip: Ipv4) -> [u8; PACKET_SIZE] {
    let mut packet = [0u8; PACKET_SIZE];

    packet[0..2].copy_from_slice(&HARDWARE_ETHERNET.to_be_bytes());
    packet[2..4].copy_from_slice(&PROTOCOL_IPV4.to_be_bytes());
    packet[4] = 6; // hardware address length
    packet[5] = 4; // protocol address length
    packet[6..8].copy_from_slice(&operation.to_be_bytes());

    packet[8..14].copy_from_slice(&crate::net::our_mac());
    packet[14..18].copy_from_slice(&crate::net::our_ip().octets());
    packet[18..24].copy_from_slice(&target_mac);
    packet[24..28].copy_from_slice(&target_ip.octets());

    packet
}

/// Ask who owns `ip`. The answer arrives later, through `receive`.
pub fn request(ip: Ipv4) -> Result<(), &'static str> {
    // The target hardware address is what we are asking for, so it is left
    // zero and the request goes to everyone.
    let packet = build(OPERATION_REQUEST, [0; 6], ip);
    crate::net::send(BROADCAST, ETHERTYPE_ARP, &packet)
}

/// Look up an address, asking for it and waiting if it is not yet known.
///
/// Safe to call from the receive path: anything we are replying to is already
/// in the cache, so it returns immediately without re-entering the poll loop.
pub fn resolve(ip: Ipv4, timeout_ms: u64) -> Option<MacAddress> {
    if let Some(mac) = lookup(ip) {
        return Some(mac);
    }

    request(ip).ok()?;

    let deadline = crate::apic::ticks() + timeout_ms / 10;
    while crate::apic::ticks() < deadline {
        crate::net::poll();

        if let Some(mac) = lookup(ip) {
            return Some(mac);
        }

        core::hint::spin_loop();
    }

    None
}

/// Handle an incoming ARP packet.
pub fn receive(packet: &[u8]) {
    if packet.len() < PACKET_SIZE {
        return;
    }

    let hardware = u16::from_be_bytes([packet[0], packet[1]]);
    let protocol = u16::from_be_bytes([packet[2], packet[3]]);
    if hardware != HARDWARE_ETHERNET || protocol != PROTOCOL_IPV4 {
        return;
    }

    let operation = u16::from_be_bytes([packet[6], packet[7]]);

    let sender_mac: MacAddress = packet[8..14].try_into().unwrap();
    let sender_ip = Ipv4::new(packet[14], packet[15], packet[16], packet[17]);
    let target_ip = Ipv4::new(packet[24], packet[25], packet[26], packet[27]);

    // Both requests and replies tell us where the sender is.
    remember(sender_ip, sender_mac);

    if operation == OPERATION_REQUEST && target_ip == crate::net::our_ip() {
        let reply = build(OPERATION_REPLY, sender_mac, sender_ip);
        crate::net::send(sender_mac, ETHERTYPE_ARP, &reply).ok();
    }
}
