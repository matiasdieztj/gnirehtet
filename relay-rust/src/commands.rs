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

use log::*;
use std::io::IsTerminal;
use std::thread;
use std::time::Duration;

use crate::adb::{ensure_adb, exec_adb, get_apk_path, must_install_client};
use crate::adb_monitor::AdbMonitor;
use crate::execution_error::{Cmd, CommandExecutionError, ProcessIoError, ProcessStatusError};
use crate::hotspot::{Hotspot, HotspotConfig};
use crate::wifi;
use relaylib::relay::serial_registry;
use relaylib::relay::tcp_connection;

const TAG: &str = "Main";
const GNIREHTET_PKG: &str = "com.genymobile.gnirehtet";

// ═══════════════════════════════════════════════════════════════════════
// Dummy Wi-Fi setup
// ═══════════════════════════════════════════════════════════════════════

/// Result of the interactive dummy Wi-Fi setup.
///
/// When `hotspot` is `Some`, the hotspot is currently active and will be
/// torn down when this struct is dropped. When `ssid_for_client` is `Some`,
/// that SSID is propagated to the Android client (via the `dummyWifiSsid`
/// intent extra) so its connectivity fallback prefers the correct AP even
/// when several `gnirehtet-*` or open networks are in range.
pub struct DummyWifiOutcome {
    pub hotspot: Option<Hotspot>,
    pub ssid_for_client: Option<String>,
}

impl DummyWifiOutcome {
    fn empty() -> Self {
        Self {
            hotspot: None,
            ssid_for_client: None,
        }
    }
}

/// Interactive setup of the dummy Wi-Fi network.
fn setup_dummy_wifi(
    requested_ssid: Option<&str>,
    requested_interface: Option<&str>,
    force: bool,
) -> DummyWifiOutcome {
    // ── Case 1: explicit interface, no decision needed ──
    if let Some(name) = requested_interface {
        info!(target: TAG, "Using explicitly requested interface '{}'", name);
        return create_hotspot(requested_ssid, Some(name));
    }

    // ── Case 2: enumerate interfaces ──
    let interfaces = wifi::list_wifi_interfaces();

    if interfaces.is_empty() {
        debug!(target: TAG, "No Wi-Fi interface to select from; using platform default");
        return create_hotspot(requested_ssid, None);
    }

        // ── Case 2a/2b: use the shared selector ──
    let (iface, destructive) = wifi::select_interface_for_hotspot(&interfaces)
        .expect("interfaces is non-empty (checked above)");

    if !destructive {
        info!(
            target: TAG,
            "Wi-Fi interfaces available: {}",
            format_interfaces(&interfaces)
        );
        info!(target: TAG, "Using free interface '{}' (host Wi-Fi stays up)", iface.name);
        return create_hotspot(requested_ssid, Some(&iface.name));
    }

    // Destructive: every interface is connected.
    let current_ssid = iface.current_ssid.as_deref().unwrap_or("(unknown SSID)");
    info!(
        target: TAG,
        "All Wi-Fi interfaces are connected; '{}' is on '{}'",
        iface.name, current_ssid
    );

    let aps = wifi::scan_open_aps(&iface.name);

    // ── Filter out the interface's own SSID ──
    // When the interface is currently connected to a network, `nmcli` may
    // report that same network in the scan results — it is our own signal,
    // not a real external AP. Accepting it here would make the relay
    // propagate an SSID that the client cannot associate with (or that it
    // would re-associate with, defeating the purpose of the dummy Wi-Fi).
    //
    // This is what produced the "Using existing open AP '...' as dummy Wi-Fi
    // (no hotspot created)" ghost SSID in earlier runs.
    let iface_own_ssid = iface.current_ssid.as_deref();
    let aps: Vec<wifi::OpenAp> = aps
        .into_iter()
        .filter(|ap| {
            if Some(ap.ssid.as_str()) == iface_own_ssid {
                warn!(
                    target: TAG,
                    "Excluding '{}' from scan results: it is the current SSID of '{}' \
                     (our own signal, not a reusable AP)",
                    ap.ssid, iface.name
                );
                false
            } else if ap.ssid.starts_with("gnirehtet-") {
                // A `gnirehtet-*` network in the scan when we were about to
                // create our own means another gnirehtet host is nearby, or
                // a stale AP is still broadcasting. Either way, prefer to
                // create a fresh hotspot with collision detection rather
                // than hand the client an SSID we do not control.
                info!(
                    target: TAG,
                    "Ignoring nearby '{}' (signal {}%) in reuse path; will create our own hotspot",
                    ap.ssid, ap.signal
                );
                false
            } else {
                true
            }
        })
        .collect();

    if let Some(req) = requested_ssid
        && let Some(ap) = aps.iter().find(|a| a.ssid == req)
    {
        info!(
            target: TAG,
            "Requested SSID '{}' is already broadcasting (signal {}%); using it as-is",
            ap.ssid, ap.signal
        );
        return DummyWifiOutcome {
            hotspot: None,
            ssid_for_client: Some(ap.ssid.clone()),
        };
    }

    if let Some(chosen) = offer_open_ap(&aps) {
        info!(
            target: TAG,
            "Using existing open AP '{}' as dummy Wi-Fi (no hotspot created)",
            chosen
        );
        return DummyWifiOutcome {
            hotspot: None,
            ssid_for_client: Some(chosen),
        };
    }

    if force {
        warn!(
            target: TAG,
            "--dummy-wifi-force: taking over '{}' and disconnecting from '{}'",
            iface.name, current_ssid
        );
        return create_hotspot(requested_ssid, Some(&iface.name));
    }

    if !std::io::stdin().is_terminal() {
        warn!(
            target: TAG,
            "Cannot prompt (no TTY) and --dummy-wifi-force not set. \
             Skipping dummy Wi-Fi setup: the device may need a manual Wi-Fi connection."
        );
        return DummyWifiOutcome::empty();
    }

    let confirmed = prompt_default_no(&format!(
        "The relay needs to take over '{}' (currently connected to '{}'). \
         This will disconnect the host from that network. Continue?",
        iface.name, current_ssid
    ));
    if !confirmed {
        warn!(target: TAG, "User declined. Skipping dummy Wi-Fi setup.");
        return DummyWifiOutcome::empty();
    }

    create_hotspot(requested_ssid, Some(&iface.name))
}

