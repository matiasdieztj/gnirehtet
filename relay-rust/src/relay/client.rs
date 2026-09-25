/*
 * Copyright (C) 2017 Genymobile
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Handles I/O with one Android device over a reverse-tunnel TCP connection.
//! Reads raw IP packets from the device and routes them to the network,
//! then sends back any response packets.

use log::*;
use std::cell::RefCell;
use std::io::{self, Cursor, Read, Write};
use std::net::TcpStream;
use std::rc::Rc;
use std::time::Duration;

use super::close_listener::CloseListener;
use super::ip_packet::IpPacket;
use super::ip_packet_buffer::IpPacketBuffer;
use super::ipv4_packet::MAX_PACKET_LENGTH;
use super::router::Router;
use super::stream_buffer::StreamBuffer;

const TAG: &str = "Client";

/// Shared outgoing buffer. Connections hold a clone of this `Rc` so they can
/// write packets back to the device without borrowing the whole `Client`
/// (which would panic since the main relay loop already holds a mutable
/// borrow of the client while polling connections).
pub type SharedBuffer = Rc<RefCell<StreamBuffer>>;

pub struct Client {
    #[allow(dead_code)]
    id: u32,
    #[allow(dead_code)]
    serial: Option<String>,
    /// Human-readable label used in log messages. Format: `#N (serial)`
    /// or `#N` when the serial is not known.
    #[allow(dead_code)]
    label: String,
    client_to_network: IpPacketBuffer,
    network_to_client: SharedBuffer,
    router: Router,
    closed: bool,
    close_listener: Box<dyn CloseListener<Client>>,
}

/// Channel for connections to send back data immediately to the client.
///
/// It holds a clone of the shared buffer `Rc`; no lifetime is required
/// because the `Rc` keeps the buffer alive independently of the `Client`.
pub struct ClientChannel {
    buffer: SharedBuffer,
}

impl ClientChannel {
    pub fn new(buffer: SharedBuffer) -> Self {
        Self { buffer }
    }

    /// Write an IP packet into the outgoing buffer to the device.
    /// Returns `WouldBlock` if the buffer is full or currently borrowed.
    pub fn send_to_client(&mut self, ip_packet: &IpPacket) -> io::Result<()> {
        let mut buffer = match self.buffer.try_borrow_mut() {
            Ok(b) => b,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "client buffer busy",
                ));
            }
        };
        if ip_packet.length() as usize <= buffer.remaining() {
            buffer.read_from(ip_packet.raw());
            Ok(())
        } else {
            warn!(target: TAG, "Client buffer full");
            Err(io::Error::new(io::ErrorKind::WouldBlock, "Client buffer full"))
        }
    }
}

impl Client {
    pub fn create(
        id: u32,
        serial: Option<String>,
        close_listener: Box<dyn CloseListener<Client>>,
    ) -> io::Result<Rc<RefCell<Self>>> {
        let label = match &serial {
            Some(s) => format!("#{} ({})", id, s),
            None => format!("#{}", id),
        };

        let buffer: SharedBuffer =
            Rc::new(RefCell::new(StreamBuffer::new(16 * MAX_PACKET_LENGTH)));
        
        let mut router = Router::new();
        router.set_buffer(buffer.clone());
        router.set_client_label(&label);

        let rc = Rc::new(RefCell::new(Self {
            id,
            serial,
            label,
            client_to_network: IpPacketBuffer::new(),
            network_to_client: buffer,
            router,
            closed: false,
            close_listener,
        }));

        {
            let mut self_ref = rc.borrow_mut();
            self_ref.router.set_client(Rc::downgrade(&rc));
        }
        Ok(rc)
    }

    #[allow(dead_code)]
    pub fn id(&self) -> u32 {
        self.id
    }

    #[allow(dead_code)]
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    pub fn router(&mut self) -> &mut Router {
        &mut self.router
    }

    /// Return a channel that connections can use to write back to the device.
    pub fn channel(&mut self) -> ClientChannel {
        ClientChannel::new(self.network_to_client.clone())
    }

    fn close(&mut self) {
        self.closed = true;
        self.router.clear();
        self.close_listener.on_closed(self);
    }

    pub fn send_to_client(&mut self, ip_packet: &IpPacket) -> io::Result<()> {
        let mut buffer = match self.network_to_client.try_borrow_mut() {
            Ok(b) => b,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "client buffer busy",
                ));
            }
        };
        if ip_packet.length() as usize <= buffer.remaining() {
            buffer.read_from(ip_packet.raw());
            Ok(())
        } else {
            warn!(target: TAG, "Client buffer full");
            Err(io::Error::new(io::ErrorKind::WouldBlock, "Client buffer full"))
        }
    }

    pub fn clean_expired_connections(&mut self) {
        self.router.clean_expired_connections();
    }

    /// Process packets from the device: feed raw bytes and route complete packets.
    /// Returns the number of complete packets routed.
    pub fn feed_device_data(&mut self, data: &[u8]) -> usize {
        let mut count = 0;
        let mut cursor = Cursor::new(data);
        if self
            .client_to_network
            .read_from(&mut cursor)
            .unwrap_or(false)
        {
            while let Some(packet) = self.client_to_network.as_ip_packet() {
                let mut channel = ClientChannel::new(self.network_to_client.clone());
                self.router.send_to_network(&mut channel, &packet);
                self.client_to_network.next();
                count += 1;
            }
        }
        count
    }

    /// Poll all network connections (TCP/UDP) for incoming/outgoing data.
    ///
    /// The `RefCell` on the shared buffer is *not* borrowed during this
    /// call, so any connection polled here may freely write back to the
    /// device by borrowing its own clone of the buffer.
    pub fn poll_network_connections(&mut self) {
        self.router.poll_connections();
    }

    /// Drain the outgoing buffer into a Vec for writing to the stream.
    pub fn drain_outgoing(&mut self) -> Vec<u8> {
        let mut buffer = match self.network_to_client.try_borrow_mut() {
            Ok(b) => b,
            Err(_) => return Vec::new(),
        };
        let size = buffer.size();
        if size == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u8; size];
        let mut cursor = Cursor::new(&mut buf[..]);
        let _ = buffer.write_to(&mut cursor);
        buf
    }

    /// Returns true if there is data to send to the device.
    #[allow(dead_code)]
    pub fn has_outgoing(&self) -> bool {
        match self.network_to_client.try_borrow() {
            Ok(b) => !b.is_empty(),
            Err(_) => false,
        }
    }

    /// Entry point for a client connection. Uses blocking I/O on the TCP stream
    /// (converted from tokio accept) and runs the sync relay loop.
    /// Spawned onto a dedicated OS thread so the main async accept loop is not blocked.
    pub fn run_blocking(tcp_stream: TcpStream, serial: Option<String>) {
        let mut stream = tcp_stream;
        if let Err(e) = stream.set_nonblocking(true) {
            error!(target: TAG, "Failed to set non-blocking: {}", e);
            return;
        }

        // Assign client ID and send it to the device first (device expects relay to write first)
        static NEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        
        // Human-readable label used in log messages. Includes the ADB serial
        // when the relay was able to correlate this connection with a device.
        let label = match &serial {
            Some(s) => format!("#{} ({})", id, s),
            None => format!("#{}", id),
        };
        let id_bytes = id.to_be_bytes();
        if write_all(&mut stream, &id_bytes).is_err() {
            error!(target: TAG, "Failed to write client ID");
            return;
        }
        info!(target: TAG, "Client {} connected", label);

        let close_tx = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let close_rx = close_tx.clone();

        let close_listener = Box::new(move |_: &Client| {
            close_tx.store(true, std::sync::atomic::Ordering::SeqCst);
        }) as Box<dyn CloseListener<Client>>;

        let client_rc = match Self::create(id, serial.clone(), close_listener) {
            Ok(c) => c,
            Err(e) => {
                error!(target: TAG, "Failed to create client state: {}", e);
                return;
            }
        };

        // Main loop: poll for I/O with basic timing
        let mut read_buf = [0u8; MAX_PACKET_LENGTH];
        let mut last_cleanup = std::time::Instant::now();

        loop {
            let mut made_progress = false;

            // Read from device stream (non-blocking)
            let mut client = client_rc.borrow_mut();
            if client.closed || close_rx.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }

            match stream.read(&mut read_buf) {
                Ok(0) => {
                    debug!(target: TAG, "Client {} EOF received", label);
                    client.close();
                    break;
                }
                Ok(n) => {
                    made_progress = true;
                    client.feed_device_data(&read_buf[..n]);
                    client.poll_network_connections();
                }
                Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    error!(target: TAG, "Client {} read error: {}", label, e);
                    client.close();
                    break;
                }
            }

            // Poll network connections
            if !client.closed {
                client.poll_network_connections();
            }

            // Periodic cleanup
            let now = std::time::Instant::now();
            if now.duration_since(last_cleanup).as_secs() >= 5 {
                client.clean_expired_connections();
                last_cleanup = now;
            }

            // Flush outgoing data
            let outgoing = client.drain_outgoing();
            drop(client);

            if !outgoing.is_empty() {
                made_progress = true;
                if write_all(&mut stream, &outgoing).is_err() {
                    error!(target: TAG, "Client {} write error", label);
                    break;
                }
            }

            if !made_progress {
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        info!(target: TAG, "Client {} disconnected", label);
    }
}

#[allow(dead_code)]
/// Helper: read exactly `buf.len()` bytes from a non-blocking stream.
fn read_exact(stream: &mut TcpStream, buf: &mut [u8]) -> io::Result<()> {
    let mut offset = 0;
    while offset < buf.len() {
        match stream.read(&mut buf[offset..]) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof")),
            Ok(n) => offset += n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Helper: write all bytes to a non-blocking stream.
fn write_all(stream: &mut TcpStream, buf: &[u8]) -> io::Result<()> {
    let mut offset = 0;
    while offset < buf.len() {
        match stream.write(&buf[offset..]) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "write zero")),
            Ok(n) => offset += n,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
