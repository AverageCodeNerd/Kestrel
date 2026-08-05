//! The network stack.
//!
//! Layered the way the protocols are: the card hands up Ethernet frames,
//! `poll` dispatches them by ethertype, and each layer parses only its own
//! header before passing the payload on.

pub mod arp;
pub mod dns;
pub mod e1000;
pub mod http;
pub mod ip;
pub mod ping;
pub mod tcp;
pub mod udp;

use spin::Mutex;

/// A hardware address.
pub type MacAddress = [u8; 6];

pub const BROADCAST: MacAddress = [0xFF; 6];

/// An IPv4 address, kept in network byte order.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Ipv4([u8; 4]);

impl Ipv4 {
    pub const UNSPECIFIED: Ipv4 = Ipv4([0, 0, 0, 0]);

    pub const fn new(a: u8, b: u8, c: u8, d: u8) -> Self {
        Self([a, b, c, d])
    }

    pub fn octets(&self) -> [u8; 4] {
        self.0
    }
}

impl core::fmt::Display for Ipv4 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let [a, b, c, d] = self.0;
        write!(f, "{a}.{b}.{c}.{d}")
    }
}

/// Ethernet header size: two addresses and a type.
pub const ETHERNET_HEADER: usize = 14;

pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;

/// This machine's addresses.
pub struct Config {
    pub mac: MacAddress,
    pub ip: Ipv4,
    pub gateway: Ipv4,
    pub netmask: Ipv4,
}

pub static CONFIG: Mutex<Option<Config>> = Mutex::new(None);

pub fn config<T>(body: impl FnOnce(&Config) -> T) -> Option<T> {
    x86_64::instructions::interrupts::without_interrupts(|| CONFIG.lock().as_ref().map(body))
}

pub fn our_mac() -> MacAddress {
    config(|c| c.mac).unwrap_or([0; 6])
}

pub fn our_ip() -> Ipv4 {
    config(|c| c.ip).unwrap_or(Ipv4::UNSPECIFIED)
}

/// Build an Ethernet frame around `payload` and send it.
pub fn send(destination: MacAddress, ethertype: u16, payload: &[u8]) -> Result<(), &'static str> {
    let mut frame = alloc::vec![0u8; ETHERNET_HEADER + payload.len()];

    frame[0..6].copy_from_slice(&destination);
    frame[6..12].copy_from_slice(&our_mac());
    frame[12..14].copy_from_slice(&ethertype.to_be_bytes());
    frame[ETHERNET_HEADER..].copy_from_slice(payload);

    e1000::with(|nic| nic.transmit(&frame)).unwrap_or(Err("no network card"))
}

/// Take any waiting frames and dispatch them.
///
/// Called from the shell's idle loop: the driver polls rather than taking
/// interrupts, so nothing arrives unless someone asks.
pub fn poll() {
    for _ in 0..16 {
        let Some(Some(frame)) = e1000::with(|nic| nic.receive()) else {
            return;
        };

        if frame.len() < ETHERNET_HEADER {
            continue;
        }

        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
        let payload = &frame[ETHERNET_HEADER..];

        match ethertype {
            ETHERTYPE_ARP => arp::receive(payload),
            ETHERTYPE_IPV4 => ip::receive(payload),
            _ => {}
        }
    }
}

/// Bring the card up and record our addresses.
///
/// The address is fixed for now: QEMU's user-mode network always hands out
/// 10.0.2.15 with a gateway at 10.0.2.2, so this matches what DHCP would say
/// until there is a DHCP client to ask.
pub fn init() -> Result<MacAddress, &'static str> {
    let mac = e1000::init()?;

    *CONFIG.lock() = Some(Config {
        mac,
        ip: Ipv4::new(10, 0, 2, 15),
        gateway: Ipv4::new(10, 0, 2, 2),
        netmask: Ipv4::new(255, 255, 255, 0),
    });

    Ok(mac)
}
