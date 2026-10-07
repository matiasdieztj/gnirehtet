# gnirehtet-legacy

The `legacy` product flavor of the gnirehtet Android client, targeting
**Android 4.4 (KitKat, API 19)** through Android 5.0 (API 20).

## Why this exists

The upstream gnirehtet README requires API 21 (Android 5.0). There are two
hard reasons for that requirement:

1. `VpnService.Builder.setBlocking()` was added in API 21.
2. `adb reverse` was added to AOSP's `adbd` in Android 5.0.

The legacy flavor works around both:

- `setBlocking()` is replaced by a JNI call to
  `fcntl(fd, F_SETFL, flags & ~O_NONBLOCK)`, giving blocking reads on the
  VPN fd without API 21.
- `adb reverse` is replaced by a "forward transport": the client listens
  on `127.0.0.1:31416` with a plain `ServerSocket`, the host runs
  `adb forward tcp:31416 tcp:31416`, and the relay connects to the client.
  Both directions travel over USB.

The rest of the API 21+ conveniences (per-app routing, metered reporting,
underlying networks, notification channels) are gated behind `Build.VERSION`
checks in `VersionCompat.java` and `GnirehtetService`.

## Requirements

- **A Wi-Fi network must be connected.** This is not a gnirehtet bug: see
  the "Why a dummy Wi-Fi is required" section below. The Wi-Fi does not
  need to have internet access — it only needs to occupy the "active
  network" slot in the framework so that apps calling
  `getActiveNetworkInfo()` see a non-null network and attempt to open
  sockets. All actual traffic is routed through the VPN.

  A practical setup: run a hotspot on the host PC (or a phone) without
  sharing its internet, and connect the Android device to it. The device
  will show "Connected, no Internet" — ignore that warning and keep the
  tunnel up.

## What works

Verified on an Amazon Fire HD 8 (3rd gen), Fire OS 4 / Android 4.4.3,
serial `00D3060740350AG8`:

- Reverse tethering over `adb forward` (forward transport).
- DNS through the tunnel (UDP/53).
- TCP and TLS through the tunnel (HTTPS).
- Brave, Fennec F-Droid, F-Droid, APKPure, SmartTube — all load pages
  and download content through the tunnel.
- Notification and foreground service.

## What does not work (or works differently)

| Feature | Modern (API 21+) | Legacy (API 19) |
|---------|------------------|-----------------|
| Blocking reads | `setBlocking(true)` | JNI `fcntl` |
| Transport | `adb reverse` + `LocalSocket` | `adb forward` + `ServerSocket` |
| Per-app exclusion | `addDisallowedApplication()` | **Not available** |
| Metered reporting | `setMetered(false)` | **Not available** (no concept pre-29) |
| Underlying network | `setUnderlyingNetworks(null)` | **Not available** (no concept pre-22) |
| Notification channels | `NotificationChannel` | Legacy builder (no channels) |
| `getActiveNetwork()` | Available | Fallback to `getActiveNetworkInfo()` |
| System default network | Promoted automatically | **Requires a dummy Wi-Fi** (see below) |

The "not available" items are **no-ops on legacy because the underlying
concept does not exist**. They are not regressions.

## Why a dummy Wi-Fi is required

Android 4.4's `ConnectivityService` does not recognise a VPN-only
transport as the system default network. Apps that gate on
`getActiveNetworkInfo()` see `null` and refuse to open sockets.

This is the same "Class A" behaviour documented for Android 12 in
`DEVELOP.md`. The workaround is identical: give the framework a real
network to report as active (a Wi-Fi or a hotspot), and let the VPN's
policy-routing rules redirect the actual traffic.

On Android 4.4 the routing rules are:

```

ip rule:
100:    from all fwmark 0x3c lookup 60
ip route show table 60:
default dev tun0  scope link

```

Apps running as user UIDs receive `fwmark 0x3c` and are routed through
`tun0`. The dummy Wi-Fi only affects the `getActiveNetworkInfo()` gate; it
does not carry any traffic, because the VPN's route table takes precedence.

**`adb shell ping` will always fail.** The shell runs as uid 2000 and
does not receive the `fwmark`, so its traffic falls through to the empty
main routing table. Use an app (a browser, F-Droid) to validate
connectivity.

## Building

```bash
# Both flavors
./gradlew assembleRelease

# Just legacy
./gradlew assembleLegacyRelease
```

Output:
`app/build/outputs/apk/legacy/release/app-legacy-release.apk`

## Installing

```
adb install -r app/build/outputs/apk/legacy/release/app-legacy-release.apk
```

The legacy and modern flavors share the same `applicationId`, so installing
one replaces the other. To test both side by side, change
`applicationIdSuffix` for one flavor in `build.gradle`.

## Running

On the host:

```
cd relay-rust
cargo build --release
RUST_LOG=info ./target/release/gnirehtet run \
  --transport=forward \
  <SERIAL> \
  -d 8.8.8.8 \
  --mtu 1500
```

The `--transport` flag defaults to `auto`: the relay queries the device's
`ro.build.version.sdk` and selects `forward` for SDK < 21, `reverse` for
SDK >= 21. Explicit values `reverse` and `forward` override the detection.

## Testing

Minimum verification on an API 19 device:

```
# With the APK installed and the relay running:
adb shell ip addr show tun0             # tun0 with 10.0.0.2/32
adb shell ip rule                       # fwmark 0x3c rule present
adb shell ip route show table all       # default dev tun0  table 60
adb logcat | grep -E "GnirehtetJNI|RelayTunnel"
```

Then, on the device, open a browser and load `http://example.com`.
If it loads, the tunnel is up. If it does not, check
`/tmp/relay.log` on the host for `TcpConnection ... Open` lines.

## Requirements

- JDK 17 (required by AGP 8.x). Do **not** use JDK 21 or 25 for the build;
see the project's `gradle.properties` for the reasoning.
- Android SDK with platform 36 and build-tools 36.x.
- Android NDK (any recent version; CMake 3.22.1 is the minimum).
- `adb` 1.0.36 or newer for `adb forward`.
- A Wi-Fi network on the device (see above).

## Reporting bugs

When filing a bug against the legacy flavor, include:

1. Android version and API level (`adb shell getprop ro.build.version.sdk`).
2. The output of `adb logcat -d | grep -E "Gnirehtet|GnirehtetJNI|RelayTunnel"`.
3. The output of `ip rule` and `ip route show table all` on the device.
4. Whether the same issue reproduces with the modern flavor.

## Status

**Working.** Verified end-to-end on an Amazon Fire HD 8 (3rd gen) running
Fire OS 4 / Android 4.4.3. The forward transport is used automatically
when the relay detects SDK < 21.

The API 14–18 range (Android 4.0–4.3) is **not tested**. The code should
work — `VpnService` exists from API 14 and `adb forward` has existed
forever — but the author has no devices to verify on. If you test and it
works, open a PR updating this section.