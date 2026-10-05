# Gnirehtet

Reverse tethering for Android — share your computer's internet with an Android
device over USB. No root required.

Works on Linux, Windows and macOS. Relays [TCP] and [UDP] over [IPv4] and
[IPv6].

[TCP]: https://en.wikipedia.org/wiki/Transmission_Control_Protocol
[UDP]: https://en.wikipedia.org/wiki/User_Datagram_Protocol
[IPv4]: https://en.wikipedia.org/wiki/IPv4
[IPv6]: https://en.wikipedia.org/wiki/IPv6

## Quick start

    ./gnirehtet run

The first start opens a prompt on the device to accept the VPN connection.
Press _Ctrl+C_ to stop.

## Requirements

- An Android device with USB debugging enabled
- The `gnirehtet.apk` installed on the device (included in releases)
- That's it — `adb` is auto-downloaded if missing

## Commands

| Command | Description |
|---------|-------------|
| `run` | Connect one device (install + start + relay, stop on Ctrl+C) |
| `start` | Start a client on the device and exit |
| `stop` | Stop the client |
| `install` | Install the APK |
| `autorun` | Auto-connect all present and future devices |
| `relay` | Start only the relay server |
| `tunnel` | Set up the `adb reverse` tunnel |

Run `gnirehtet --help` for all options and flags.

## Environment

- `ADB` — path to `adb` executable
- `GNIREHTET_APK` — path to `gnirehtet.apk`
- `RUST_LOG` — set to `debug` or `trace` for verbose logging

## Known limitations

### Speed test apps do not measure the tunnel

Ookla Speedtest, fast.com and WiFiman all call
`ConnectivityManager.bindProcessToNetwork()`. This hard-binds every socket
the app opens to the physical network — their traffic **never enters the
gnirehtet tunnel**, regardless of how the VPN is configured. Any throughput
number these apps report reflects the WiFi or cellular connection, not the
USB tunnel.

**To measure gnirehtet's real throughput, use `iperf3` from Termux:**

```bash
# On the host PC:
iperf3 -s -p 5201

# On the device, via Termux:
adb reverse tcp:5201 tcp:5201
iperf3 -c localhost -p 5201 -t 10          # upload   (device → host)
iperf3 -c localhost -p 5201 -t 10 -R       # download (host → device)
```

Reference numbers on a Samsung Galaxy A21s (Android 12), host on Debian 13:
~257 Mbps upload / ~252 Mbps download, 4–5 retransmissions over 10 s.

### Store downloads require a system default network

In strict airplane mode (WiFi off, cellular off, USB only), Android's
`NetworkRanker` refuses to promote a VPN-only transport to the system
default network. Confirmed on-device:

```
Active default network: none
```

The Play Store and Galaxy Store download managers gate on
`getActiveNetwork()`, which returns `null` in this state. The download item
sits at **"Pendiente"** indefinitely and never opens a socket.

**Workaround:** run a dummy hotspot on the host and connect the device to
it. The hotspot does not need internet — it only needs to occupy the
default network slot.

```
nmcli dev wifi hotspot ifname wlan0 ssid gnirehtet-dummy password gnirehtet123
```

Then, on the device: connect to `gnirehtet-dummy`, ignore the "Sin acceso a
Internet" toast ("Mantener conexión"), and keep the gnirehtet tunnel
active. Store downloads will work.

Note that this workaround is mutually exclusive with speed test apps — see
the table below.

### Environment matrix

| Use case ↕▾ | Airplane mode ↕▾ | Dummy hotspot ↕▾ | Real WiFi ↕▾ |
|---|---|---|---|
| −Browsing (any app) | ✅ | ✅ | ✅ |
| −`curl` / `ping` from Termux | ✅ | ✅ | ✅ |
| `netcheck` suite | ✅ | ✅ | ✅ |
| iperf3 over tunnel | ✅ 257 Mbps | ✅ 257 Mbps | ✅ 257 Mbps |
| Play Store / Galaxy Store downloads | ❌ | ✅ | ✅ |
| Ookla / fast.com / WiFiman | ❌ | ❌ | ✅ (measures WiFi) |
⚙

There is no configuration where speed test apps measure the tunnel. This
is a platform constraint, not a gnirehtet bug. See `DEVELOP.md` for the
full technical explanation.

## Build from source

Build the relay (works everywhere, no extra tools):

scripts/build.sh

