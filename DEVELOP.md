# Gnirehtet — developer guide

## Requirements

- **Rust** 1.85+ (install via [rustup](https://rustup.rs/))
- **Android SDK** (for APK builds) — JDK 17+ with `javac`, plus platform 35 and
  build-tools 36. Install through your package manager or Android Studio.
  The release script auto-detects the SDK at common locations.
- **adb** (1.0.36+) — or let gnirehtet auto-download it

## Project structure

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
│           ├── datagram_buffer.rs# Datagram buffer
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

```bash
./gradlew :app:assembleDebug
```

### Everything

```bash
make          # Linux/macOS
build.bat     # Windows
```

### Cross-compilation

```bash
make build-linux-x86_64
make build-linux-aarch64
make build-macos-x86_64
make build-macos-arm64
make build-windows-x86_64
```

## Run

```bash
cargo run --manifest-path relay-rust/Cargo.toml -- run
```

Or after building:

```bash
./relay-rust/target/release/gnirehtet run
```

## Test

```bash
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

| Decision | Rationale |
|----------|-----------|
| Single-threaded event loop | Sufficient for typical use (1-5 devices); avoids sync overhead |
| HashMap for routing | O(1) lookup vs Vec's O(n); comment saying "HashMap less efficient" was incorrect |
| Synthetic TCP state machine | Only implements enough TCP states to fool the device's TCP stack, not a full RFC 793 |
| No Rc<RefCell> | Prevents runtime borrow panics; connections are Box<dyn Connection> |
| jiff over chrono | jiff has no CVE history, lighter, actively maintained |

## Release

Tag a commit and push:

```bash
git tag v2.6.0
git push --tags
```

GitHub Actions builds and attaches binaries for all 5 targets.

## Known limitation: Android VpnService and the default network (Android 12+)

### Summary

In strict airplane mode (WiFi OFF, cellular OFF, USB only), gnirehtet routes
IP traffic correctly but apps that use
`ConnectivityManager.registerDefaultNetworkCallback()` hang at startup.
This is an **AOSP `NetworkRanker` limitation**, not a gnirehtet bug.

### Evidence

**IP traffic works in airplane mode.** Running `ping -c 3 1.1.1.1` through
gnirehtet (USB only, no WiFi/cellular):

    64 bytes from 1.1.1.1: icmp_seq=1 ttl=64 time=1.79 ms
    64 bytes from 1.1.1.1: icmp_seq=2 ttl=64 time=1.83 ms
    64 bytes from 1.1.1.1: icmp_seq=3 ttl=64 time=1.49 ms

`TTL=64` with `<2 ms` latency is the synthetic ICMP echo reply from
`icmp_handler.rs` — the packet never left the host. The VPN routes traffic.

**But the default network is absent.** Comparing `dumpsys connectivity`:

| Field                        | WiFi ON              | Airplane mode        |
|------------------------------|----------------------|----------------------|
| VPN Transports               | `WIFI\|VPN`          | `VPN` only           |
| VPN UnderlyingNetworks       | `[174]`              | `[]`                 |
| Active default network       | `174` (WiFi)         | `none`               |
| Capabilities                 | `INTERNET&VALIDATED` | `INTERNET&VALIDATED` |

Capabilities are identical. The difference is the transport: AOSP's
`NetworkRanker` requires a non-VPN transport for default-network eligibility.
A VPN-only transport is **never** promoted to default, regardless of
capabilities.

### Affected apps

Any app that gates its network operations on `registerDefaultNetworkCallback`
or `getActiveNetwork()`:

- **fast.com** — stuck at logo, never progresses
- **Ookla Speedtest** — works only when WiFi/cellular is also present
- **WiFiman** — download works; upload may timeout

Apps that use raw sockets or `VpnService` file descriptors directly are
unaffected.

### Why it cannot be fixed

- `setUnderlyingNetworks(null)` and `setUnderlyingNetworks(new Network[0])`
  are functionally identical on Android 12. Neither promotes a VPN-only
  transport.
- The only workaround would be reflection over private `ConnectivityManager`
  APIs, which break every Android release and are hardened against exactly
  this use case.

### Workaround

Connect the device to **any** WiFi network (with or without internet).
The WiFi provides the default-network slot; gnirehtet's routes still
intercept all app traffic via the USB tunnel.

For environments without WiFi, create a hotspot on the host PC and connect
the device to it. The hotspot does not need internet access — it only needs
to provide the `Active default network` slot.

### References

- `VpnService.setUnderlyingNetworks()`:
  https://developer.android.com/reference/android.net/VpnService#setUnderlyingNetworks(android.net.Network[])
- AOSP `NetworkRanker`:
  https://cs.android.com/android/platform/superproject/+/main:packages/modules/Connectivity/service/src/com/android/server/connectivity/NetworkRanker.java
