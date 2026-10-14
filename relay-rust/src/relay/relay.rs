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

    /// Start the relay server in reverse mode. Creates a tokio runtime and
    /// enters the async accept loop. Accepted clients are handed off to
    /// dedicated OS threads.
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
    /// # Retry strategy
    ///
    /// `adb forward` accepts the host-side TCP handshake *immediately*, even
    /// before the device has bound its `ServerSocket`. A successful
    /// `TcpStream::connect` therefore does NOT mean the client is listening.
    /// When the device is not ready, adb closes the connection right away and
    /// the relay sees EOF as soon as it reads — a "probe" connection.
    ///
    /// Without backing off, the relay would produce a storm of ~10–16 probe
    /// connections during the 5–8 seconds that KitKat typically takes to
    /// bring up the `ServerSocket`. To avoid that, consecutive short
    /// connections (which indicate the client is not yet ready) trigger
    /// exponential backoff: 500 ms → 1 s → 2 s → 4 s (capped). A normal
    /// disconnect (connection lived long enough to be real) resets the
    /// counter and retries at the default 500 ms cadence.
    pub fn run_forward(&self, serial: Option<String>) -> io::Result<()> {
        let mut consecutive_probes: u32 = 0;

        loop {
            let start = Instant::now();
            let stream = self.connect_forward()?;
            Client::run_blocking(stream, serial.clone());
            let duration = start.elapsed();

            if duration < Duration::from_millis(500) {
                // Probe connection: the device's ServerSocket wasn't listening
                // yet. Back off exponentially so a slow client start does not
                // flood the log or waste CPU.
                consecutive_probes = consecutive_probes.saturating_add(1);
                let shift = consecutive_probes.min(3);
                let delay_ms = 500u64 << shift;
                debug!(
                    target: TAG,
                    "Client not ready (probe #{}, connection ended after {:?}); retrying in {} ms",
                    consecutive_probes, duration, delay_ms
                );
                std::thread::sleep(Duration::from_millis(delay_ms));
            } else {
                // Real client: it connected, did work, then dropped. Reset
                // the backoff and retry at the normal cadence.
                consecutive_probes = 0;
                info!(
                    target: TAG,
                    "Client disconnected after {:?}, retrying in 500 ms...",
                    duration
                );
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }

    fn connect_forward(&self) -> io::Result<TcpStream> {
        let addr = format!("127.0.0.1:{}", self.port);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match TcpStream::connect(&addr) {
                Ok(stream) => {
                    // Not logged here: the connect succeeding does not mean
                    // the client is listening (see run_forward's doc comment).
                    // The real "connected" log happens inside Client after
                    // the first successful read.
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
