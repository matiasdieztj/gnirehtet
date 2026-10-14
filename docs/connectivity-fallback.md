# Connectivity fallback

## The problem

gnirehtet routes all traffic through a VPN (`tun0`). The kernel is always
ready to route packets, but many apps — and, critically, the Google
account stack — gate on the framework's notion of the "active network"
before opening sockets:

```java
ConnectivityManager cm = (ConnectivityManager) getSystemService(CONNECTIVITY_SERVICE);
NetworkInfo info = cm.getActiveNetworkInfo();
if (info == null || !info.isConnected()) {
    return; // never opens a socket
}
```

The framework does not promote a VPN-only transport to the system default
network on every Android version. When it does not, `getActiveNetworkInfo()`
returns `null` (or the VPN itself, which apps that filter `NOT_VPN` will
reject), even though the VPN is up and routing correctly. The behaviour
varies by version:

| API range ↕▾ | Android ↕▾ | VPN becomes default? ↕▾ | Fallback required? ↕▾ |
|---|---|---|---|
| −19–20 | KitKat | No | Yes |
| −21–30 | Lollipop – Android 11 | Usually yes | No |
| −31+ | Android 12+ | No (`NetworkRanker` filters `NOT_VPN`) | Yes |
⚙

Additionally, from Android 5.0 onwards, the Google account stack refuses
to run its `AccountManager.addAccount()` flow when the default network is
a VPN without a physical underlying network — even when the VPN is
marked `VALIDATED`. Chrome's own HTTPS login succeeds through the same
VPN, which confirms that the transport is healthy and isolates the
failure to GMS's network-type check. The fix is to exclude the Google
account packages (`com.google.android.gms`, `com.android.vending`,
`com.google.android.gsf`) from the VPN.

## The fix

`ConnectivityFallback` is started in a background thread from
`GnirehtetService.onStartCommand`, after the VPN is established. It
ensures an "active transport" exists, in this order:

1. **Preferred SSID** (when the relay passed `dummyWifiSsid`) — switch
to the relay's dummy Wi-Fi AP, even if another network is currently
active. The previous SSID is saved to `SharedPreferences` and
restored by `restorePreviousNetwork()` when the VPN stops.
2. **Wi-Fi Direct group** (`WifiP2pManager.createGroup()`) — creates a
`p2p0` interface without needing an AP or a peer. Requires
`FEATURE_WIFI_DIRECT`. Confirmed by either the
`WIFI_P2P_CONNECTION_CHANGED` broadcast or the `p2p0` interface
coming up with a non-loopback address.
3. **Local-only hotspot** (`WifiManager.startLocalOnlyHotspot()`,
API 26+) — creates a `wlan` interface without internet.
4. **Generic selection** — strongest saved secure network, then
strongest open network in range.

If every strategy fails, `Notifier.showDummyWifiRequired()` posts a
notification asking the user to connect Wi-Fi manually.

The fallback is invoked on every API level from `GnirehtetService`,
regardless of the transport mode (reverse or forward).

## Non-obvious facts

These cost time to discover and are load-bearing for the implementation:

- **`getActiveNetworkInfo()` does not see the WiFi when the VPN is
default.** `hasActiveTransport()` iterates `getAllNetworks()` and
accepts any network with `TRANSPORT_WIFI` or `TRANSPORT_CELLULAR`
and `NET_CAPABILITY_INTERNET`, skipping `TRANSPORT_VPN`. On API < 21
the legacy `getActiveNetworkInfo()` check is used, because the VPN
is not promoted to default on KitKat the same way.
- **`enableNetwork(netId, true)` alone is the correct switch.** Calling
`disconnect()` first, or `reconnect()` around it, races the state
machine on KitKat and can leave the framework idle with no pending
association. The `true` flag already disconnects and disables all
other networks atomically.
- **`addNetwork()` returns -1 for an already-saved SSID on KitKat.**
`findOrAddOpenNetwork()` first lists configured networks and reuses
the existing `networkId`. As an additional safeguard for configs
whose security parameters no longer match the AP (which would
produce a driver-level "Association Rejection reason code=1"), the
existing entry is removed and recreated with the correct (open)
key management.
- **The framework gives up after 1–2 association attempts.** It does
not keep retrying in the background. `waitForTransport()` re-issues
`enableNetwork` every 12 seconds during the association phase so
that a slow scan or a failed handshake does not consume the whole
budget in one attempt.
- **Scan throttling on Android 9 is aggressive.** The framework allows
roughly 4 scans per 2 minutes. The association timeout for the
preferred SSID is therefore 45 seconds, with the DHCP phase
(post-association) on a separate 15-second budget. Renewing the
deadline on the transition to `associated = true` is what makes the
two-phase timing work.
- **The relay must leave `captive_portal_mode` at its default (1).**
Setting it to `0` disables both the "Sign in to network" prompt and
the NetworkMonitor validation probe, which leaves the WiFi in a
`NOT_VALIDATED` state. Google account operations refuse to run on a
non-validated network and show a misleading "no connection" error.
- **Google account stack packages are excluded from the VPN.** The
`ACCOUNT_STACK_PACKAGES` array in `GnirehtetService` (GMS, Play
Store, GSF) is added to the VPN's disallow list whenever the user
did not pass `--allow-apps`. On the physical WiFi side, the relay's
dummy AP has its own NAT to the internet (via `ipv4.method shared`
in NetworkManager), so their traffic still reaches Google. In the
`--allow-apps` case, the exclusion is implicit — GMS is not in the
allow list.

## Testing

### On KitKat (API 19)

1. Start the relay with `--dummy-wifi`.
2. Verify in logcat: `Fallback 1 succeeded: connected to preferred SSID`.
3. Open a browser and load `http://example.com`.
4. Ctrl+C the relay. Verify: `Restoring previous network '<SSID>'`.

### On Android 9 (API 28) and Android 12+ (API 31+)

Same steps as above. Additionally, on Android 9:

- Verify `Fallback 1 succeeded` appears at least 7 seconds after the
`enableNetwork` call. Earlier appearance means the strict SSID check
is being bypassed and the framework may still be on the old network.
- Verify `dumpsys connectivity` shows both `WIFI` and `VPN` network
agents, both `VALIDATED`.
- Verify the Google account can be added from **Settings → Accounts**,
and that Play Store opens without "no connection".

### On Android 14 (API 34)

`WifiManager.startLocalOnlyHotspot()` requires the runtime permission
`NEARBY_WIFI_DEVICES`. The current manifest does not request it, so
fallback 3 will throw `SecurityException`. Fallback 2 (Wi-Fi Direct)
works and is the recommended path on this API level.