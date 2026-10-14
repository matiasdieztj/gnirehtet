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

//! Wi-Fi interface enumeration and open-AP scanning for the relay host.
//!
//! The relay uses this module to decide where to create the dummy Wi-Fi
//! hotspot:
//!
//! 1. Enumerate the Wi-Fi interfaces on the host.
//! 2. Pick a non-connected one, so the host's own Wi-Fi stays online.
//! 3. If only one interface exists and it is connected, offer to use an
//!    existing open AP (non-destructive) instead of taking it over.
//!
//! Linux-only via `nmcli`. Windows and macOS return empty lists — on those
//! platforms `hotspot.rs` uses different mechanisms that do not expose
//! interface enumeration.

use log::*;

const TAG: &str = "Wifi";

/// A Wi-Fi interface visible to the OS.
#[derive(Clone, Debug)]
pub struct WifiInterface {
    /// Kernel interface name, e.g. `wlan0`.
    pub name: String,
    /// True if the interface currently has an active connection.
    pub connected: bool,
    /// SSID of the active connection, if any.
    pub current_ssid: Option<String>,
}

impl WifiInterface {
    /// Human-readable description for logs and prompts.
    pub fn describe(&self) -> String {
        match (&self.current_ssid, self.connected) {
            (Some(ssid), _) => format!("{} (connected to {})", self.name, ssid),
            (None, true) => format!("{} (connected)", self.name),
            (None, false) => format!("{} (disconnected)", self.name),
        }
    }
}

/// An open (no WPA/WEP) access point visible to a given interface.
#[derive(Clone, Debug)]
pub struct OpenAp {
    pub ssid: String,
    /// Signal strength in percent (0–100), as reported by nmcli.
    pub signal: i32,
}

/// Pick the interface to use for the hotspot.
///
/// Prefers a non-connected interface, so the host's Wi-Fi keeps working.
/// Returns `(interface, destructive)`:
///
/// - `destructive = false` when a free interface was found.
/// - `destructive = true` when every interface is connected, in which case
///   the caller should warn the user that the current connection will drop.
///
/// Returns `None` if the list is empty.
pub fn select_interface_for_hotspot(
    interfaces: &[WifiInterface],
) -> Option<(&WifiInterface, bool)> {
    if interfaces.is_empty() {
        return None;
    }

    // A Wi-Fi interface that is currently connected to one of our own
    // `gnirehtet-*` networks is treated as effectively free: it is a
    // leftover from a previous relay run (its Drop did not fire, or the
    // profile was deleted without the interface being disconnected).
    // Reusing it is safe because the SSID belongs to us, not to the user.
    //
    // Without this, a stale `gnirehtet-dummy-XXXX` profile makes the
    // selector believe there is no free interface, and the caller falls
    // through to the "reuse existing AP" path, which then propagates an
    // SSID that was never actually broadcast.
    let is_free = |i: &WifiInterface| -> bool {
        if !i.connected {
            return true;
        }
        matches!(
            &i.current_ssid,
            Some(ssid) if ssid.starts_with("gnirehtet-")
        )
    };

    if let Some(iface) = interfaces.iter().find(|i| is_free(i)) {
        // `destructive = false` means the caller will not prompt the user
        // before taking over this interface.
        return Some((iface, false));
    }

    Some((&interfaces[0], true))
}

// ═══════════════════════════════════════════════════════════════════════
// Linux (nmcli)
// ═══════════════════════════════════════════════════════════════════════

