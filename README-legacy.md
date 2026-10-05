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
```

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