fn create_hotspot(ssid: Option<&str>, interface: Option<&str>) -> DummyWifiOutcome {
    let config = HotspotConfig {
        ssid: ssid.map(|s| s.to_string()),
        interface: interface.map(|s| s.to_string()),
        ..Default::default()
    };
    match Hotspot::start(&config) {
        Ok(h) => {
            let effective_ssid = h.ssid().to_string();
            info!(
                target: TAG,
                "Dummy Wi-Fi '{}' active for the relay lifetime",
                effective_ssid
            );
            // NetworkManager's `connection up` for an AP profile returns when
            // the interface is configured, but the first beacon may take a
            // moment to go out. Clients that associate in the next 2-3
            // seconds may fail with a silent "AP not found". A short delay
            // here is cheap and eliminates that race for everyone.
            std::thread::sleep(std::time::Duration::from_millis(2500));
            DummyWifiOutcome {
                hotspot: Some(h),
                ssid_for_client: Some(effective_ssid),
            }
        }
        Err(e) => {
            warn!(target: TAG, "Failed to start dummy Wi-Fi hotspot: {}", e);
            warn!(
                target: TAG,
                "The device may need a manual Wi-Fi connection for apps to work"
            );
            DummyWifiOutcome::empty()
        }
    }
}

fn offer_open_ap(aps: &[wifi::OpenAp]) -> Option<String> {
    if aps.is_empty() {
        return None;
    }

    if !std::io::stdin().is_terminal() {
        debug!(
            target: TAG,
            "No TTY: auto-selecting strongest open AP '{}'",
            aps[0].ssid
        );
        return Some(aps[0].ssid.clone());
    }

    eprintln!();
    eprintln!(
        "Found {} open Wi-Fi network(s) in range. Instead of taking over the",
        aps.len()
    );
    eprintln!("host's Wi-Fi (which would disconnect it), you can connect the");
    eprintln!("Android device to one of these and let the relay use it as-is:");
    eprintln!();
    for (i, ap) in aps.iter().enumerate() {
        eprintln!("  [{}] {} (signal {}%)", i + 1, ap.ssid, ap.signal);
    }
    eprintln!();

    if aps.len() == 1 {
        if prompt_default_yes(&format!("Use '{}' as dummy Wi-Fi?", aps[0].ssid)) {
            return Some(aps[0].ssid.clone());
        }
        return None;
    }

    eprint!("Choose [1-{}, or 0 to skip]: ", aps.len());
    flush_stderr();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return None;
    }
    let choice: usize = line.trim().parse().unwrap_or(0);
    if (1..=aps.len()).contains(&choice) {
        Some(aps[choice - 1].ssid.clone())
    } else {
        None
    }
}

fn prompt_default_yes(question: &str) -> bool {
    eprint!("{} [Y/n] ", question);
    flush_stderr();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    let answer = line.trim().to_ascii_lowercase();
    answer.is_empty() || answer == "y" || answer == "yes"
}

