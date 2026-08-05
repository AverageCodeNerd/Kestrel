//! TCP. Client side only: enough to connect out, exchange a request and
//! response, and close.

use alloc::vec::Vec;
use spin::Mutex;

use crate::net::{Ipv4, ip, udp::pseudo_header};

const FIN: u8 = 1 << 0;
const SYN: u8 = 1 << 1;
const RST: u8 = 1 << 2;
const PSH: u8 = 1 << 3;
const ACK: u8 = 1 << 4;

/// A header with no options.
const HEADER_SIZE: usize = 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Closed,
    SynSent,
    Established,
    FinWait,
    Closing,
}

pub struct Connection {
    pub state: State,
    pub remote: Ipv4,
    pub remote_port: u16,
    pub local_port: u16,
    /// Next sequence number we will send.
    pub send_next: u32,
    /// Next sequence number we expect from them.
    pub receive_next: u32,
    pub received: Vec<u8>,
    /// Set when the peer has sent FIN.
    pub peer_finished: bool,
}

impl Connection {
    fn new(remote: Ipv4, remote_port: u16, local_port: u16) -> Self {
        Self {
            state: State::Closed,
            remote,
            remote_port,
            local_port,
            // A fixed initial sequence number. Real stacks randomise this to
            // make blind injection harder; nothing here is exposed to a
            // hostile network.
            send_next: 0x1000,
            receive_next: 0,
            received: Vec::new(),
            peer_finished: false,
        }
    }
}

/// One connection at a time, which is all the shell needs.
pub static CONNECTION: Mutex<Option<Connection>> = Mutex::new(None);

fn build(
    connection: &Connection,
    flags: u8,
    payload: &[u8],
) -> Vec<u8> {
    let mut segment = alloc::vec![0u8; HEADER_SIZE + payload.len()];

    segment[0..2].copy_from_slice(&connection.local_port.to_be_bytes());
    segment[2..4].copy_from_slice(&connection.remote_port.to_be_bytes());
    segment[4..8].copy_from_slice(&connection.send_next.to_be_bytes());
    segment[8..12].copy_from_slice(&connection.receive_next.to_be_bytes());
    // Data offset in 32-bit words, in the high nibble.
    segment[12] = (HEADER_SIZE as u8 / 4) << 4;
    segment[13] = flags;
    segment[14..16].copy_from_slice(&8192u16.to_be_bytes()); // window

    segment[HEADER_SIZE..].copy_from_slice(payload);

    let pseudo = pseudo_header(
        crate::net::our_ip(),
        connection.remote,
        ip::PROTOCOL_TCP,
        segment.len() as u16,
    );
    let sum = ip::checksum(&[&pseudo, &segment]);
    segment[16..18].copy_from_slice(&sum.to_be_bytes());

    segment
}

fn transmit(connection: &Connection, flags: u8, payload: &[u8]) -> Result<(), &'static str> {
    let segment = build(connection, flags, payload);
    ip::send(connection.remote, ip::PROTOCOL_TCP, &segment)
}

/// Open a connection and wait for the handshake to complete.
pub fn connect(remote: Ipv4, port: u16, timeout_ms: u64) -> Result<(), &'static str> {
    let local_port = 49152 + (crate::apic::ticks() as u16 % 4096);
    let mut connection = Connection::new(remote, port, local_port);

    connection.state = State::SynSent;
    transmit(&connection, SYN, &[])?;
    // The SYN itself consumes a sequence number.
    connection.send_next = connection.send_next.wrapping_add(1);

    *CONNECTION.lock() = Some(connection);

    wait_for(timeout_ms, |c| c.state == State::Established)
        .map_err(|_| "connection timed out")
}

pub fn send(payload: &[u8]) -> Result<(), &'static str> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut guard = CONNECTION.lock();
        let connection = guard.as_mut().ok_or("not connected")?;

        if connection.state != State::Established {
            return Err("not connected");
        }

        transmit(connection, ACK | PSH, payload)?;
        connection.send_next = connection.send_next.wrapping_add(payload.len() as u32);
        Ok(())
    })
}

/// Poll until `ready` holds, or the timeout expires.
fn wait_for(
    timeout_ms: u64,
    ready: impl Fn(&Connection) -> bool,
) -> Result<(), &'static str> {
    let deadline = crate::apic::ticks() + timeout_ms / 10;

    while crate::apic::ticks() < deadline {
        crate::net::poll();

        let done = x86_64::instructions::interrupts::without_interrupts(|| {
            CONNECTION.lock().as_ref().is_some_and(&ready)
        });
        if done {
            return Ok(());
        }

        core::hint::spin_loop();
    }

    Err("timed out")
}

/// Wait until the peer closes, then take everything received.
pub fn read_to_end(timeout_ms: u64) -> Vec<u8> {
    wait_for(timeout_ms, |c| c.peer_finished).ok();

    x86_64::instructions::interrupts::without_interrupts(|| {
        CONNECTION
            .lock()
            .as_mut()
            .map(|c| core::mem::take(&mut c.received))
            .unwrap_or_default()
    })
}

pub fn close() {
    x86_64::instructions::interrupts::without_interrupts(|| {
        if let Some(connection) = CONNECTION.lock().as_mut() {
            if connection.state == State::Established {
                transmit(connection, ACK | FIN, &[]).ok();
                connection.state = State::FinWait;
            }
        }
    });

    *CONNECTION.lock() = None;
}

pub fn receive(source: Ipv4, segment: &[u8]) {
    if segment.len() < HEADER_SIZE {
        return;
    }

    let source_port = u16::from_be_bytes([segment[0], segment[1]]);
    let destination_port = u16::from_be_bytes([segment[2], segment[3]]);
    let sequence = u32::from_be_bytes(segment[4..8].try_into().unwrap());
    let acknowledgement = u32::from_be_bytes(segment[8..12].try_into().unwrap());
    let data_offset = (segment[12] >> 4) as usize * 4;
    let flags = segment[13];

    if data_offset < HEADER_SIZE || data_offset > segment.len() {
        return;
    }
    let payload = &segment[data_offset..];

    let mut guard = CONNECTION.lock();
    let Some(connection) = guard.as_mut() else {
        return;
    };

    // Ignore anything that is not this connection.
    if connection.remote != source
        || connection.remote_port != source_port
        || connection.local_port != destination_port
    {
        return;
    }

    if flags & RST != 0 {
        connection.state = State::Closed;
        connection.peer_finished = true;
        return;
    }

    match connection.state {
        State::SynSent => {
            if flags & SYN != 0 && flags & ACK != 0 {
                // Their SYN occupies one sequence number, which our ACK covers.
                connection.receive_next = sequence.wrapping_add(1);
                connection.state = State::Established;
                let _ = acknowledgement;
                transmit(connection, ACK, &[]).ok();
            }
        }

        State::Established | State::FinWait => {
            // Only in-order data is accepted; there is no reassembly queue.
            if !payload.is_empty() && sequence == connection.receive_next {
                connection.received.extend_from_slice(payload);
                connection.receive_next =
                    connection.receive_next.wrapping_add(payload.len() as u32);
                transmit(connection, ACK, &[]).ok();
            }

            if flags & FIN != 0 && sequence.wrapping_add(payload.len() as u32) == connection.receive_next
            {
                // FIN also consumes a sequence number.
                connection.receive_next = connection.receive_next.wrapping_add(1);
                connection.peer_finished = true;
                transmit(connection, ACK, &[]).ok();
                connection.state = State::Closing;
            }
        }

        _ => {}
    }
}
