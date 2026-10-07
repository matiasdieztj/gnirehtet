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
use std::thread;
use std::time::Duration;

use crate::adb::{ensure_adb, exec_adb, get_apk_path, must_install_client};
use crate::adb_monitor::AdbMonitor;
use crate::execution_error::{Cmd, CommandExecutionError, ProcessIoError, ProcessStatusError};
use relaylib::relay::serial_registry;
use relaylib::relay::tcp_connection;

const TAG: &str = "Main";

/// Detect system DNS servers by parsing OS-specific configuration.
pub fn detect_system_dns() -> Vec<String> {
    // Linux: parse /etc/resolv.conf for nameserver entries
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
    // macOS: use scutil --dns
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
    // Windows: use powershell to query DNS client server addresses
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
    // Fallback
    vec!["8.8.8.8".to_string()]
}

/// Detect the MTU of the default route interface; falls back to 16384.
pub fn detect_mtu() -> u16 {
    // Try to detect interface MTU; fall back to 16384
    #[cfg(target_os = "linux")]
    {
        // Use `ip route` to find the default route's MTU
        if let Ok(output) = std::process::Command::new("ip")
            .args(["route", "show", "default"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            // Parse "default via X dev Y ... mtu N"
            for word in stdout.split_whitespace() {
                if let Some(mtu_str) = word.strip_prefix("mtu")
                    && let Ok(mtu) = mtu_str.trim().parse::<u16>()
                {
                    return mtu.max(1280); // minimum MTU for IPv6
                }
            }
        }
    }
    0x4000 // default 16384
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
    exec_adb(serial, vec!["uninstall", "com.genymobile.gnirehtet"])
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
            // Remove any stale forward on this port before creating a new
            // one. A leftover forward from a previous run accepts the
            // relay's TCP handshake via adb, then immediately closes it
            // (the device is not listening anymore), producing a spurious
            // EOF that kills the relay before the client has had a chance
            // to start. Errors are ignored: --remove fails when there is
            // nothing to remove, which is the common case.
            
            // Suppress adb's stderr for this best-effort cleanup: it prints
            // "listener 'tcp:N' not found" when there is nothing to remove,
            // which is the common case and would just be log noise.
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

pub fn cmd_relay(
    port: u16,
    transport: crate::transport::TransportMode,
    serial: Option<String>,
) -> Result<(), CommandExecutionError> {
    use crate::transport::TransportMode;
    match transport {
        TransportMode::Reverse => {
            info!(target: TAG, "Starting relay server in reverse mode on port {}...", port);
            relaylib::relay(port)?;
        }
        TransportMode::Forward => {
            info!(target: TAG, "Starting relay in forward mode on port {}...", port);
            relaylib::relay::Relay::new(port).run_forward(serial)?;
        }
    }
    Ok(())
}

/// Return the ADB serial of the device this `cmd_start` call is targeting.
///
/// If `serial` is `None`, try to auto-detect a single connected device.
/// Return `None` if there are zero or multiple devices, in which case the
/// serial correlation is skipped (the relay falls back to `Client #N`
/// without a serial in the logs).
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
        .skip(1) // "List of devices attached"
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
) -> Result<(), CommandExecutionError> {
    ensure_adb();
    if must_install_client(serial)? {
        cmd_install(serial)?;
        thread::sleep(Duration::from_millis(500));
    }

    info!(target: TAG, "Starting client...");

    // `am start` on API 19 does not support `--esa` (String array extras);
    // it was added in API 21. On KitKat we have to skip array extras and
    // let the client fall back to its defaults (DNS 8.8.8.8, route 0.0.0.0/0,
    // no per-app allow/deny). Single-value extras (--es, --ei) work everywhere
    // and are always passed.
    let sdk = crate::transport::query_device_sdk(serial).unwrap_or(0);
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

    // Best-effort correlation: remember which serial is about to open a
    // connection to the relay. See relay::serial_registry for details.
    if let Some(s) = effective_serial(serial) {
        serial_registry::register_pending(&s);
        debug!(target: TAG, "Registered serial {} for the next relay connection", s);
    }

    cmd_tunnel(serial, port, transport)?;

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

pub fn cmd_autostart(
    dns_servers: Option<&str>,
    routes: Option<&str>,
    port: u16,
    mtu: u16,
    allow_wifi: bool,
    socks5: Option<&str>,
) -> Result<(), CommandExecutionError> {
    let start_dns_servers = dns_servers.map(String::from);
    let start_routes = routes.map(String::from);
    let start_socks5 = socks5.map(String::from);
    let mut adb_monitor = AdbMonitor::new(Box::new(move |serial: &str| {
        let dns_servers = start_dns_servers.as_ref().map(String::as_ref);
        let routes = start_routes.as_ref().map(String::as_ref);
        let socks5 = start_socks5.as_ref().map(String::as_ref);
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
) {
    let start_serial = serial.map(String::from);
    let start_dns_servers = dns_servers.map(String::from);
    let start_routes = routes.map(String::from);
    let start_proxy = proxy.map(String::from);
    let start_exclusions = proxy_exclusions.map(String::from);
    let start_socks5 = socks5.map(String::from);
    let allow_apps_owned = allow_apps.to_vec();
    let deny_apps_owned = deny_apps.to_vec();
    thread::spawn(move || {
        let serial = start_serial.as_ref().map(String::as_ref);
        let dns_servers = start_dns_servers.as_ref().map(String::as_ref);
        let routes = start_routes.as_ref().map(String::as_ref);
        let proxy = start_proxy.as_ref().map(String::as_ref);
        let exclusions = start_exclusions.as_ref().map(String::as_ref);
        let socks5 = start_socks5.as_ref().map(String::as_ref);
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
        ) {
            crate::execution_error::print_error(&err);
        }
    });
}

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
) -> Result<(), CommandExecutionError> {
    if let Some(proxy) = socks5
        && let Ok(addr) = proxy.parse::<std::net::SocketAddr>()
    {
        let _ = tcp_connection::SOCKS5_PROXY.set(addr);
    }

    let transport = transport_pref.resolve(serial);
    info!(target: TAG, "Using transport mode: {:?}", transport);

    async_start(
        serial, dns_servers, routes, port, proxy, proxy_exclusions,
        mtu, allow_apps, deny_apps, socks5,
        transport,
    );

    let ctrlc_serial = serial.map(String::from);
    let relay_serial = serial.map(String::from);
    let rt = tokio::runtime::Runtime::new().map_err(|e| CommandExecutionError::Io(std::io::Error::other(e)))?;

    rt.block_on(async {
        tokio::select! {
            result = tokio::task::spawn_blocking(move ||
                    cmd_relay(port, transport, relay_serial)) => {
                        result.map_err(|e| {
                        CommandExecutionError::Io(std::io::Error::other(e))
            })?
            }
            _ = tokio::signal::ctrl_c() => {
                info!(target: TAG, "Interrupted");
                if let Err(err) = cmd_stop(ctrlc_serial.as_deref()) {
                    error!(target: TAG, "Cannot stop client: {}", err);
                }
                std::process::exit(0);
            }
        }
    })
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
    {
        let autostart_dns_servers = dns_servers.map(String::from);
        let autostart_routes = routes.map(String::from);
        let autostart_socks5 = socks5.map(String::from);
        thread::spawn(move || {
            let dns_servers = autostart_dns_servers.as_ref().map(String::as_ref);
            let routes = autostart_routes.as_ref().map(String::as_ref);
            let socks5 = autostart_socks5.as_ref().map(String::as_ref);
            if let Err(err) = cmd_autostart(dns_servers, routes, port, mtu, allow_wifi, socks5) {
                error!(target: TAG, "Cannot auto start clients: {}", err);
            }
        });
    }

    cmd_relay(port, transport, None)
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
    )?;
    Ok(())
}