fn prompt_default_no(question: &str) -> bool {
    eprint!("{} [y/N] ", question);
    flush_stderr();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    let answer = line.trim().to_ascii_lowercase();
    answer == "y" || answer == "yes"
}

fn flush_stderr() {
    use std::io::Write;
    let _ = std::io::stderr().flush();
}

fn format_interfaces(interfaces: &[wifi::WifiInterface]) -> String {
    interfaces
        .iter()
        .map(|i| i.describe())
        .collect::<Vec<_>>()
        .join(", ")
}

// ═══════════════════════════════════════════════════════════════════════
// System detection helpers
// ═══════════════════════════════════════════════════════════════════════

pub fn detect_system_dns() -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(content) = std::fs::read_to_string("/etc/resolv.conf") {
            let servers: Vec<String> = content
                .lines()
                .filter_map(|line| line.trim().strip_prefix("nameserver "))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && *s != "127.0.0.1" && *s != "::1")
                .collect();
            if !servers.is_empty() {
                return servers;
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = std::process::Command::new("scutil").arg("--dns").output() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let servers: Vec<String> = stdout
                .lines()
                .filter_map(|line| {
                    line.trim()
                        .strip_prefix("nameserver[")
                        .and_then(|s| s.split(']').nth(1))
                })
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && *s != "127.0.0.1" && *s != "::1")
                .collect();
            if !servers.is_empty() {
                return servers;
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(output) = std::process::Command::new("powershell")
            .args(["-Command", "(Get-DnsClientServerAddress).ServerAddresses"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let servers: Vec<String> = stdout
                .split_whitespace()
                .filter(|s| !s.is_empty() && *s != "127.0.0.1" && *s != "::1" && *s != "0.0.0.0")
                .map(|s| s.to_string())
                .collect();
            if !servers.is_empty() {
                return servers;
            }
        }
    }
    vec!["8.8.8.8".to_string()]
}

pub fn detect_mtu() -> u16 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(output) = std::process::Command::new("ip")
            .args(["route", "show", "default"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for word in stdout.split_whitespace() {
                if let Some(mtu_str) = word.strip_prefix("mtu")
                    && let Ok(mtu) = mtu_str.trim().parse::<u16>()
                {
                    return mtu.max(1280);
                }
            }
        }
    }
    0x4000
}

// ═══════════════════════════════════════════════════════════════════════
// Install / uninstall / stop / tunnel
// ═══════════════════════════════════════════════════════════════════════

/// Grant runtime permissions the client needs, so the user does not have to
/// answer system dialogs on every launch.
///
/// ACCESS_FINE_LOCATION and ACCESS_COARSE_LOCATION are required by
/// WifiManager.getScanResults() and WifiInfo.getSSID() on API 23+. Without
/// them the SSID is silently redacted as "<unknown ssid>" and scan results
/// come back empty, which makes the preferred-SSID fallback time out
/// without any diagnostic.
///
/// Each permission is granted at two levels. `pm grant` sets the
/// package-level op. On some ROMs (notably Samsung) the UID-level appops
/// mode is also set to "foreground", which overrides the package op and
/// blocks the grant whenever the code runs from a background thread of a
/// foreground service — which is exactly how ConnectivityFallback runs.
/// `appops set --uid` clears that restriction.
///
/// POST_NOTIFICATIONS only exists on API 33+, where it must be granted
/// at runtime before the foreground-service notification can be shown.
///
/// Best-effort: each command's outcome is logged so a silent failure is
/// visible in the relay log.
fn grant_runtime_permissions(serial: Option<&str>, sdk: u32) {
    if sdk >= 23 {
        for perm in &[
            "android.permission.ACCESS_FINE_LOCATION",
            "android.permission.ACCESS_COARSE_LOCATION",
        ] {
            debug!(target: TAG, "Granting {} (package + uid)", perm);
            if let Err(e) = exec_adb(serial, vec![
                "shell", "pm", "grant", GNIREHTET_PKG, perm,
            ]) {
                warn!(target: TAG, "pm grant {} failed: {}", perm, e);
            }
            // Strip the "android.permission." prefix to get the appops
            // op name (e.g. ACCESS_FINE_LOCATION).
            let op = perm.strip_prefix("android.permission.").unwrap_or(perm);
            if let Err(e) = exec_adb(serial, vec![
                "shell", "appops", "set", "--uid", GNIREHTET_PKG, op, "allow",
            ]) {
                warn!(target: TAG, "appops --uid {} failed: {}", op, e);
            }
        }
    }
    if sdk >= 33 {
        let _ = exec_adb(serial, vec![
            "shell", "pm", "grant", GNIREHTET_PKG,
            "android.permission.POST_NOTIFICATIONS",
        ]);
    }
}

