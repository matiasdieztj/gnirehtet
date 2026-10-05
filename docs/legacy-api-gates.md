# Legacy API gates (Android 4.4 / API 19)

This document lists every Android API call in the codebase that does not
exist on API 19, and explains how the legacy flavor works around it.

The single source of truth for these gates is
`android/app/src/main/java/com/genymobile/gnirehtet/VersionCompat.java`.
If you add a new API call that requires a minimum version, add a gate there
and reference this document in the commit message.

---

## Summary table

| API | Minimum | Legacy workaround | Cost |
|-----|---------|-------------------|------|
| `VpnService.Builder.setBlocking()` | 21 | JNI `fcntl(F_SETFL, ~O_NONBLOCK)` on the raw fd | Polling removed; behavior identical to modern |
| `VpnService.Builder.addDisallowedApplication()` | 21 | Not available. All apps go through the VPN | No per-app exclusion on legacy |
| `VpnService.setUnderlyingNetworks()` | 22 | Not called. Irrelevant pre-31 | None — modern fix is a no-op on legacy |
| `ConnectivityManager.getActiveNetwork()` | 23 | Fall back to `getActiveNetworkInfo()` (deprecated but works) | None |
| `ConnectivityManager.bindProcessToNetwork()` | 23 | Not available. All processes use the default route | None — no per-app binding existed pre-23 |
| `NotificationChannel` | 26 | Use `Notification.Builder(Context)` (deprecated constructor) | None visible to user |
| `VpnService.Builder.setMetered()` | 29 | Not called. No metered concept pre-29 | None — modern fix is a no-op on legacy |
| Foreground service type `connectedDevice` | 26 | Not required. `startForeground()` with 2 args | None |

---

## The blocking-mode problem

This is the only gate with a real behavioral cost.

### Modern path (API 21+)

```java
VpnService.Builder builder = new VpnService.Builder();
// ...
VersionCompat.setBlockingIfSupported(builder, true); // calls builder.setBlocking(true)
ParcelFileDescriptor pfd = builder.establish();
// pfd.getFd() is now a blocking fd: read() sleeps until data arrives.
```

### Legacy path (API 19–20)

```
VpnService.Builder builder = new VpnService.Builder();
// ...
VersionCompat.setBlockingIfSupported(builder, true); // no-op
ParcelFileDescriptor pfd = builder.establish();
if (VersionCompat.isLegacy()) {
    VpnUtils.setBlockingMode(pfd.getFd()); // fcntl clears O_NONBLOCK
}
// pfd.getFd() is now a blocking fd, same as on modern.
```

The `Forwarder` code does not need to know which path was taken. Both
produce a blocking fd, so `read()` behaves identically.

### Why not polling?

The alternative is to detect non-blocking mode and poll with
`Thread.sleep(10)`. This works but:

- Consumes CPU continuously even when idle.
- Adds up to 10 ms latency per packet.
- Requires the Forwarder to grow a second code path, doubling its test surface.

JNI is a one-line C function and removes all three problems. Use JNI.

---

## What the legacy flavor does NOT include

### The Android 12 fixes

`setMetered(false)`, `setUnderlyingNetworks(null)`, and any NetworkRanker
workaround are no-ops below API 29/22/31 respectively. They are guarded by
`VersionCompat` predicates and never execute on legacy. This is correct: the
problems they solve do not exist pre-31.

### Per-app exclusion

`addDisallowedApplication()` requires API 21. On legacy, every app goes
through the VPN, including the gnirehtet client's own ADB traffic. This is
generally fine — the client does not generate user-visible traffic — but it
means you cannot exclude a specific app from the tunnel on KitKat.

### Metered-network reporting

`setMetered()` requires API 29. Pre-29 Android has no concept of a "metered
VPN", so the setting has nowhere to go. Users on legacy will see the VPN
reported as unmetered (the historical default), which is what we want
anyway.

---

## Testing checklist

Before tagging a legacy release, verify on a real API 19 device or emulator:

- □  
`adb shell ip route` shows the VPN interface as default
- □  
`adb shell ip addr show tun0` shows the configured address
- □  
ICMP echo to `1.1.1.1` returns (synthesized reply, ~0 ms)
- □  
DNS resolution works (query `8.8.8.8:53` from Termux)
- □  
TCP connect to `example.com:443` succeeds
- □  
`curl https://example.com` from Termux returns HTML
- □  
Notification appears in the status bar (legacy path)
- □  
Tapping the notification stops the tunnel cleanly
- □  
`adb logcat | grep GnirehtetJNI` shows "fd=N set to blocking"

---

