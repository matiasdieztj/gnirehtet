//! Routes IP packets between the device and the appropriate TCP/UDP connections.
//! Maintains a map of active connections and creates new ones on demand.

use log::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::rc::{Rc, Weak};

use super::binary;
use super::client::{Client, ClientChannel, SharedBuffer};
use super::connection::{Connection, ConnectionId};
use super::ip_packet::IpPacket;
use super::ipv4_header::Protocol;
use super::tcp_connection::TcpConnection;
use super::udp_connection::UdpConnection;

const TAG: &str = "Router";

pub struct Router {
    client: Weak<RefCell<Client>>,
    /// Shared outgoing buffer. Passed to every connection so that they can
    /// write back to the device without borrowing the `Client`.
    buffer: Option<SharedBuffer>,
    /// Label included in every connection id so that log lines can be
    /// correlated with a specific device.
    client_label: Rc<str>,
    connections: HashMap<ConnectionId, Rc<RefCell<dyn Connection>>>,
}

impl Router {
    pub fn new() -> Self {
        Self {
            client: Weak::new(),
            buffer: None,
            client_label: Rc::from(""),
            connections: HashMap::new(),
        }
    }

    pub fn set_client(&mut self, client: Weak<RefCell<Client>>) {
        self.client = client;
    }

    /// Set the client label used in connection ids. Must be called before
    /// any connection is created.
    pub fn set_client_label(&mut self, label: &str) {
        self.client_label = Rc::from(label);
    }

    /// Register the shared outgoing buffer. Must be called before any
    /// connection is created.
    pub fn set_buffer(&mut self, buffer: SharedBuffer) {
        self.buffer = Some(buffer);
    }

    /// Route an IP packet from the device to the appropriate connection.
    /// Creates a new connection if one doesn't exist for this flow.
    pub fn send_to_network(&mut self, client_channel: &mut ClientChannel, ip_packet: &IpPacket) {
        if ip_packet.is_valid() {
            let id = {
                let (ip_header_data, transport_header_data) = ip_packet.headers_data();
                let Some(transport_header_data) = transport_header_data else {
                    warn!(target: TAG, "Dropping packet: no transport header data");
                    return;
                };
                ConnectionId::from_headers(
                    &ip_header_data,
                    transport_header_data,
                    self.client_label.clone(),
                )
        };
            match self.connections.entry(id.clone()) {
                std::collections::hash_map::Entry::Occupied(entry) => {
                    let mut connection = entry.get().borrow_mut();
                    connection.send_to_network(client_channel, ip_packet);
                    if connection.is_closed() {
                        debug!(target: TAG, "Removing connection from router: {}", id);
                        drop(connection);
                        entry.remove();
                    }
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let buffer = match self.buffer.clone() {
                        Some(b) => b,
                        None => {
                            error!(target: TAG, "Router buffer not initialized, dropping packet");
                            return;
                        }
                    };
                    match Self::create_connection(id.clone(), &self.client, &buffer, ip_packet) {
                        Ok(connection) => {
                            entry.insert(connection);
                        }
                        Err(err) => {
                            error!(target: TAG, "Cannot create route, dropping packet: {}", err);
                        }
                    }
                }
            }
        } else {
            warn!(target: TAG, "Dropping invalid packet");
            if log_enabled!(target: TAG, Level::Trace) {
                trace!(
                    target: TAG,
                    "{}",
                    binary::build_packet_string(ip_packet.raw())
                );
            }
        }
    }

    fn create_connection(
        id: ConnectionId,
        client: &Weak<RefCell<Client>>,
        buffer: &SharedBuffer,
        ip_packet: &IpPacket,
    ) -> io::Result<Rc<RefCell<dyn Connection>>> {
        let (ip_header, transport_header) = ip_packet.headers();
        let transport_header =
            transport_header.ok_or_else(|| io::Error::other("No transport header"))?;
        match id.protocol() {
            Protocol::Tcp => Ok(TcpConnection::create(
                id,
                client.clone(),
                buffer.clone(),
                ip_header,
                transport_header,
            )?),
            Protocol::Udp => Ok(UdpConnection::create(
                id,
                client.clone(),
                buffer.clone(),
                ip_header,
                transport_header,
            )?),
            p => Err(io::Error::other(format!("Unsupported protocol: {:?}", p))),
        }
    }

    pub fn remove(&mut self, id: &ConnectionId) {
        if self.connections.remove(id).is_some() {
            debug!(target: TAG, "Removing connection from router: {}", id);
        }
    }

    pub fn clear(&mut self) {
        for connection in self.connections.values() {
            connection.borrow_mut().close();
        }
        self.connections.clear();
    }

    pub fn clean_expired_connections(&mut self) {
        let expired_ids: Vec<ConnectionId> = self
            .connections
            .iter()
            .filter(|(_, connection)| connection.borrow().is_expired())
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired_ids {
            if let Some(connection) = self.connections.remove(id) {
                debug!(target: TAG, "Removing expired connection from router: {}", id);
                connection.borrow_mut().close();
            }
        }
    }

    /// Poll all connections for network I/O.
    /// Removes closed connections.
    pub fn poll_connections(&mut self) {
        let closed_ids: Vec<ConnectionId> = self
            .connections
            .iter()
            .filter_map(|(id, connection)| {
                let mut conn = connection.borrow_mut();
                match conn.poll() {
                    Ok(_) => {
                        if conn.is_closed() {
                            Some(id.clone())
                        } else {
                            None
                        }
                    }
                    Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => None,
                    Err(_) => {
                        // error — close the connection
                        conn.close();
                        Some(id.clone())
                    }
                }
            })
            .collect();
        for id in &closed_ids {
            debug!(target: TAG, "Removing connection from router: {}", id);
            self.connections.remove(id);
        }
    }
}