/// Pre-authorize VPN creation via `appops`, suppressing the system
/// "Connection request" dialog that otherwise blocks the client on the
/// first launch after install.
///
/// Best-effort: some ROMs restrict WRITE_SECURE_SETTINGS to root, in which
/// case the dialog still appears and the user must tap "OK" once.
fn grant_vpn_permission(serial: Option<&str>) {
    let _ = exec_adb(serial, vec![
        "shell", "appops", "set", GNIREHTET_PKG, "ACTIVATE_VPN", "allow",
    ]);
}

pub fn cmd_install(serial: Option<&str>) -> Result<(), CommandExecutionError> {
    let apk_path = get_apk_path();
    info!(target: TAG, "Installing gnirehtet client...");
    let adb = crate::adb::get_adb_path();
    let args = crate::adb::create_adb_args(serial, vec!["install", "-r", &apk_path]);
    let cmd_obj = Cmd::new(adb.clone(), args.clone());
    match std::process::Command::new(&adb).args(&args).output() {
        Ok(output) => {
            if output.status.success() {
                Ok(())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!(target: TAG, "Install (-r) failed, attempting clean reinstall: {}", stderr.trim());
                let _ = cmd_uninstall(serial);
                let retry_args = crate::adb::create_adb_args(serial, vec!["install", &apk_path]);
                let retry_cmd = Cmd::new(adb.clone(), retry_args.clone());
                match std::process::Command::new(&adb).args(&retry_args).output() {
                    Ok(retry_output) => {
                        if retry_output.status.success() {
                            info!(target: TAG, "Reinstall succeeded after clean uninstall.");
                            Ok(())
                        } else {
                            let retry_stderr = String::from_utf8_lossy(&retry_output.stderr);
                            if retry_stderr.contains("INSTALL_FAILED") || stderr.contains("INSTALL_FAILED") {
                                eprintln!(
                                    "Tip: Make sure USB debugging is enabled on your device and check for a confirmation dialog on the device screen."
                                );
                            }
                            Err(ProcessStatusError::new(retry_cmd, retry_output.status).into())
                        }
                    }
                    Err(err) => Err(ProcessIoError::new(retry_cmd, err).into()),
                }
            }
        }
        Err(err) => Err(ProcessIoError::new(cmd_obj, err).into()),
    }
}

pub fn cmd_uninstall(serial: Option<&str>) -> Result<(), CommandExecutionError> {
    info!(target: TAG, "Uninstalling gnirehtet client...");
    exec_adb(serial, vec!["uninstall", GNIREHTET_PKG])
}

pub fn cmd_reinstall(serial: Option<&str>) -> Result<(), CommandExecutionError> {
    cmd_uninstall(serial)?;
    cmd_install(serial)?;
    Ok(())
}

pub fn cmd_stop(serial: Option<&str>) -> Result<(), CommandExecutionError> {
    info!(target: TAG, "Stopping client...");
    exec_adb(
        serial,
        vec![
            "shell",
            "am",
            "start",
            "-a",
            "com.genymobile.gnirehtet.STOP",
            "-n",
            "com.genymobile.gnirehtet/.GnirehtetActivity",
        ],
    )
}