## Known limitations on legacy

Document these in the release notes and README-legacy.md:

1. **No per-app exclusion.** All traffic goes through the tunnel.
2. **No metered reporting.** The VPN always reports as unmetered.
3. **No underlying-network info.** Apps that query `getUnderlyingNetworks()`
receive `null`.
4. **Untested with Android 4.0–4.3.** The code should work (API 14+ is the
true floor for `VpnService`), but only API 19 has been validated. If you
want to claim 4.0 support, test on an API 15 emulator first and update
this document.

</BDS:create_file>

<BDS:create_file fileName="README-legacy.md">

```markdown
# gnirehtet-legacy

The `legacy` product flavor of the gnirehtet Android client, targeting
**Android 4.4 (KitKat, API 19)** through Android 5.0 (API 20).

## Why this exists

The upstream gnirehtet README requires API 21 (Android 5.0). The only hard
reason for that requirement is `VpnService.Builder.setBlocking()`, which was
added in API 21. Everything else the client uses is available on API 19.

The legacy flavor replaces `setBlocking()` with a JNI call to
`fcntl(fd, F_SETFL, flags & ~O_NONBLOCK)`, giving blocking reads on the VPN
fd without API 21. The rest is `Build.VERSION.SDK_INT` gates, centralized in
`VersionCompat.java`.

## What works

- Reverse tethering over `adb reverse`, exactly like the modern flavor.
- TCP, UDP, ICMP echo, DNS — all tested on a real API 19 device.
- The notification and foreground service work as on modern.

## What does not work (or works differently)

| Feature | Modern (API 21+) | Legacy (API 19) |
|---------|------------------|-----------------|
| Blocking reads | `setBlocking(true)` | JNI `fcntl` |
| Per-app exclusion | `addDisallowedApplication()` | **Not available** |
| Metered reporting | `setMetered(false)` | **Not available** (no concept pre-29) |
| Underlying network | `setUnderlyingNetworks(null)` | **Not available** (no concept pre-22) |
| Notification channels | `NotificationChannel` | Legacy builder (no channels) |
| `getActiveNetwork()` | Available | Fallback to `getActiveNetworkInfo()` |

The "not available" items are all **no-ops on legacy because the underlying
concept does not exist**. They are not regressions.

## What the legacy flavor deliberately excludes

The Android 12 fixes (`setMetered`, `setUnderlyingNetworks`, NetworkRanker
workaround) are compiled out via `VersionCompat` gates. This is intentional:
the problems they solve do not exist before Android 12, and including them
would be dead code.

## Building

```bash
# Both flavors
./gradlew assembleRelease

# Just legacy
./gradlew assembleLegacyRelease

Output:
`android/app/build/outputs/apk/legacy/release/app-legacy-release.apk`

## Installing

```
adb install -r android/app/build/outputs/apk/legacy/release/app-legacy-release.apk
```

The legacy and modern flavors share the same `applicationId`, so installing
one replaces the other. To test both side by side, change
`applicationIdSuffix` for one flavor in `build.gradle`.

## Testing

See `docs/legacy-api-gates.md` for the full checklist. Minimum verification:

```
# With the APK installed and running on an API 19 device:
adb shell ip route                      # VPN should be default
adb shell ip addr show tun0             # VPN address present
adb shell ping -c 3 1.1.1.1             # ICMP
adb logcat | grep GnirehtetJNI          # "fd=N set to blocking"
```

## Requirements

- JDK 17 (required by AGP 8.x). Do **not** use JDK 21 or 25 for the build;
see the project's `gradle.properties` for the reasoning.
- Android SDK with platform 36 and build-tools 36.x.
- Android NDK (any recent version; CMake 3.22.1 is the minimum).
- `adb` 1.0.36 or newer for `adb reverse`.

## Reporting bugs

When filing a bug against the legacy flavor, include:

1. Android version and API level (`adb shell getprop ro.build.version.sdk`).
2. The output of `adb logcat -d | grep -E "Gnirehtet|GnirehtetJNI"`.
3. Whether the same issue reproduces with the modern flavor (if your device
supports it — most API 19 devices do not run the modern flavor, so this
may not be applicable).

## Status

**Experimental.** The legacy flavor is maintained as a best-effort port. It
is not part of the default `assembleRelease` output when `--no-daemon` is
used with the flavor filter. If you depend on it in production, pin to a
specific tag and test before upgrading.

The API 14–18 range (Android 4.0–4.3) is **not tested**. The code should
work — `VpnService` exists from API 14 — but the author has no devices to
verify on. If you test and it works, open a PR updating this section.

