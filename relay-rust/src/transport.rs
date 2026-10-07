//! Transport mode selection between the relay and the Android client.
//!
//! On API 21+ (Android 5.0+), gnirehtet uses `adb reverse` to expose an
//! abstract local socket on the device. The client connects to it with
//! `LocalSocket`, and the relay accepts the connection on the host.
//!
//! `adb reverse` does not exist on API 19 (Android 4.4 KitKat). For those
//! devices we invert the direction: the client listens on 127.0.0.1:<port>,
//! the host uses `adb forward tcp:<port> tcp:<port>`, and the relay connects
//! to the client. This is called "forward transport".

use std::process;
use std::str::FromStr;

/// Concrete transport direction, after resolving `TransportPreference`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportMode {
    /// Relay listens on 0.0.0.0:<port>, client connects via `adb reverse`.
    Reverse,
    /// Client listens on 127.0.0.1:<port>, relay connects via `adb forward`.
    Forward,
}

/// User-facing transport preference (`--transport`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportPreference {
    /// Detect from the device's SDK level.
    Auto,
    /// Always use `adb reverse` (API 21+).
    Reverse,
    /// Always use `adb forward` (API 14+).
    Forward,
}

impl TransportPreference {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(Self::Auto),
            "reverse" => Ok(Self::Reverse),
            "forward" => Ok(Self::Forward),
            other => Err(format!(
                "invalid transport '{}' (expected auto, reverse or forward)",
                other
            )),
        }
    }

    /// Resolve to a concrete transport, querying the device when needed.
    pub fn resolve(self, serial: Option<&str>) -> TransportMode {
        match self {
            Self::Reverse => TransportMode::Reverse,
            Self::Forward => TransportMode::Forward,
            Self::Auto => match query_device_sdk(serial) {
                Some(sdk) if sdk < 21 => {
                    log::info!(
                        target: "Transport",
                        "Device SDK {} < 21, using forward transport",
                        sdk
                    );
                    TransportMode::Forward
                }
                Some(sdk) => {
                    log::debug!(
                        target: "Transport",
                        "Device SDK {} >= 21, using reverse transport",
                        sdk
                    );
                    TransportMode::Reverse
                }
                None => {
                    log::warn!(
                        target: "Transport",
                        "Cannot query device SDK, defaulting to reverse transport"
                    );
                    TransportMode::Reverse
                }
            },
        }
    }
}

impl FromStr for TransportPreference {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Query `ro.build.version.sdk` via adb. Returns `None` on any failure.
pub fn query_device_sdk(serial: Option<&str>) -> Option<u32> {
    let adb = crate::adb::get_adb_path();
    let mut cmd = process::Command::new(&adb);
    if let Some(s) = serial {
        cmd.args(["-s", s]);
    }
    cmd.args(["shell", "getprop", "ro.build.version.sdk"]);
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_auto() {
        assert_eq!(TransportPreference::parse("auto").unwrap(), TransportPreference::Auto);
    }

    #[test]
    fn parse_reverse() {
        assert_eq!(
            TransportPreference::parse("reverse").unwrap(),
            TransportPreference::Reverse
        );
    }

    #[test]
    fn parse_forward() {
        assert_eq!(
            TransportPreference::parse("forward").unwrap(),
            TransportPreference::Forward
        );
    }

    #[test]
    fn parse_invalid() {
        assert!(TransportPreference::parse("bogus").is_err());
    }
}