pub fn cmd_tunnel(
    serial: Option<&str>,
    port: u16,
    transport: crate::transport::TransportMode,
) -> Result<(), CommandExecutionError> {
    use crate::transport::TransportMode;

    match transport {
        TransportMode::Reverse => exec_adb(
            serial,
            vec![
                "reverse".to_string(),
                "localabstract:gnirehtet".to_string(),
                format!("tcp:{}", port),
            ],
        ),
        TransportMode::Forward => {
            let spec = format!("tcp:{}", port);
            let adb = crate::adb::get_adb_path();
            let mut cmd = std::process::Command::new(&adb);
            if let Some(s) = serial {
                cmd.args(["-s", s]);
            }
            let _ = cmd
                .args(["forward", "--remove", &spec])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            exec_adb(serial, vec!["forward".to_string(), spec.clone(), spec])
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Relay
// ═══════════════════════════════════════════════════════════════════════

#[allow(clippy::too_many_arguments)]
pub fn cmd_relay(
    port: u16,
    transport: crate::transport::TransportMode,
    serial: Option<String>,
    dummy_wifi: bool,
    dummy_wifi_ssid: Option<&str>,
    dummy_wifi_interface: Option<&str>,
    dummy_wifi_force: bool,
) -> Result<(), CommandExecutionError> {
    use crate::transport::TransportMode;

    let outcome = if dummy_wifi {
        setup_dummy_wifi(dummy_wifi_ssid, dummy_wifi_interface, dummy_wifi_force)
    } else {
        DummyWifiOutcome::empty()
    };

    let result: Result<(), CommandExecutionError> = match transport {
        TransportMode::Reverse => {
            info!(target: TAG, "Starting relay server in reverse mode on port {}...", port);
            relaylib::relay(port).map_err(CommandExecutionError::from)
        }
        TransportMode::Forward => {
            info!(target: TAG, "Starting relay in forward mode on port {}...", port);
            relaylib::relay::Relay::new(port)
                .run_forward(serial)
                .map_err(CommandExecutionError::from)
        }
    };

    if let Some(ref h) = outcome.hotspot
        && let Err(e) = h.stop()
    {
        warn!(target: TAG, "Failed to stop dummy Wi-Fi hotspot: {}", e);
    }

    result
}

// ═══════════════════════════════════════════════════════════════════════
// Client start
// ═══════════════════════════════════════════════════════════════════════

fn effective_serial(serial: Option<&str>) -> Option<String> {
    if let Some(s) = serial {
        return Some(s.to_string());
    }
    let adb = crate::adb::get_adb_path();
    let out = std::process::Command::new(&adb).args(["devices"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let devices: Vec<String> = stdout
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let serial = parts.next()?;
            let state = parts.next()?;
            if state == "device" {
                Some(serial.to_string())
            } else {
                None
            }
        })
        .collect();
    if devices.len() == 1 {
        Some(devices[0].clone())
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_start(
    serial: Option<&str>,
    dns_servers: Option<&str>,
    routes: Option<&str>,
    port: u16,
    proxy: Option<&str>,
    proxy_exclusions: Option<&str>,
    mtu: u16,
    allow_apps: &[String],
    deny_apps: &[String],
    socks5: Option<&str>,
    transport: crate::transport::TransportMode,
    dummy_wifi_ssid: Option<&str>,
) -> Result<(), CommandExecutionError> {
    ensure_adb();
    if must_install_client(serial)? {
        cmd_install(serial)?;
        // Wait for the package manager to fully index the newly installed
        // package before granting permissions. On Samsung ROMs, `pm grant`
        // issued less than ~1 s after `pm install` returns fails silently
        // with a "package does not have permission" style error, and the
        // app then starts without ACCESS_FINE_LOCATION — which makes every
        // WiFi SSID query return "<unknown ssid>".
        thread::sleep(Duration::from_millis(1500));
    }

    // Grant permissions on every launch. Cheap and idempotent, and it
    // ensures the grants are in place even if the app was reinstalled
    // outside of this relay's control.
    let sdk = crate::transport::query_device_sdk(serial).unwrap_or(0);
    grant_runtime_permissions(serial, sdk);
    grant_vpn_permission(serial);

    info!(target: TAG, "Starting client...");
    if let Some(ssid) = dummy_wifi_ssid {
        info!(target: TAG, "Client will prefer dummy Wi-Fi SSID '{}'", ssid);
    }

    let supports_array_extras = sdk >= 21;
    if !supports_array_extras && (dns_servers.is_some() || routes.is_some()
        || proxy_exclusions.is_some() || !allow_apps.is_empty() || !deny_apps.is_empty())
    {
        warn!(
            target: TAG,
            "Device SDK {} < 21: --esa not supported, ignoring array extras \
             (DNS, routes, proxy exclusions, allow/deny apps). \
             The client will use its built-in defaults.",
            sdk
        );
    }

    if let Some(s) = effective_serial(serial) {
        serial_registry::register_pending(&s);
        debug!(target: TAG, "Registered serial {} for the next relay connection", s);
    }

    cmd_tunnel(serial, port, transport)?;
    
    // Disable captive-portal detection so the device does not show the
    // "Sign in to network" notification when connected to an open AP that
    // requires a portal. This is best-effort: if the setting is not writable
    // (older Android without root), the command fails silently and the
    // behaviour is unchanged.
    if sdk >= 25 {
        let _ = exec_adb(serial, vec![
            "shell", "settings", "put", "global", "captive_portal_mode", "0",
        ]);
    } else if sdk >= 19 {
        let _ = exec_adb(serial, vec![
            "shell", "settings", "put", "global", "captive_portal_detection_enabled", "0",
        ]);
    }

    let mut adb_args: Vec<String> = vec![
        "shell".into(),
        "am".into(),
        "start".into(),
        "-a".into(),
        "com.genymobile.gnirehtet.START".into(),
        "-n".into(),
        "com.genymobile.gnirehtet/.GnirehtetActivity".into(),
    ];
    if supports_array_extras && let Some(dns_servers) = dns_servers {
        adb_args.push("--esa".into());
        adb_args.push("dnsServers".into());
        adb_args.push(dns_servers.into());
    }
    if supports_array_extras && let Some(routes) = routes {
        adb_args.push("--esa".into());
        adb_args.push("routes".into());
        adb_args.push(routes.into());
    }
    if let Some(proxy) = proxy {
        adb_args.push("--es".into());
        adb_args.push("proxyHostPort".into());
        adb_args.push(proxy.into());
    }
    if supports_array_extras && let Some(exclusions) = proxy_exclusions {
        adb_args.push("--esa".into());
        adb_args.push("proxyExclusionList".into());
        adb_args.push(exclusions.into());
    }
    if let Some(socks5_host_port) = socks5 {
        adb_args.push("--es".into());
        adb_args.push("socks5Proxy".into());
        adb_args.push(socks5_host_port.into());
    }
    if let Some(ssid) = dummy_wifi_ssid {
        adb_args.push("--es".into());
        adb_args.push("dummyWifiSsid".into());
        adb_args.push(ssid.into());
    }
    adb_args.push("--ei".into());
    adb_args.push("mtu".into());
    adb_args.push(mtu.to_string());
    if supports_array_extras && !allow_apps.is_empty() {
        adb_args.push("--esa".into());
        adb_args.push("allowApps".into());
        adb_args.push(allow_apps.join(","));
    }
    if supports_array_extras && !deny_apps.is_empty() {
        adb_args.push("--esa".into());
        adb_args.push("denyApps".into());
        adb_args.push(deny_apps.join(","));
    }
    exec_adb(serial, adb_args)
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_autostart(
    dns_servers: Option<&str>,
    routes: Option<&str>,
    port: u16,
    mtu: u16,
    allow_wifi: bool,
    socks5: Option<&str>,
    dummy_wifi_ssid: Option<&str>,
) -> Result<(), CommandExecutionError> {
    let start_dns_servers = dns_servers.map(String::from);
    let start_routes = routes.map(String::from);
    let start_socks5 = socks5.map(String::from);
    let start_dummy_wifi_ssid = dummy_wifi_ssid.map(String::from);
    let mut adb_monitor = AdbMonitor::new(Box::new(move |serial: &str| {
        let dns_servers = start_dns_servers.as_ref().map(String::as_ref);
        let routes = start_routes.as_ref().map(String::as_ref);
        let socks5 = start_socks5.as_ref().map(String::as_ref);
        let dummy_wifi_ssid = start_dummy_wifi_ssid.as_ref().map(String::as_ref);
        async_start(
            Some(serial),
            dns_servers,
            routes,
            port,
            None,
            None,
            mtu,
            &[],
            &[],
            socks5,
            crate::transport::TransportMode::Reverse,
            dummy_wifi_ssid,
        )
    }));
    adb_monitor.set_usb_only(!allow_wifi);
    adb_monitor.monitor();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn async_start(
    serial: Option<&str>,
    dns_servers: Option<&str>,
    routes: Option<&str>,
    port: u16,
    proxy: Option<&str>,
    proxy_exclusions: Option<&str>,
    mtu: u16,
    allow_apps: &[String],
    deny_apps: &[String],
    socks5: Option<&str>,
    transport: crate::transport::TransportMode,
    dummy_wifi_ssid: Option<&str>,
) {
    let start_serial = serial.map(String::from);
    let start_dns_servers = dns_servers.map(String::from);
    let start_routes = routes.map(String::from);
    let start_proxy = proxy.map(String::from);
    let start_exclusions = proxy_exclusions.map(String::from);
    let start_socks5 = socks5.map(String::from);
    let start_dummy_wifi_ssid = dummy_wifi_ssid.map(String::from);
    let allow_apps_owned = allow_apps.to_vec();
    let deny_apps_owned = deny_apps.to_vec();
    thread::spawn(move || {
        let serial = start_serial.as_ref().map(String::as_ref);
        let dns_servers = start_dns_servers.as_ref().map(String::as_ref);
        let routes = start_routes.as_ref().map(String::as_ref);
        let proxy = start_proxy.as_ref().map(String::as_ref);
        let exclusions = start_exclusions.as_ref().map(String::as_ref);
        let socks5 = start_socks5.as_ref().map(String::as_ref);
        let dummy_wifi_ssid = start_dummy_wifi_ssid.as_ref().map(String::as_ref);
        if let Err(err) = cmd_start(
            serial,
            dns_servers,
            routes,
            port,
            proxy,
            exclusions,
            mtu,
            &allow_apps_owned,
            &deny_apps_owned,
            socks5,
            transport,
            dummy_wifi_ssid,
        ) {
            crate::execution_error::print_error(&err);
        }
    });
}

// ═══════════════════════════════════════════════════════════════════════
// run / autorun / restart
// ═══════════════════════════════════════════════════════════════════════

#[allow(clippy::too_many_arguments)]
pub fn cmd_run(
    serial: Option<&str>,
    dns_servers: Option<&str>,
    routes: Option<&str>,
    port: u16,
    proxy: Option<&str>,
    proxy_exclusions: Option<&str>,
    _stop_on_disconnect: bool,
    mtu: u16,
    allow_apps: &[String],
    deny_apps: &[String],
    socks5: Option<&str>,
    transport_pref: crate::transport::TransportPreference,
    dummy_wifi: bool,
    dummy_wifi_ssid: Option<&str>,
    dummy_wifi_interface: Option<&str>,
    dummy_wifi_force: bool,
) -> Result<(), CommandExecutionError> {
    if let Some(proxy) = socks5
        && let Ok(addr) = proxy.parse::<std::net::SocketAddr>()
    {
        let _ = tcp_connection::SOCKS5_PROXY.set(addr);
    }

    let transport = transport_pref.resolve(serial);
    info!(target: TAG, "Using transport mode: {:?}", transport);

    let outcome = if dummy_wifi {
        setup_dummy_wifi(dummy_wifi_ssid, dummy_wifi_interface, dummy_wifi_force)
    } else {
        DummyWifiOutcome::empty()
    };

    async_start(
        serial, dns_servers, routes, port, proxy, proxy_exclusions,
        mtu, allow_apps, deny_apps, socks5,
        transport,
        outcome.ssid_for_client.as_deref(),
    );

    let ctrlc_serial = serial.map(String::from);
    let relay_serial = serial.map(String::from);
    let rt = tokio::runtime::Runtime::new().map_err(|e| CommandExecutionError::Io(std::io::Error::other(e)))?;

    // ── Run the relay in a plain OS thread, not spawn_blocking. ──
    // The relay's accept loop is infinite and cannot be interrupted: on
    // Ctrl+C, the tokio Runtime's drop would wait forever for the blocking
    // task to finish, hanging the process and the shell. A detached OS
    // thread is fine — we exit hard below, and the OS reclaims everything.
    let relay_thread = std::thread::spawn(move || {
        cmd_relay(port, transport, relay_serial, false, None, None, false)
    });

    rt.block_on(async {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!(target: TAG, "Interrupted");
                if let Err(err) = cmd_stop(ctrlc_serial.as_deref()) {
                    error!(target: TAG, "Cannot stop client: {}", err);
                }
            }
            _ = async {
                while !relay_thread.is_finished() {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            } => {
                debug!(target: TAG, "Relay exited on its own");
            }
        }
    });

    // If the relay thread already finished (which is the reason we got
    // here in the "exited on its own" branch), collect its result and
    // log any error. Without this, a `cmd_relay` early-return (e.g. bind
    // failure on port 31416) would be silent: the thread would just end,
    // the hotspot would be torn down, and the process would exit without
    // any diagnostic.
    //
    // Do NOT join unconditionally: in the Ctrl+C branch the thread is
    // still blocked in accept() and joining would hang forever. Only
    // join when the thread has already finished, which is a safe
    // non-blocking operation.
    if relay_thread.is_finished() {
        match relay_thread.join() {
            Ok(Ok(())) => {}
            Ok(Err(err)) => error!(target: TAG, "Relay exited with error: {}", err),
            Err(_) => error!(target: TAG, "Relay thread panicked"),
        }
    }

    // ── Teardown ──
    if let Some(ref h) = outcome.hotspot
        && let Err(e) = h.stop()
    {
        warn!(target: TAG, "Failed to stop dummy Wi-Fi hotspot: {}", e);
    }

    // The relay thread may still be blocked in accept(). Do NOT join it:
    // that would hang. std::process::exit kills it safely — the hotspot
    // has already been stopped above, and cmd_stop has already told the
    // device to tear down the VPN.
    std::process::exit(0);
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_autorun(
    dns_servers: Option<&str>,
    routes: Option<&str>,
    port: u16,
    _stop_on_disconnect: bool,
    mtu: u16,
    allow_wifi: bool,
    socks5: Option<&str>,
    transport_pref: crate::transport::TransportPreference,
    dummy_wifi: bool,
    dummy_wifi_ssid: Option<&str>,
    dummy_wifi_force: bool,
) -> Result<(), CommandExecutionError> {
    use crate::transport::TransportMode;
    let transport = transport_pref.resolve(None);
    if transport == TransportMode::Forward {
        return Err(CommandExecutionError::Io(std::io::Error::other(
            "forward transport is not supported by 'autorun' (multi-device); \
             use 'run --transport=forward --serial <SERIAL>' instead",
        )));
    }

    if let Some(proxy) = socks5
        && let Ok(addr) = proxy.parse::<std::net::SocketAddr>()
    {
        let _ = tcp_connection::SOCKS5_PROXY.set(addr);
    }

    let outcome = if dummy_wifi {
        setup_dummy_wifi(dummy_wifi_ssid, None, dummy_wifi_force)
    } else {
        DummyWifiOutcome::empty()
    };

    {
        let autostart_dns_servers = dns_servers.map(String::from);
        let autostart_routes = routes.map(String::from);
        let autostart_socks5 = socks5.map(String::from);
        let autostart_ssid = outcome.ssid_for_client.clone();
        thread::spawn(move || {
            let dns_servers = autostart_dns_servers.as_ref().map(String::as_ref);
            let routes = autostart_routes.as_ref().map(String::as_ref);
            let socks5 = autostart_socks5.as_ref().map(String::as_ref);
            let ssid = autostart_ssid.as_ref().map(String::as_ref);
            if let Err(err) = cmd_autostart(dns_servers, routes, port, mtu, allow_wifi, socks5, ssid) {
                error!(target: TAG, "Cannot auto start clients: {}", err);
            }
        });
    }

    // ── Run the relay in a plain OS thread, same pattern as cmd_run. ──
    // The accept loop is infinite and cannot be interrupted: on Ctrl+C,
    // the default SIGINT handler would kill the process without running
    // teardown, leaving the client's VPN up and the port bound. Waiting
    // on a plain thread lets us catch the signal, stop every connected
    // client, stop the hotspot, and exit cleanly.
    let relay_thread = std::thread::spawn(move || {
        cmd_relay(port, transport, None, false, None, None, false)
    });

    let rt = tokio::runtime::Runtime::new()
        .map_err(|e| CommandExecutionError::Io(std::io::Error::other(e)))?;

    rt.block_on(async {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!(target: TAG, "Interrupted");
                stop_all_connected_devices();
            }
            _ = async {
                while !relay_thread.is_finished() {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            } => {
                debug!(target: TAG, "Relay exited on its own");
            }
        }
    });

    // ── Teardown ──
    if let Some(ref h) = outcome.hotspot
        && let Err(e) = h.stop()
    {
        warn!(target: TAG, "Failed to stop dummy Wi-Fi hotspot: {}", e);
    }

    // The relay thread may still be blocked in accept(). Do NOT join it:
    // that would hang. std::process::exit kills it safely — the hotspot
    // has already been stopped above, and stop_all_connected_devices()
    // has already told every device to tear down the VPN.
    std::process::exit(0);
}

/// Send the STOP intent to every device currently visible to ADB.
///
/// Used by `cmd_autorun` (which manages multiple devices) on Ctrl+C,
/// so that the VPN is torn down cleanly on each of them. Best-effort:
/// failures are logged but do not abort the teardown.
fn stop_all_connected_devices() {
    let adb = crate::adb::get_adb_path();
    let output = std::process::Command::new(&adb)
        .args(["devices"])
        .output();
    let Ok(output) = output else { return };
    if !output.status.success() {
        return;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines().skip(1) {
        let mut parts = line.split_whitespace();
        let Some(serial) = parts.next() else { continue };
        let Some("device") = parts.next() else { continue };
        info!(target: TAG, "Stopping client on {}", serial);
        if let Err(err) = cmd_stop(Some(serial)) {
            warn!(target: TAG, "Cannot stop client on {}: {}", serial, err);
        }
    }
}

pub fn cmd_restart(
    serial: Option<&str>,
    dns_servers: Option<&str>,
    routes: Option<&str>,
    port: u16,
    transport_pref: crate::transport::TransportPreference,
) -> Result<(), CommandExecutionError> {
    let transport = transport_pref.resolve(serial);
    cmd_stop(serial)?;
    cmd_start(
        serial,
        dns_servers,
        routes,
        port,
        None,
        None,
        0x4000,
        &[],
        &[],
        None,
        transport,
        None,
    )?;
    Ok(())
}
