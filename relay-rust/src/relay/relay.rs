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

//! The relay engine: runs on tokio's multi-threaded runtime, accepting reverse-tunnel
//! connections from Android devices and relaying IP packets to/from the internet.

use log::*;
use std::io;
use std::net::TcpStream;
use std::time::{Duration, Instant};

use super::client::Client;
use super::serial_registry;

const TAG: &str = "Relay";

pub struct Relay {
    port: u16,
}

impl Relay {
    pub fn new(port: u16) -> Self {
        Self { port }
    }

    /// Start the relay server. Creates a tokio runtime and enters the async accept loop.
    /// Accepted clients are handed off to dedicated OS threads.
    pub fn run(&self) -> io::Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(self.run_async())
    }

    async fn run_async(&self) -> io::Result<()> {
        let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", self.port)).await?;
        info!(target: TAG, "Relay server started on port {}", self.port);
        loop {
            let (stream, peer) = listener.accept().await?;

            // Best-effort: consume the oldest pending serial, if any.
            let serial = serial_registry::take_pending();
            match &serial {
                Some(s) => debug!(target: TAG, "New connection from {} (serial {})", peer, s),
                None => debug!(target: TAG, "New connection from {}", peer),
            }

            let std_stream = stream.into_std()?;
            std::thread::spawn(move || {
                Client::run_blocking(std_stream, serial);
            });
        }
    }

    /// Forward mode: connect to the client's `ServerSocket` instead of
    /// listening. Used for API 19 devices where `adb reverse` is unavailable.
    ///
    /// The connection is retried on every disconnect. There are two reasons:
    ///
    /// 1. Startup race: `adb forward` accepts the host-side TCP handshake
    ///    immediately, even before the device has bound its `ServerSocket`.
    ///    A successful `TcpStream::connect` therefore does NOT mean the
    ///    client is listening. If the client is not ready yet, adb closes
    ///    the connection and `Client::run_blocking` sees EOF right away.
    ///    Without a retry, the whole process would exit before the client
    ///    had a chance to start.
    ///
    /// 2. Client reconnects: the Java `PersistentRelayTunnel` recreates its
    ///    `ServerSocket` when the tunnel drops. The relay must reconnect too.
    pub fn run_forward(&self, serial: Option<String>) -> io::Result<()> {
        loop {
            let stream = self.connect_forward()?;
            Client::run_blocking(stream, serial.clone());
            info!(target: TAG, "Client disconnected, retrying in 500 ms...");
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    fn connect_forward(&self) -> io::Result<TcpStream> {
        let addr = format!("127.0.0.1:{}", self.port);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match TcpStream::connect(&addr) {
                Ok(stream) => {
                    info!(target: TAG, "Forward tunnel connected on {}", addr);
                    return Ok(stream);
                }
                Err(err) => {
                    if Instant::now() >= deadline {
                        error!(
                            target: TAG,
                            "Timed out waiting for client to listen on {}: {}",
                            addr, err
                        );
                        return Err(err);
                    }
                    debug!(
                        target: TAG,
                        "Waiting for client to accept on {} ({}); retrying...",
                        addr, err
                    );
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    }
}
