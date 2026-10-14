/*
 * Copyright (C) 2026 Genymobile
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

//! Cross-platform dummy Wi-Fi hotspot creation for the relay host.
//!
//! On API 19-20 (KitKat) and API 31+ (Android 12+), the framework refuses to
//! promote a VPN-only transport to the system default network. Apps gating on
//! `ConnectivityManager.getActiveNetworkInfo()` see `null` and never open
//! sockets. The Android client's `ConnectivityFallback` can auto-connect to an
//! open AP — but only if one exists in range.
//!
//! This module creates that AP on the host, automatically, for the duration of
//! the relay. When the relay exits, the hotspot is torn down.
//!
//! # SSID scheme
//!
//! The default SSID is `gnirehtet-<5 hex chars>`, derived from an FNV-1a hash
//! of the hostname. This gives a **stable, opaque identifier per host**:
//! the same machine always proposes the same suffix, so the Android client can
//! reconnect reliably. Before creating the AP, the relay scans nearby Wi-Fi
//! networks for existing `gnirehtet-*` SSIDs. If the derived one is already
//! in use (e.g. a neighbour with the same hostname), a random 5-hex suffix is
//! tried instead (up to 8 attempts).
//!
//! The actual SSID is exposed via [`Hotspot::ssid`] so the caller can propagate
//! it to the Android client through `am start --es dummyWifiSsid <ssid>`.
//! This is what allows the client to pick the right AP when several hosts are
//! broadcasting a `gnirehtet-*` network in range.
//!
//! # Platform support
//!
//! | OS      | Mechanism                          | Privileges            |
//! |---------|------------------------------------|-----------------------|
//! | Linux   | NetworkManager (`nmcli`)           | polkit (usually none) |
//! | Windows | `netsh` hostednetwork, then WinRT  | admin / none          |
//! | macOS   | Not supported (Apple restriction)  | —                     |
//!
//! # Usage
//!
//! ```no_run
//! use gnirehtet::hotspot::{Hotspot, HotspotConfig};
//!
//! let config = HotspotConfig::default();
//! let guard = Hotspot::start(&config).expect("cannot start hotspot");
//! println!("Broadcasting as {}", guard.ssid());
//! // ... run relay ...
//! guard.stop().ok(); // or let Drop handle it
//! ```

use log::*;
use std::io;

const TAG: &str = "Hotspot";

/// Configuration for the dummy Wi-Fi hotspot.
#[derive(Clone, Default)]
pub struct HotspotConfig {
    /// SSID to broadcast.
    ///
    /// - `None` (default): auto-generate a unique SSID of the form
    ///   `gnirehtet-<5 hex>`. The suffix is derived from the hostname, so it
    ///   is stable across restarts on the same machine. Collision detection
    ///   against nearby `gnirehtet-*` networks is performed before creation.
    /// - `Some(name)`: use `name` verbatim, no auto-generation, no collision
    ///   detection. Intended for manual overrides.
    pub ssid: Option<String>,
    /// WPA2 password. `None` creates an open network, which is what the
    /// Android client's `ConnectivityFallback` expects.
    pub password: Option<String>,
    /// Wi-Fi interface to use (Linux only). `None` auto-detects via `nmcli`.
    pub interface: Option<String>,
}

// ═══════════════════════════════════════════════════════════════════════
// Linux
// ═══════════════════════════════════════════════════════════════════════

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    const SSID_PREFIX: &str = "gnirehtet-";
    const MAX_COLLISION_ATTEMPTS: u32 = 8;

    /// Linux implementation using NetworkManager's `nmcli`.
    ///
    /// `ipv4.method shared` runs dnsmasq as a DHCP server on the interface and
    /// installs a NAT masquerade rule. The NAT is not a problem for gnirehtet:
    /// the VPN's policy routing rule (`fwmark 0x3c` → table 60 → `tun0`)
    /// intercepts all app traffic before it can reach the hotspot's NAT. Only
    /// traffic without the fwmark (e.g. the ADB shell) would use the NAT, and
    /// that traffic is not routed through the tunnel anyway.
    ///
    /// `ipv4.method manual` is NOT usable here: it does not start a DHCP
    /// server, so the Android device associates to the AP but never receives
    /// an IP, resulting in "DHCP error, no internet" on the device.
    pub struct Hotspot {
        con_name: String,
        ssid: String,
    }

    impl Hotspot {
        pub fn start(config: &HotspotConfig) -> io::Result<Self> {
            let interface = config
                .interface
                .clone()
                .or_else(detect_wifi_interface)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "No Wi-Fi interface found. Install NetworkManager \
                         (`nmcli`) or pass --dummy-wifi-interface <iface>",
                    )
                })?;

            // Clean up any stale gnirehtet-dummy-* profile from a previous
            // crashed run before we scan or create anything. Leaving one
            // behind would keep the interface in AP mode and break the scan.
            cleanup_stale_profiles();

            // Compute the SSID: use the caller's override if any, otherwise
            // derive one from the hostname with collision detection.
            let ssid = match config.ssid.as_deref() {
                Some(s) => {
                    info!(target: TAG, "Using explicitly configured SSID '{}'", s);
                    s.to_string()
                }
                None => generate_unique_ssid(&interface)?,
            };

            // Unique connection name so multiple relays don't collide.
            let con_name = format!("gnirehtet-dummy-{}", std::process::id());
            let _ = nmcli(&["connection", "delete", &con_name]);

            // 1. Create the connection profile.
            nmcli(&[
                "connection", "add",
                "type", "wifi",
                "ifname", &interface,
                "con-name", &con_name,
                "autoconnect", "no",
                "ssid", &ssid,
            ])?;

            // 2. AP mode, 2.4 GHz (most compatible band for older Android).
            nmcli(&[
                "connection", "modify", &con_name,
                "802-11-wireless.mode", "ap",
                "802-11-wireless.band", "bg",
                "802-11-wireless.channel", "6",
            ])?;

            // 3. Shared mode: runs dnsmasq for DHCP + DNS. We assign a static
            //    address so the pool is deterministic. IPv6 is disabled to
            //    avoid older ROMs hanging on RA/DHCPv6 that never arrive.
            nmcli(&[
                "connection", "modify", &con_name,
                "ipv4.method", "shared",
                "ipv4.addresses", "192.168.12.1/24",
                "ipv6.method", "disabled",
            ])?;

            // 4. Security: open network by default. The Android client only
            //    auto-connects to networks whose capabilities do not contain
            //    WPA or WEP, so an open AP is required for unattended use.
            if let Some(ref password) = config.password {
                nmcli(&[
                    "connection", "modify", &con_name,
                    "wifi-sec.key-mgmt", "wpa-psk",
                    "wifi-sec.psk", password,
                ])?;
            }

            // 5. Bring the AP up.
            nmcli(&["connection", "up", &con_name])?;

            info!(
                target: TAG,
                "Dummy Wi-Fi '{}' started on {} (open network, DHCP via dnsmasq)",
                ssid, interface
            );

            Ok(Self { con_name, ssid })
        }

        /// The actual SSID being broadcast. May differ from the requested one
        /// when auto-generation was in effect and a collision occurred.
        pub fn ssid(&self) -> &str {
            &self.ssid
        }

        pub fn stop(&self) -> io::Result<()> {
            let _ = nmcli(&["connection", "down", &self.con_name]);
            let _ = nmcli(&["connection", "delete", &self.con_name]);
            info!(target: TAG, "Dummy Wi-Fi '{}' stopped", self.ssid);
            Ok(())
        }
    }

    impl Drop for Hotspot {
        fn drop(&mut self) {
            let _ = self.stop();
        }
    }

    // ────────────────────────────────────────────────────────────────────
    // SSID generation
    // ────────────────────────────────────────────────────────────────────

    /// Generate a unique SSID of the form `gnirehtet-<5 hex>`, checking
    /// against nearby `gnirehtet-*` networks and randomizing on collision.
    fn generate_unique_ssid(interface: &str) -> io::Result<String> {
        // Scan for existing gnirehtet-* SSIDs in range before we take over
        // the interface. If the scan fails (rate-limited, interface busy),
        // we proceed with an empty list — a collision would just degrade
        // reconnection reliability, not correctness.
        let existing = scan_nearby_gnirehtet_ssids(interface);
        if !existing.is_empty() {
            debug!(target: TAG, "Nearby gnirehtet SSIDs: {:?}", existing);
        }

        // Base candidate: derived from the hostname, so it is stable across
        // restarts on the same machine.
        let hostname = get_hostname();
        let base_suffix = fnv1a_5hex(&hostname);
        let base_ssid = format!("{}{}", SSID_PREFIX, base_suffix);
        info!(
            target: TAG,
            "Hostname '{}' maps to base SSID '{}'",
            hostname, base_ssid
        );

        if !existing.iter().any(|s| s == &base_ssid) {
            return Ok(base_ssid);
        }

        // Collision: try random 5-hex suffixes until one is free. Using
        // random rather than base+N avoids lock-step collisions between two
        // hosts whose hostnames hash to adjacent values.
        warn!(
            target: TAG,
            "Base SSID '{}' already in range, trying a random suffix",
            base_ssid
        );
        for attempt in 1..=MAX_COLLISION_ATTEMPTS {
            let suffix = format!("{:05x}", rand::random::<u32>() & 0xF_FFFF);
            let candidate = format!("{}{}", SSID_PREFIX, suffix);
            if !existing.iter().any(|s| s == &candidate) {
                info!(
                    target: TAG,
                    "Using '{}' (attempt {}/{})",
                    candidate, attempt, MAX_COLLISION_ATTEMPTS
                );
                return Ok(candidate);
            }
        }

        Err(io::Error::other(format!(
            "Could not find a unique {}{{XXXXX}} SSID after {} attempts",
            SSID_PREFIX, MAX_COLLISION_ATTEMPTS
        )))
    }

    /// Return the list of nearby SSIDs that start with `gnirehtet-`.
    /// Best-effort: returns an empty list on any error.
    fn scan_nearby_gnirehtet_ssids(interface: &str) -> Vec<String> {
        // Trigger a fresh scan. This fails if a scan was done in the last
        // 30 s (NM rate limit), in which case we fall through and read the
        // cached results from the following `list` call.
        let _ = Command::new("nmcli")
            .args(["device", "wifi", "rescan", "ifname", interface])
            .output();

        // Give the driver a moment to populate the cache.
        std::thread::sleep(Duration::from_millis(1500));

        let output = Command::new("nmcli")
            .args([
                "-t", "-f", "SSID",
                "device", "wifi", "list",
                "ifname", interface,
            ])
            .output();

        match output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|line| line.trim().to_string())
                .filter(|s| !s.is_empty())
                .filter(|s| s.starts_with(SSID_PREFIX) && s.len() <= 20)
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Delete any connection profile whose name starts with
    /// `gnirehtet-dummy-`. These are leftovers from previous relay runs that
    /// crashed or were killed before their `Drop` fired. They must be removed
    /// before we scan, because an active profile keeps the interface in AP
    /// mode and the scan would return nothing.
    fn cleanup_stale_profiles() {
        // Query NAME + DEVICE together so we can also disconnect the interface
        // if it is still up. Deleting the profile alone is not enough on some
        // NetworkManager versions: the device remains in "connected" state
        // (showing the stale SSID) and the interface selector refuses to reuse
        // it for a new hotspot.
        let output = Command::new("nmcli")
            .args(["-t", "-f", "NAME,DEVICE", "connection", "show"])
            .output();
        let Ok(o) = output else { return };
        if !o.status.success() {
            return;
        }
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            // Terse format: NAME:DEVICE. Either side may be empty (unbound
            // profile) or contain `\:` escapes.
            let mut parts = line.splitn(2, ':');
            let name = parts.next().unwrap_or("").trim();
            if !name.starts_with("gnirehtet-dummy-") {
                continue;
            }
            let device = parts.next().unwrap_or("").trim();

            if !device.is_empty() && device != "--" {
                info!(
                    target: TAG,
                    "Disconnecting stale interface '{}' (profile '{}')",
                    device, name
                );
                let _ = Command::new("nmcli")
                    .args(["device", "disconnect", device])
                    .output();
            }

            info!(target: TAG, "Removing stale profile '{}'", name);
            let _ = Command::new("nmcli")
                .args(["connection", "delete", name])
                .output();
        }
    }

    /// Read the machine's hostname. Falls back to `/etc/hostname`, then the
    /// `HOSTNAME` env var, then the literal `"host"`.
    fn get_hostname() -> String {
        if let Ok(s) = std::fs::read_to_string("/etc/hostname")
            && !s.trim().is_empty()
        {
            return s.trim().to_string();
        }
        if let Ok(s) = std::env::var("HOSTNAME")
            && !s.is_empty()
        {
            return s;
        }
        "host".to_string()
    }

    /// FNV-1a hash of `input`, truncated to 20 bits and formatted as 5 hex
    /// characters. Deterministic across machines and runs.
    fn fnv1a_5hex(input: &str) -> String {
        const FNV_OFFSET: u32 = 2_166_136_261;
        const FNV_PRIME: u32 = 16_777_619;
        let mut hash: u32 = FNV_OFFSET;
        for byte in input.as_bytes() {
            hash ^= u32::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        format!("{:05x}", hash & 0xF_FFFF)
    }

    /// Find the first Wi-Fi interface reported by `nmcli device status`.
    fn detect_wifi_interface() -> Option<String> {
        let output = Command::new("nmcli")
            .args(["-t", "-f", "DEVICE,TYPE", "device", "status"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            // `-t` (terse) format: "wlan0:wifi" or "wlan0:wifi:connected"
            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() >= 2 && parts[1] == "wifi" {
                return Some(parts[0].to_string());
            }
        }
        None
    }

    fn nmcli(args: &[&str]) -> io::Result<()> {
        let output = Command::new("nmcli").args(args).output().map_err(|e| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("nmcli not found: {}. Install NetworkManager.", e),
            )
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(io::Error::other(format!(
                "nmcli {:?} failed: {}",
                args,
                stderr.trim()
            )));
        }
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Windows
// ═══════════════════════════════════════════════════════════════════════

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use std::process::Command;

    const SSID_PREFIX: &str = "gnirehtet-";

    /// Windows implementation.
    ///
    /// Two mechanisms, tried in order:
    ///
    /// 1. **`netsh wlan set hostednetwork`** — works on Windows 7 through
    ///    Windows 10 21H1. Deprecated from Windows 10 1803 onward but still
    ///    present on many systems. Requires Administrator privileges. The
    ///    hosted network runs a DHCP server automatically (via ICS).
    ///
    /// 2. **WinRT `NetworkOperatorTetheringManager`** via PowerShell — the
    ///    modern Mobile Hotspot API. Requires Windows 10 1803+ / Windows 11.
    ///    Runs without elevation. Also runs DHCP (via ICS). The SSID is
    ///    whatever the user configured in Settings; we cannot set it from
    ///    the command line, so on this path we ignore the auto-generated
    ///    SSID and log the limitation.
    ///
    /// The dummy network is normally open (no password) because the Android
    /// client's `ConnectivityFallback` only auto-connects to open APs. Note
    /// that `netsh hostednetwork` does not support open networks — it always
    /// requires a WPA2 key — so when that path is used the client must be
    /// told the password via a saved profile, or the WinRT path (which does
    /// support open networks via Settings) must be used.
    pub struct Hotspot {
        ssid: String,
        kind: HotspotKind,
    }

    #[derive(Clone, Copy)]
    enum HotspotKind {
        Netsh,
        WinRt,
    }

    impl Hotspot {
        pub fn start(config: &HotspotConfig) -> io::Result<Self> {
            // Resolve the SSID: use override if given, otherwise generate
            // from hostname. Windows does not have a reliable way to scan
            // for nearby SSIDs synchronously, so no collision detection
            // here — the hostname hash is used as-is.
            let ssid = match config.ssid.as_deref() {
                Some(s) => s.to_string(),
                None => format!("{}{}", SSID_PREFIX, fnv1a_5hex(&get_hostname())),
            };

            // `netsh hostednetwork` always requires a WPA2 key (8–63 chars).
            // Use the configured password if any, otherwise a well-known
            // dummy key.
            let key = config.password.as_deref().unwrap_or("gnirehtet123");

            if try_netsh_start(&ssid, key)? {
                return Ok(Self {
                    ssid,
                    kind: HotspotKind::Netsh,
                });
            }

            if try_winrt_start()? {
                warn!(
                    target: TAG,
                    "Using WinRT Mobile Hotspot; the SSID is whatever is \
                     configured in Settings, not '{}'", ssid
                );
                return Ok(Self {
                    ssid,
                    kind: HotspotKind::WinRt,
                });
            }

            Err(io::Error::other(
                "Cannot create a Wi-Fi hotspot on this Windows version. \
                 Try running the relay as Administrator, or enable Mobile \
                 Hotspot manually from Settings > Network & internet.",
            ))
        }

        pub fn ssid(&self) -> &str {
            &self.ssid
        }

        pub fn stop(&self) -> io::Result<()> {
            match self.kind {
                HotspotKind::Netsh => {
                    let _ = Command::new("netsh")
                        .args(["wlan", "stop", "hostednetwork"])
                        .output();
                    let _ = Command::new("netsh")
                        .args(["wlan", "set", "hostednetwork", "mode=disallow"])
                        .output();
                }
                HotspotKind::WinRt => {
                    let _ = Command::new("powershell")
                        .args([
                            "-ExecutionPolicy", "Bypass",
                            "-Command", WINRT_STOP_SCRIPT,
                        ])
                        .output();
                }
            }
            info!(target: TAG, "Dummy Wi-Fi '{}' stopped", self.ssid);
            Ok(())
        }
    }

    impl Drop for Hotspot {
        fn drop(&mut self) {
            let _ = self.stop();
        }
    }

    fn try_netsh_start(ssid: &str, key: &str) -> io::Result<bool> {
        let set = Command::new("netsh")
            .args([
                "wlan", "set", "hostednetwork",
                "mode=allow",
                &format!("ssid={}", ssid),
                &format!("key={}", key),
            ])
            .output();
        match set {
            Ok(o) if o.status.success() => {}
            _ => return Ok(false),
        }

        let start = Command::new("netsh")
            .args(["wlan", "start", "hostednetwork"])
            .output();
        match start {
            Ok(o) if o.status.success() => {
                info!(target: TAG, "Dummy Wi-Fi '{}' started (hostednetwork)", ssid);
                Ok(true)
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                debug!(target: TAG, "netsh start failed: {}", stderr.trim());
                Ok(false)
            }
            Err(_) => Ok(false),
        }
    }

    fn try_winrt_start() -> io::Result<bool> {
        let output = Command::new("powershell")
            .args([
                "-ExecutionPolicy", "Bypass",
                "-Command", WINRT_START_SCRIPT,
            ])
            .output();
        match output {
            Ok(o) if o.status.success() => {
                info!(target: TAG, "Dummy Wi-Fi started (WinRT Mobile Hotspot)");
                Ok(true)
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                debug!(target: TAG, "WinRT start failed: {}", stderr.trim());
                Ok(false)
            }
            Err(_) => Ok(false),
        }
    }

    fn get_hostname() -> String {
        std::env::var("COMPUTERNAME")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "host".to_string())
    }

    fn fnv1a_5hex(input: &str) -> String {
        const FNV_OFFSET: u32 = 2_166_136_261;
        const FNV_PRIME: u32 = 16_777_619;
        let mut hash: u32 = FNV_OFFSET;
        for byte in input.as_bytes() {
            hash ^= u32::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        format!("{:05x}", hash & 0xF_FFFF)
    }

    const WINRT_START_SCRIPT: &str = r#"
$profile = [Windows.Networking.Connectivity.NetworkInformation,Windows.Networking.Connectivity,ContentType=WindowsRuntime]::GetConnectionProfiles() | Select-Object -First 1
$tether = [Windows.Networking.NetworkOperators.NetworkOperatorTetheringManager,Windows.Networking.NetworkOperators,ContentType=WindowsRuntime]::CreateFromConnectionProfile($profile)
$tether.StartTetheringAsync()
"#;

    const WINRT_STOP_SCRIPT: &str = r#"
$profile = [Windows.Networking.Connectivity.NetworkInformation,Windows.Networking.Connectivity,ContentType=WindowsRuntime]::GetConnectionProfiles() | Select-Object -First 1
$tether = [Windows.Networking.NetworkOperators.NetworkOperatorTetheringManager,Windows.Networking.NetworkOperators,ContentType=WindowsRuntime]::CreateFromConnectionProfile($profile)
$tether.StopTetheringAsync()
"#;
}

// ═══════════════════════════════════════════════════════════════════════
// macOS
// ═══════════════════════════════════════════════════════════════════════

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    /// macOS does not expose a supported API for creating an infrastructure
    /// Wi-Fi AP from the command line. Ad-Hoc networks are possible via
    /// undocumented `airportd` flags, but they are unstable, deprecated in
    /// macOS Sonoma, and do not satisfy Android's active-network check.
    pub struct Hotspot {
        _private: (),
    }

    impl Hotspot {
        pub fn start(_config: &HotspotConfig) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Dummy Wi-Fi hotspot is not supported on macOS. \
                 Connect the Android device to any Wi-Fi network manually, \
                 or use a USB/Bluetooth tethering app.",
            ))
        }

        pub fn ssid(&self) -> &str {
            ""
        }

        pub fn stop(&self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for Hotspot {
        fn drop(&mut self) {}
    }
}

pub use platform::Hotspot;
