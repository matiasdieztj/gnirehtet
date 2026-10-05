# Gnirehtet — developer guide

## Requirements

- **Rust** 1.85
```

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
./gradlew :app:ass
```

### Everything

```
make          # Linux/macOS
build.bat     # Windows
```

### Cross-compilation

```
make build-linux-x
```

## Run

```
cargo run --
```

Or after building:

```
./relay-rust/target/release/gnirehtet run
```

## Test

```
c
```

## Design

### Data flow

```
Android apps → VpnService → IP packets → adb reverse tunnel → Relay server
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
| −jiff over chrono | jiff has no CVE history, lighter, actively maintained |
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
| −Dummy hotspot | WiFi `Network` | ✅ Works — socket traverses the tunnel |
| −Real WiFi | WiFi `Network` | ✅ Works |
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
| −Real WiFi | WiFi `Network` | ✅ Works, but measures WiFi, not the tunnel |
⚙

### Symptom summary

| Symptom ↕▾ | Class ↕▾ | Environment ↕▾ | Explanation ↕▾ |
|---|---|---|---|
| −Play Store download stuck at "Pendiente" | A | Airplane | `getActiveNetwork() == null` |
| −Galaxy Store download stuck at "Pendiente" | A | Airplane | Same |
| −Ookla hangs at splash | B | Airplane | Same, then `RetrieveServerListTask` throws |
| −Ookla fails with dummy hotspot | B | Dummy hotspot | Sockets bound to hotspot, no upstream |
| −Ookla works with real WiFi, 88 Mbps | B | Real WiFi | Sockets bound to WiFi, tunnel bypassed |
| −WiFiman upload fails at ~5 s | B | Any | Same pattern |
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
RUST_LOG=debug ./
```

| Observation ↕▾ | Likely cause ↕▾ |
|---|---|
| −0 TCP opens during failure | Class A or B — no default network to gate on |
| −TCP opens, none to the app's servers | Class A — UI connecting, feature still blocked |
| −TCP opens to the app's servers, then stalls | Relay bug — investigate flow control |
| −Many `Rejecting QUIC` | App uses QUIC heavily; may benefit from isolating fix #11 |
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
adb -s <serial>
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
iperf3
```

**Baseline** (v2.6.3, Samsung Galaxy A21s, host on Debian 13):

| Direction ↕▾ | Throughput ↕▾ | Retransmissions ↕▾ | Duration ↕▾ |
|---|---|---|---|
| −Upload (device → host) | ~257 Mbps | 4 | 10 s stable |
| −Download (host → device) | ~252 Mbps | 5 | 10 s stable |
⚙

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