Build the APK (requires JDK and Android SDK):

scripts/build-apk.sh

Or use `make` (Linux/macOS) or `build.bat` (Windows) for the same commands.

Pre-built binaries for all platforms are attached to each
[release](https://github.com/Genymobile/gnirehtet/releases).

## Licence

Copyright (C) 2017 Genymobile

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.

</BDS:create_file>

<BDS:create_file fileName="DEVELOP.md">

```markdown
# Gnirehtet — developer guide

## Requirements

- **Rust** 1.85+ (install via [rustup](https://rustup.rs/))
- **Android SDK** (for APK builds) — JDK 17+ with `javac`, plus platform 35 and
  build-tools 36. Install through your package manager or Android Studio.
  The release script auto-detects the SDK at common locations.
- **adb** (1.0.36+) — or let gnirehtet auto-download it

## Project structure

gnirehtet/
├── app/                          # Android VPN client (Java)
│   └── src/main/java/.../
├── relay-rust/                   # Rust relay server
│   └── src/
│       ├── main.rs               # Entry point (thin: init logger, dispatch CLI)
│       ├── cli.rs                # clap-based CLI definitions
│       ├── commands.rs           # Command implementations (run, start, stop, etc.)
│       ├── adb.rs                # ADB helper functions
│       ├── adb_monitor.rs        # ADB device tracking
│       ├── logger.rs             # Logging (RUST_LOG env, --log-file support)
│       ├── execution_error.rs    # Error types
│       └── relay/                # Core relay engine
│           ├── relay.rs          # TCP listener + event loop
│           ├── client.rs         # Per-device client handler
│           ├── router.rs         # Packet routing (HashMap<ConnectionId, Connection>)
│           ├── tcp_connection.rs # TCP state machine + async I/O
│           ├── udp_connection.rs # UDP virtual connections
│           ├── ip_packet.rs      # IpPacket enum (V4|V6)
│           ├── ipv4_header.rs    # IPv4 header parsing
│           ├── ipv6_header.rs    # IPv6 header parsing
│           ├── tcp_header.rs     # TCP header parsing
│           ├── udp_header.rs     # UDP header parsing
│           ├── transport_header.rs
│           ├── packetizer.rs     # L5→L3 packet construction
│           ├── stream_buffer.rs  # Circular byte buffer
│           ├── datagram_[buffer.rs#](https://buffer.rs/#) Datagram buffer
│           ├── selector.rs       # No-op shim (replaced mio)
│           ├── net.rs            # Socket address helpers
│           └── ...
├── Makefile                      # Build/run/test targets (Linux/macOS)
├── build.bat                     # Build/run/test targets (Windows)
└── release                       # Release packaging script

```

## Build

### Rust relay only

```bash
cd relay-rust
cargo build --release
```

### Android APK

```
./gradlew :app:assembleDebug
```

### Everything

```
make          # Linux/macOS
build.bat     # Windows
```

### Cross-compilation

```
make build-linux-x86_64
make build-linux-aarch64
make build-macos-x86_64
make build-macos-arm64
make build-windows-x86_64
```

## Run

```
cargo run --manifest-path relay-rust/Cargo.toml -- run
```

Or after building:

```
./relay-rust/target/release/gnirehtet run
```

## Test

```
cargo test --manifest-path relay-rust/Cargo.toml
```

## Design

### Data flow

```
Android apps → VpnService → IP packets → adb reverse tunnel → Relay server
                                                                   ↓
                                                            Real OS sockets
                                                                   ↓
                                                            Remote servers
```

The Android device creates a VPN interface that captures all IPv4/IPv6 traffic.
Raw IP packets are forwarded over the ADB reverse tunnel to the relay server on
the host. The relay parses the IP/TCP/UDP headers, creates real OS sockets to
the destination, and relays data bidirectionally.

### Key properties

- **Rust-only** — the Java relay has been removed
- **IPv4 + IPv6** — first-class dual-stack support
- **Synchronous I/O with tokio runtime** — single-threaded event loop
- **No root required** on either device or host
- **No Rc<RefCell<>>** — connections stored in `HashMap<ConnectionId, Box<dyn Connection>>`
- **Custom MTU, per-app routing, HTTP proxy, DNS**, and more via CLI flags

### Architecture decisions

| Decision ↕▾ | Rationale ↕▾ |
|---|---|
| −Single-threaded event loop | Sufficient for typical use (1-5 devices); avoids sync overhead |
| −HashMap for routing | O(1) lookup vs Vec's O(n); comment saying "HashMap less efficient" was incorrect |
| −Synthetic TCP state machine | Only implements enough TCP states to fool the device's TCP stack, not a full RFC 793 |
| −No Rc<RefCell> | Prevents runtime borrow panics; connections are Box<dyn Connection> |
| jiff over chrono | jiff has no CVE history, lighter, actively maintained |
⚙

## Release

Tag a commit and push:

```
git tag v2.6.0
git push --tags
```

GitHub Actions builds and attaches binaries for all 5 targets.

## Android default-network limitation

This section documents a platform-level constraint that affects gnirehtet
in strict airplane mode. It is **not** a bug in the relay, the Java client,
or any of the fixes applied in v2.6.x. It is a documented behaviour of
AOSP's `NetworkRanker`.

### The rule

AOSP refuses to promote a VPN-only transport to the system default network
slot. On the test device (Samsung Galaxy A21s, Android 12), `dumpsys
connectivity` reports:

```
Active default network: none
```

And every framework `NetworkRequest` carries an explicit `&NOT_VPN`
capability filter:

```
[ Capabilities: INTERNET&NOT_RESTRICTED&TRUSTED&NOT_VPN&NOT_VCN_MANAGED ...]
```

No network can satisfy that request when only the VPN is up. This is the
root cause of every symptom described below.

### App taxonomy: Class A vs Class B

Apps that need connectivity fall into two behavioural classes. The
distinction determines whether they traverse the tunnel.

#### Class A — "check-and-delegate"

Calls `ConnectivityManager.getActiveNetwork()` (or waits for `onAvailable`
on a `NetworkRequest`), then **opens sockets normally**. The OS routes
those sockets through `tun0` because gnirehtet's `addRoute()` has installed
the required routes.

**Examples:** Play Store (`GmsDownloadService`), Galaxy Store download
manager, Android `DownloadManager`, `WorkManager` with
`NetworkType.CONNECTED`, most banking apps, most browser downloads, most
chat apps.

**Behaviour:**

| Environment ↕▾ | `getActiveNetwork()` ↕▾ | Result ↕▾ |
|---|---|---|
| −Strict airplane | `null` | Hangs — never opens a socket |
| Dummy hotspot | WiFi `Network` | ✅ Works — socket traverses the tunnel |
| Real WiFi | WiFi `Network` | ✅ Works |
⚙

#### Class B — "bind-to-default"

Calls `ConnectivityManager.bindProcessToNetwork(getActiveNetwork())` (or
the per-socket equivalent `Network.bindSocket()`). This hard-binds every
socket the app opens to a specific `Network` object, **bypassing the VPN
route table entirely**.

**Examples:** Ookla Speedtest, fast.com, WiFiman, some CDN-aware SDKs
(Akamai, Cloudflare).

**Behaviour:**

| Environment ↕▾ | `getActiveNetwork()` ↕▾ | Result ↕▾ |
|---|---|---|
| −Strict airplane | `null` | Hangs at splash |
| −Dummy hotspot | WiFi `Network` | ❌ Fails — hotspot has no upstream |
| Real WiFi | WiFi `Network` | ✅ Works, but measures WiFi, not the tunnel |
⚙

### Symptom summary

| Symptom ↕▾ | Class ↕▾ | Environment ↕▾ | Explanation ↕▾ |
|---|---|---|---|
| −Play Store download stuck at "Pendiente" | A | Airplane | `getActiveNetwork() == null` |
| −Galaxy Store download stuck at "Pendiente" | A | Airplane | Same |
| −Ookla hangs at splash | B | Airplane | Same, then `RetrieveServerListTask` throws |
| Ookla fails with dummy hotspot | B | Dummy hotspot | Sockets bound to hotspot, no upstream |
| Ookla works with real WiFi, 88 Mbps | B | Real WiFi | Sockets bound to WiFi, tunnel bypassed |
| WiFiman upload fails at ~5 s | B | Any | Same pattern |
⚙

### Why this cannot be fixed

- `setUnderlyingNetworks(null)` and `setUnderlyingNetworks(new Network[0])`
are functionally identical on Android 12.
- Forcing promotion of a VPN-only transport to default would require
reflection over private `ConnectivityManager` APIs, which are hardened
and unstable across Android versions.
- Even if promotion worked, Class B apps would still
`bindProcessToNetwork()` and sidestep the tunnel. There is no public API
that lets a `VpnService` intercept sockets that another app has
explicitly bound to a different `Network`.

### Workaround

Run a dummy hotspot on the host:

```
nmcli dev wifi hotspot ifname wlan0 ssid gnirehtet-dummy password gnirehtet123
```

Connect the device to it, ignore the "Sin acceso a Internet" toast, keep
the tunnel active. This satisfies Class A apps (stores, downloaders) while
leaving Class B apps (speed tests) broken.

### Diagnostic recipe

When a user reports "app X fails", classify it in four steps.

#### 1. Relay-side check

```
RUST_LOG=debug ./target/release/gnirehtet run -d 8.8.8.8 --mtu 1500 2>&1 | tee /tmp/relay.log
# (reproduce the failure)

grep -c 'TcpConnection.*Open' /tmp/relay.log
grep -c 'UdpConnection.*Open' /tmp/relay.log
grep -c 'Rejecting QUIC' /tmp/relay.log
grep -c 'Unexpected first packet' /tmp/relay.log
```

| Observation ↕▾ | Likely cause ↕▾ |
|---|---|
| −0 TCP opens during failure | Class A or B — no default network to gate on |
| −TCP opens, none to the app's servers | Class A — UI connecting, feature still blocked |
| TCP opens to the app's servers, then stalls | Relay bug — investigate flow control |
| Many `Rejecting QUIC` | App uses QUIC heavily; may benefit from isolating fix #11 |
⚙

For Play Store specifically, look for CDN hosts:

```
grep -iE 'gvt1|dl\.google|android\.clients|play\.googleapis' /tmp/relay.log | head -20
```

If only `play.googleapis.com` and `android.clients.google.com:5228` appear
— no `*.gvt1.com`, no `dl.google.com` — the downloader never fired. Class A.

#### 2. Framework-side check

```
adb -s <serial> shell dumpsys connectivity \
  | grep -iE "Active default network|SystemDefault|UnderlyingNetworks" | head -10
```

If `Active default network: none`, both classes will fail.

#### 3. App-side check

```
adb -s <serial> logcat -c
# (reproduce failure)
adb -s <serial> logcat -d -v time \
  | grep -iE 'bindProcessToNetwork|bindSocket|requestNetwork' | head -40
```

If the app's UID appears calling `bindProcessToNetwork`, it is Class B and
will never use the tunnel.

#### 4. Final classification

| Behaviour ↕▾ | Class ↕▾ |
|---|---|
| −Works with dummy hotspot, no change in relay traffic | Class A |
| −Fails with dummy hotspot, works with real WiFi | Class B |
| −Neither helps | Real relay bug — open an issue |
⚙

### Throughput validation: use iperf3, not speed test apps

Because Class B apps bypass the tunnel, they cannot be used to validate
gnirehtet's throughput. The only test that traverses the tunnel is iperf3
from Termux (Termux does not call `bindProcessToNetwork()`):

```
# Host:
iperf3 -s -p 5201

# Device:
adb -s <serial> reverse tcp:5201 tcp:5201
iperf3 -c localhost -p 5201 -t 10          # upload
iperf3 -c localhost -p 5201 -t 10 -R       # download
```

**Baseline** (v2.6.3, Samsung Galaxy A21s, host on Debian 13):

| Direction | Throughput | Retransmissions | Duration |
|---|---|---|---|
| Upload (device → host) | ~257 Mbps | 4 | 10 s stable |
| Download (host → device) | ~252 Mbps | 5 | 10 s stable |

If a future change produces materially lower iperf3 numbers, that is a real
relay regression. If only speed test apps regress, that is a Class B issue
and is not a bug.

### References

- `ConnectivityManager.bindProcessToNetwork()`:
https://developer.android.com/reference/android/net/ConnectivityManager#bindProcessToNetwork(android.net.Network)
- `Network.bindSocket()`:
https://developer.android.com/reference/android/net/Network#bindSocket(java.net.Socket)
- AOSP `NetworkRanker`:
https://cs.android.com/android/platform/superproject/+/main:packages/modules/Connectivity/service/src/com/android/server/connectivity/NetworkRanker.java
- NetworkManager `nmcli`:
https://networkmanager.dev/docs/api/latest/nmcli.html