#[cfg(target_os = "linux")]
pub fn list_wifi_interfaces() -> Vec<WifiInterface> {
    use std::process::Command;

    let output = match Command::new("nmcli")
        .args(["-t", "-f", "DEVICE,TYPE,STATE,CONNECTION", "device", "status"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            debug!(target: TAG, "nmcli device status failed: {}", stderr.trim());
            return Vec::new();
        }
        Err(e) => {
            debug!(target: TAG, "Cannot run nmcli: {}", e);
            return Vec::new();
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().filter_map(parse_nmcli_device_line).collect()
}

#[cfg(target_os = "linux")]
fn parse_nmcli_device_line(line: &str) -> Option<WifiInterface> {
    // Terse format: DEVICE:TYPE:STATE:CONNECTION
    // Colons in CONNECTION are escaped as \:
    // Examples:
    //   wlan0:wifi:connected:Profesores 02
    //   wlan1:wifi:disconnected:
    //   eth0:ethernet:connected:Wired connection 1
    let mut parts = line.splitn(4, ':');
    let name = parts.next()?;
    let kind = parts.next()?;
    let state = parts.next()?;
    let connection = parts.next().unwrap_or("");

    if kind != "wifi" {
        return None;
    }
    let connected = state == "connected";
    let current_ssid = if connected && !connection.is_empty() {
        Some(connection.replace("\\:", ":"))
    } else {
        None
    };
    Some(WifiInterface {
        name: name.to_string(),
        connected,
        current_ssid,
    })
}

#[cfg(target_os = "linux")]
pub fn scan_open_aps(interface: &str) -> Vec<OpenAp> {
    use std::process::Command;
    use std::time::Duration;

    // Trigger a rescan. This fails if a scan was done in the last 30 s (NM
    // rate limit), in which case we fall through and read the cached results
    // from the following `list` call.
    let _ = Command::new("nmcli")
        .args(["device", "wifi", "rescan", "ifname", interface])
        .output();
    std::thread::sleep(Duration::from_millis(1500));

    let output = match Command::new("nmcli")
        .args([
            "-t", "-f", "SSID,SIGNAL,SECURITY",
            "device", "wifi", "list",
            "ifname", interface,
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut aps: Vec<OpenAp> = stdout.lines().filter_map(parse_nmcli_ap_line).collect();
    // Highest signal first, so the caller can offer the strongest.
    aps.sort_by_key(|a| std::cmp::Reverse(a.signal));
    aps
}

#[cfg(target_os = "linux")]
fn parse_nmcli_ap_line(line: &str) -> Option<OpenAp> {
    // Terse format: SSID:SIGNAL:SECURITY
    // `rsplitn` because SSIDs may contain colons (nmcli escapes them as \:).
    // The last two fields never contain a colon.
    let mut parts = line.rsplitn(3, ':');
    let security = parts.next().unwrap_or("");
    let signal_str = parts.next().unwrap_or("0");
    let ssid_raw = parts.next().unwrap_or("");

    if ssid_raw.is_empty() {
        // Hidden network: cannot be offered as a target.
        return None;
    }
    // Empty or "--" means no security. Anything else (WPA1, WPA2, WEP, ...)
    // is not an open network, and the Android client's fallback only
    // auto-connects to open APs.
    let is_open = security.is_empty() || security == "--";
    if !is_open {
        return None;
    }
    let signal: i32 = signal_str.parse().unwrap_or(0);
    Some(OpenAp {
        ssid: ssid_raw.replace("\\:", ":"),
        signal,
    })
}

// ═══════════════════════════════════════════════════════════════════════
// Windows and macOS: not implemented
// ═══════════════════════════════════════════════════════════════════════

#[cfg(not(target_os = "linux"))]
pub fn list_wifi_interfaces() -> Vec<WifiInterface> {
    // Windows: `netsh wlan set hostednetwork` does not expose interface
    // selection — the system picks one automatically. macOS: hotspots are
    // not supported at all. Return empty so the caller falls back to
    // hotspot.rs's built-in detection.
    debug!(target: TAG, "Wi-Fi interface enumeration not implemented on this platform");
    Vec::new()
}

#[cfg(not(target_os = "linux"))]
pub fn scan_open_aps(_interface: &str) -> Vec<OpenAp> {
    Vec::new()
}
