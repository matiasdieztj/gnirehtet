/*
 * Copyright (C) 2026 Genymobile
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

package com.genymobile.gnirehtet;

import android.content.Context;
import android.content.SharedPreferences;
import android.content.pm.PackageManager;
import android.content.BroadcastReceiver;
import android.content.Intent;
import android.content.IntentFilter;
import android.net.ConnectivityManager;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.NetworkInfo;
import android.net.wifi.ScanResult;
import android.net.wifi.WifiConfiguration;
import android.net.wifi.WifiManager;
import android.net.wifi.p2p.WifiP2pManager;
import android.os.Build;
import android.os.Handler;
import android.os.Looper;
import android.util.Log;

import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.net.InetAddress;
import java.net.NetworkInterface;
import java.net.SocketException;
import java.util.Enumeration;

/**
 * Multi-layer fallback to ensure an "active transport" exists on the device
 * so that apps gating on {@code ConnectivityManager.getActiveNetworkInfo()}
 * proceed to open sockets.
 *
 * <p>Background: gnirehtet routes all traffic through the VPN ({@code tun0}).
 * But the framework refuses to promote a VPN-only transport to the system
 * default network on some Android versions. Apps that check for an active
 * network see {@code null} and never open sockets, even though the kernel
 * is ready to route them.
 *
 * <p>When the relay passes a {@code preferredSsid} (via the
 * {@code dummyWifiSsid} intent extra), the fallback switches to that SSID
 * <b>even if another network is already active</b>. This is what makes the
 * behaviour deterministic: the active network is always the one we control.
 * The previous network is remembered in {@code SharedPreferences} and
 * restored by {@link #restorePreviousNetwork(Context)} when the VPN stops.
 *
 * <p>When the preferred SSID is unavailable (relay failed to create the
 * hotspot, or it is out of range), the selection order is:
 * <ol>
 *   <li>Strongest <b>saved network with a password</b> that is in range.</li>
 *   <li>Strongest <b>open network</b> in range.</li>
 * </ol>
 *
 * <p>The relay leaves Android's captive-portal detection enabled (the
 * default value). When the transport works, the native NetworkMonitor
 * probe succeeds and no "Sign in to network" notification is posted.
 * Disabling the detection (as an earlier revision did) also disabled the
 * validation probe, which left the WiFi in a {@code NOT_VALIDATED} state
 * and broke Google account operations on the device.
 */
public class ConnectivityFallback {

    private static final String TAG = "ConnectivityFallback";

    /** Prefix shared by every SSID the relay host may generate. */
    private static final String GNIREHTET_SSID_PREFIX = "gnirehtet-";

    private static final String PREFS_NAME = "gnirehtet_prefs";
    private static final String KEY_PREVIOUS_SSID = "previous_ssid";

    private static final long CREATE_TIMEOUT_MS = 3000;
    private static final long TRANSPORT_POLL_TIMEOUT_MS = 2000;

    /**
     * Poll timeout for the association phase on the preferred (relay) SSID.
     *
     * <p>Android 9+ can spend 10+ seconds in SCANNING before the framework
     * even attempts association, due to scan throttling. KitKat associates
     * in well under a second. Budgeting for the slowest case costs nothing
     * on fast devices.
     */
    private static final long PREFERRED_SSID_ASSOC_TIMEOUT_MS = 45000;

    /**
     * Poll timeout for the DHCP / post-association handshake, applied AFTER
     * the target SSID appears in getConnectionInfo().
     *
     * <p>Shared by every SSID-tracking path. The relay's dnsmasq answers
     * in 5-8 seconds; 15s leaves generous margin for slow DHCP clients.
     */
    private static final long SSID_DHCP_TIMEOUT_MS = 15000;

    /** Poll timeout for the association phase on a generic (non-relay) SSID. */
    private static final long OPEN_AP_ASSOC_TIMEOUT_MS = 8000;

    private static final long WIFI_ENABLE_WAIT_MS = 3000;

    /** Poll interval while waiting for DHCP. */
    private static final long POLL_INTERVAL_MS = 250;

    private final Context context;

    /**
     * SSID the client should prefer, as provided by the relay through the
     * {@code dummyWifiSsid} intent extra. May be {@code null}, in which case
     * the fallback runs the generic selection (saved network, then open
     * network).
     */
    private final String preferredSsid;

    public ConnectivityFallback(Context context, String preferredSsid) {
        this.context = context.getApplicationContext();
        this.preferredSsid = preferredSsid;
        if (preferredSsid != null) {
            Log.d(TAG, "Preferred SSID: " + preferredSsid);
        }
    }

    // ═══════════════════════════════════════════════════════════════════
    // Public entry points
    // ═══════════════════════════════════════════════════════════════════

    public boolean ensureActiveTransport() {
        if (preferredSsid != null) {
            if (isConnectedToSsid(preferredSsid)) {
                Log.d(TAG, "Already connected to preferred SSID '" + preferredSsid + "'");
                return true;
            }
            Log.d(TAG, "Not on preferred SSID; saving current network and switching");
            saveCurrentSsid();
            if (tryConnectToPreferredSsid()) {
                Log.d(TAG, "Fallback 1 succeeded: connected to preferred SSID");
                return true;
            }
            Log.w(TAG, "Could not connect to preferred SSID; falling back to generic chain");
        }

        if (hasActiveTransport()) {
            Log.d(TAG, "Active transport already present");
            return true;
        }

        if (tryWifiP2pGroup()) {
            Log.d(TAG, "Fallback 2 succeeded: WiFi P2P group created");
            return true;
        }

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O && tryLocalOnlyHotspot()) {
            Log.d(TAG, "Fallback 3 succeeded: local-only hotspot started");
            return true;
        }

        if (tryGenericNetworkSelection()) {
            Log.d(TAG, "Fallback 4 succeeded: connected via generic selection");
            return true;
        }

        Log.e(TAG, "All fallbacks failed");
        return false;
    }

    public static void restorePreviousNetwork(Context context) {
        SharedPreferences prefs = context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE);
        String previousSsid = prefs.getString(KEY_PREVIOUS_SSID, null);
        if (previousSsid == null) {
            Log.d(TAG, "No previous network to restore");
            return;
        }

        WifiManager wifiManager = (WifiManager)
                context.getApplicationContext().getSystemService(Context.WIFI_SERVICE);
        if (wifiManager == null) {
            return;
        }

        String currentSsid = normalizeSsid(wifiManager.getConnectionInfo().getSSID());
        if (previousSsid.equals(currentSsid)) {
            Log.d(TAG, "Already on previous network '" + previousSsid + "'");
            prefs.edit().remove(KEY_PREVIOUS_SSID).apply();
            return;
        }

        Log.d(TAG, "Restoring previous network '" + previousSsid + "'");
        try {
            List<WifiConfiguration> configured = wifiManager.getConfiguredNetworks();
            if (configured != null) {
                for (WifiConfiguration config : configured) {
                    if (previousSsid.equals(normalizeSsid(config.SSID))) {
                        // enableNetwork with disableOthers=true atomically
                        // switches from the current network to this one.
                        // Do NOT call disconnect() first: on KitKat, an
                        // explicit disconnect races with the enableNetwork
                        // state transition and can leave the framework idle.
                        wifiManager.enableNetwork(config.networkId, true);
                        break;
                    }
                }
            }
        } catch (SecurityException e) {
            Log.w(TAG, "Cannot restore previous network", e);
        }
        prefs.edit().remove(KEY_PREVIOUS_SSID).apply();
    }

    // ═══════════════════════════════════════════════════════════════════
    // Preferred SSID
    // ═══════════════════════════════════════════════════════════════════

    /**
     * Connect to the SSID the relay asked for. Uses a longer timeout than
     * the generic path, because on KitKat the association + DHCP sequence
     * against a freshly created AP regularly takes more than 5 seconds.
     */
    private boolean tryConnectToPreferredSsid() {
        WifiManager wifiManager = (WifiManager)
                context.getSystemService(Context.WIFI_SERVICE);
        if (wifiManager == null) {
            return false;
        }

        if (!ensureWifiEnabled(wifiManager)) {
            return false;
        }

        if (!tryConnectBySsid(wifiManager, preferredSsid)) {
            Log.w(TAG, "Cannot add/connect network '" + preferredSsid + "'");
            return false;
        }

        return waitForTransport(PREFERRED_SSID_ASSOC_TIMEOUT_MS, wifiManager, preferredSsid);
    }

    // ═══════════════════════════════════════════════════════════════════
    // Generic selection: preferred > saved-secure > open
    // ═══════════════════════════════════════════════════════════════════

    @SuppressWarnings("deprecation")
    private boolean tryGenericNetworkSelection() {
        WifiManager wifiManager = (WifiManager)
                context.getSystemService(Context.WIFI_SERVICE);
        if (wifiManager == null) {
            Log.w(TAG, "WifiManager not available");
            return false;
        }

        if (!ensureWifiEnabled(wifiManager)) {
            return false;
        }

        List<ScanResult> results;
        try {
            results = wifiManager.getScanResults();
        } catch (SecurityException e) {
            Log.w(TAG, "getScanResults requires ACCESS_FINE_LOCATION", e);
            return false;
        }

        if (results == null || results.isEmpty()) {
            Log.d(TAG, "No scan results available");
            return false;
        }

        // ── Tier 1: preferred SSID (exact match) ──
        if (preferredSsid != null) {
            for (ScanResult result : results) {
                if (preferredSsid.equals(result.SSID) && isOpen(result)) {
                    Log.d(TAG, "Found preferred SSID '" + preferredSsid + "' in scan");
                    if (tryConnectBySsid(wifiManager, result.SSID)
                            && waitForTransport(PREFERRED_SSID_ASSOC_TIMEOUT_MS, wifiManager, result.SSID)) {
                        return true;
                    }
                }
            }
        }

        // ── Tier 2: strongest saved network that requires a password ──
        List<WifiConfiguration> saved = null;
        try {
            saved = wifiManager.getConfiguredNetworks();
        } catch (SecurityException e) {
            Log.w(TAG, "getConfiguredNetworks failed", e);
        }

        ScanResult bestSavedSecure = pickStrongest(results, saved, true);
        if (bestSavedSecure != null) {
            Log.d(TAG, "Trying strongest saved secure network '"
                    + bestSavedSecure.SSID + "' (signal " + bestSavedSecure.level + ")");
            if (tryConnectBySsid(wifiManager, bestSavedSecure.SSID)
                    && waitForTransport(OPEN_AP_ASSOC_TIMEOUT_MS, wifiManager, bestSavedSecure.SSID)) {
                return true;
            }
        }

        // ── Tier 3: strongest open network ──
        ScanResult bestOpen = pickStrongest(results, saved, false);
        if (bestOpen != null) {
            Log.d(TAG, "Trying strongest open network '"
                    + bestOpen.SSID + "' (signal " + bestOpen.level + ")");
            if (tryConnectBySsid(wifiManager, bestOpen.SSID)
                    && waitForTransport(OPEN_AP_ASSOC_TIMEOUT_MS, wifiManager, bestOpen.SSID)) {
                return true;
            }
        }

        Log.d(TAG, "No suitable network found in scan results");
        return false;
    }

    private static ScanResult pickStrongest(
            List<ScanResult> results,
            List<WifiConfiguration> savedNetworks,
            boolean requireSavedAndSecure) {
        ScanResult best = null;
        for (ScanResult result : results) {
            boolean match;
            if (requireSavedAndSecure) {
                match = !isOpen(result) && isSaved(savedNetworks, result.SSID);
            } else {
                match = isOpen(result);
            }
            if (!match) {
                continue;
            }
            if (best == null || result.level > best.level) {
                best = result;
            }
        }
        return best;
    }

    private static boolean isSaved(List<WifiConfiguration> savedNetworks, String ssid) {
        if (savedNetworks == null) {
            return false;
        }
        String quotedSsid = "\"" + ssid + "\"";
        for (WifiConfiguration config : savedNetworks) {
            if (quotedSsid.equals(config.SSID)) {
                return true;
            }
        }
        return false;
    }

    // ═══════════════════════════════════════════════════════════════════
    // P2P and local-only hotspot
    // ═══════════════════════════════════════════════════════════════════

    /**
     * Check whether the device has any active physical transport (WiFi or
     * cellular) with INTERNET capability, regardless of which network is
     * currently the system default.
     *
     * <p>This is NOT the same as {@code getActiveNetworkInfo()}. When the
     * gnirehtet VPN is running, the VPN network has a higher score than the
     * underlying WiFi and becomes the default. {@code getActiveNetworkInfo()}
     * then returns the VPN, and any check that filters out VPN transports
     * sees no physical transport at all — even when the WiFi is CONNECTED
     * and VALIDATED behind it.
     *
     * <p>On API 21+, iterate {@code getAllNetworks()} and accept any network
     * with a WIFI or CELLULAR transport and NET_CAPABILITY_INTERNET. On
     * older platforms (KitKat), fall back to the legacy
     * {@code getActiveNetworkInfo()} check: on API 19 the VPN is not
     * promoted to default the same way, and the legacy check works.
     */
    @SuppressWarnings("deprecation")
    private boolean hasActiveTransport() {
        ConnectivityManager cm = (ConnectivityManager)
                context.getSystemService(Context.CONNECTIVITY_SERVICE);
        if (cm == null) {
            return false;
        }

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP) {
            Network[] networks = cm.getAllNetworks();
            if (networks != null) {
                for (Network network : networks) {
                    NetworkCapabilities caps = cm.getNetworkCapabilities(network);
                    if (caps == null) {
                        continue;
                    }
                    // Skip the VPN itself; we want the physical transport
                    // underneath it.
                    if (caps.hasTransport(NetworkCapabilities.TRANSPORT_VPN)) {
                        continue;
                    }
                    boolean isPhysical =
                            caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)
                            || caps.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR);
                    if (isPhysical
                            && caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)) {
                        return true;
                    }
                }
            }
            return false;
        }

        // API < 21: legacy behaviour. The VPN is not promoted to the
        // system default on KitKat the same way, so getActiveNetworkInfo()
        // still reflects the physical transport.
        NetworkInfo info = cm.getActiveNetworkInfo();
        return info != null && info.isConnected();
    }

    private boolean tryWifiP2pGroup() {
    if (!context.getPackageManager().hasSystemFeature(PackageManager.FEATURE_WIFI_DIRECT)) {
        Log.w(TAG, "Device does not advertise FEATURE_WIFI_DIRECT");
        return false;
    }

    WifiP2pManager manager = (WifiP2pManager)
            context.getSystemService(Context.WIFI_P2P_SERVICE);
    if (manager == null) {
        Log.w(TAG, "WifiP2pManager not available");
        return false;
    }

    final CountDownLatch createLatch = new CountDownLatch(1);
    final CountDownLatch connectLatch = new CountDownLatch(1);
    final boolean[] createSuccess = {false};
    final boolean[] connectSuccess = {false};

    // Register the connection-changed receiver BEFORE createGroup: on
    // some ROMs the broadcast fires immediately after the group is
    // created, and registering afterwards would miss it.
    final BroadcastReceiver receiver = new BroadcastReceiver() {
        @Override
        public void onReceive(Context ctx, Intent intent) {
            if (WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION
                    .equals(intent.getAction())) {
                NetworkInfo info = intent.getParcelableExtra(
                        WifiP2pManager.EXTRA_NETWORK_INFO);
                if (info != null && info.isConnected()) {
                    Log.d(TAG, "P2P connection broadcast: connected");
                    connectSuccess[0] = true;
                    connectLatch.countDown();
                } else {
                    Log.d(TAG, "P2P connection broadcast: not connected");
                }
            }
        }
    };
    IntentFilter filter = new IntentFilter();
    filter.addAction(WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION);
    context.registerReceiver(receiver, filter);

    try {
        WifiP2pManager.Channel channel =
                manager.initialize(context, Looper.getMainLooper(), null);
        if (channel == null) {
            Log.w(TAG, "WifiP2pManager.initialize returned null channel");
            return false;
        }

        manager.createGroup(channel, new WifiP2pManager.ActionListener() {
            @Override
            public void onSuccess() {
                Log.d(TAG, "WiFi P2P createGroup succeeded");
                createSuccess[0] = true;
                createLatch.countDown();
            }

            @Override
            public void onFailure(int reason) {
                Log.w(TAG, "createGroup failed with reason code " + reason);
                createLatch.countDown();
            }
        });

        try {
            if (!createLatch.await(CREATE_TIMEOUT_MS, TimeUnit.MILLISECONDS)) {
                Log.w(TAG, "WiFi P2P createGroup timed out");
                return false;
            }
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            return false;
        }

        if (!createSuccess[0]) {
            return false;
        }

        // Wait for either signal:
        //   (a) the WIFI_P2P_CONNECTION_CHANGED broadcast — canonical,
        //       but not tracked by ConnectivityManager on all ROMs; or
        //   (b) the p2p0 interface coming up with a non-loopback address —
        //       last-resort for ROMs where the broadcast never fires.
        //
        // We intentionally do NOT gate on hasActiveTransport() here:
        // ConnectivityManager does not register the P2P interface as a
        // "network" on Android 4.4 nor on many Android 9 ROMs, so that
        // check always returned false even when P2P was fully up. That
        // is exactly the "framework never reported connected" failure
        // this method used to hit.
        long deadline = System.currentTimeMillis() + TRANSPORT_POLL_TIMEOUT_MS;
        while (System.currentTimeMillis() < deadline) {
            if (connectSuccess[0]) {
                Log.d(TAG, "P2P confirmed via broadcast");
                return true;
            }
            if (hasP2pInterface()) {
                Log.d(TAG, "P2P confirmed via p2p0 interface");
                return true;
            }
            try {
                Thread.sleep(POLL_INTERVAL_MS);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                return false;
            }
        }

        Log.w(TAG, "Timed out: P2P group created but no connection signal "
                + "(broadcast=" + connectSuccess[0]
                + ", p2p0=" + hasP2pInterface() + ")");
        return false;
    } finally {
        try {
            context.unregisterReceiver(receiver);
        } catch (IllegalArgumentException e) {
            // already unregistered; safe to ignore
        }
    }
}

/**
 * Check whether the P2P network interface (usually {@code p2p0}) is up
 * and has a non-loopback address.
 *
 * <p>Android does not track the P2P interface as an active network in
 * {@code ConnectivityManager} on all ROMs — notably on KitKat (API 19)
 * and on some Android 9 builds. The broadcast receiver covers most cases;
 * this interface check is the last-resort confirmation.
 */
private static boolean hasP2pInterface() {
    try {
        NetworkInterface ni = NetworkInterface.getByName("p2p0");
        if (ni == null || !ni.isUp()) {
            return false;
        }
        Enumeration<InetAddress> addrs = ni.getInetAddresses();
        while (addrs.hasMoreElements()) {
            InetAddress addr = addrs.nextElement();
            if (!addr.isLoopbackAddress()) {
                return true;
            }
        }
    } catch (SocketException e) {
        Log.w(TAG, "p2p0 lookup failed", e);
    }
    return false;
}

    private boolean tryLocalOnlyHotspot() {
        WifiManager wifiManager = (WifiManager)
                context.getSystemService(Context.WIFI_SERVICE);
        if (wifiManager == null) {
            Log.w(TAG, "WifiManager not available");
            return false;
        }

        final CountDownLatch latch = new CountDownLatch(1);
        final boolean[] success = {false};

        try {
            wifiManager.startLocalOnlyHotspot(
                    new WifiManager.LocalOnlyHotspotCallback() {
                        @Override
                        public void onStarted(WifiManager.LocalOnlyHotspotReservation reservation) {
                            Log.d(TAG, "Local-only hotspot started");
                            success[0] = true;
                            latch.countDown();
                        }

                        @Override
                        public void onFailed(int reason) {
                            Log.w(TAG, "Local-only hotspot failed: " + reason);
                            latch.countDown();
                        }

                        @Override
                        public void onStopped() {
                            Log.d(TAG, "Local-only hotspot stopped");
                        }
                    },
                    new Handler(Looper.getMainLooper()));
        } catch (SecurityException e) {
            Log.w(TAG, "Local-only hotspot requires CHANGE_WIFI_STATE", e);
            return false;
        }

        try {
            if (!latch.await(CREATE_TIMEOUT_MS, TimeUnit.MILLISECONDS)) {
                Log.w(TAG, "Local-only hotspot timed out");
                return false;
            }
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            return false;
        }

        if (!success[0]) {
            return false;
        }

        return waitForTransport(TRANSPORT_POLL_TIMEOUT_MS, null, null);
    }

    // ═══════════════════════════════════════════════════════════════════
    // Helpers
    // ═══════════════════════════════════════════════════════════════════

    @SuppressWarnings("deprecation")
    private boolean ensureWifiEnabled(WifiManager wifiManager) {
        if (wifiManager.isWifiEnabled()) {
            return true;
        }
        Log.d(TAG, "Wi-Fi is disabled; enabling it");
        try {
            wifiManager.setWifiEnabled(true);
        } catch (SecurityException e) {
            Log.w(TAG, "setWifiEnabled requires CHANGE_WIFI_STATE", e);
            return false;
        }
        try {
            Thread.sleep(WIFI_ENABLE_WAIT_MS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
            return false;
        }
        return true;
    }

    /**
     * Connect to the given SSID.
     *
     * <p>Uses {@code enableNetwork(netId, true)} alone. The {@code true}
     * flag already disconnects from the current network and disables all
     * other networks atomically, so we must NOT call {@code disconnect()}
     * first — on KitKat that races with the state machine and can leave
     * the framework idle with no pending association. Likewise,
     * {@code reconnect()} is not called: it is for retrying a failed
     * association and can cancel a freshly started one.
     */
    private static boolean tryConnectBySsid(WifiManager wifiManager, String ssid) {
        int netId = findOrAddOpenNetwork(wifiManager, ssid);
        if (netId == -1) {
            return false;
        }
        try {
            return wifiManager.enableNetwork(netId, true);
        } catch (SecurityException e) {
            return false;
        }
    }

    @SuppressWarnings("deprecation")
    private static int findOrAddOpenNetwork(WifiManager wifiManager, String ssid) {
        String quotedSsid = "\"" + ssid + "\"";
        int staleNetId = -1;
        try {
            List<WifiConfiguration> configured = wifiManager.getConfiguredNetworks();
            if (configured != null) {
                for (WifiConfiguration config : configured) {
                    if (quotedSsid.equals(config.SSID)) {
                        staleNetId = config.networkId;
                        break;
                    }
                }
            }
        } catch (SecurityException e) {
            // fall through
        }

        // Delete the existing entry so the config is recreated with the
        // correct (open) security parameters. Reusing a stale config whose
        // key management does not match the AP produces driver-level
        // "Assocation Rejection reason code=1" (UNSPECIFIED_FAILURE), because
        // the supplicant tries the wrong handshake.
        if (staleNetId != -1) {
            try {
                wifiManager.removeNetwork(staleNetId);
            } catch (SecurityException e) {
                // best-effort: the recreated config will still be added
            }
        }

        WifiConfiguration config = new WifiConfiguration();
        config.SSID = quotedSsid;
        config.allowedKeyManagement.set(WifiConfiguration.KeyMgmt.NONE);
        return wifiManager.addNetwork(config);
    }

    @SuppressWarnings("deprecation")
    private boolean isConnectedToSsid(String ssid) {
        WifiManager wifiManager = (WifiManager)
                context.getSystemService(Context.WIFI_SERVICE);
        if (wifiManager == null) {
            return false;
        }
        String current = normalizeSsid(wifiManager.getConnectionInfo().getSSID());
        return ssid.equals(current);
    }

    private static String normalizeSsid(String ssid) {
        if (ssid == null) {
            return null;
        }
        if (ssid.length() >= 2 && ssid.startsWith("\"") && ssid.endsWith("\"")) {
            return ssid.substring(1, ssid.length() - 1);
        }
        return ssid;
    }

    @SuppressWarnings("deprecation")
    private void saveCurrentSsid() {
        WifiManager wifiManager = (WifiManager)
                context.getSystemService(Context.WIFI_SERVICE);
        if (wifiManager == null) {
            return;
        }
        String current = normalizeSsid(wifiManager.getConnectionInfo().getSSID());
        if (current == null || current.isEmpty() || "<unknown ssid>".equals(current)) {
            return;
        }
        if (current.startsWith(GNIREHTET_SSID_PREFIX)) {
            return;
        }
        context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
                .edit()
                .putString(KEY_PREVIOUS_SSID, current)
                .apply();
        Log.d(TAG, "Saved previous SSID: " + current);
    }

    private static boolean isOpen(ScanResult result) {
        return !result.capabilities.contains("WPA") && !result.capabilities.contains("WEP");
    }

    /**
     * Poll until the target SSID is confirmed as connected AND the framework
     * reports an active network.
     *
     * <p>When {@code wifiManager} and {@code targetSsid} are non-null, the
     * check is strict: {@code hasActiveTransport()} is only trusted once
     * {@code getConnectionInfo().getSSID()} matches the target. This avoids
     * the race observed on KitKat where the old network is still reported as
     * active for a short window after {@code enableNetwork()} returns, which
     * used to make the fallback declare success while still associated with
     * the previous network.
     *
     * <p>When the SSID arguments are null (P2P and local-only hotspot paths),
     * the behaviour is the original: just wait for {@code hasActiveTransport()}
     * to become true.
     */
    @SuppressWarnings("deprecation")
    private boolean waitForTransport(long timeoutMs, WifiManager wifiManager, String targetSsid) {
        long deadline = System.currentTimeMillis() + timeoutMs;
        long lastLog = 0;
        long lastRetry = 0;
        boolean associated = false;

        while (System.currentTimeMillis() < deadline) {
            if (wifiManager != null && targetSsid != null) {
                String current = normalizeSsid(wifiManager.getConnectionInfo().getSSID());
                if (!associated) {
                    if (targetSsid.equals(current)) {
                        associated = true;
                        // The caller's timeout covers only the association
                        // phase. Once we're associated, DHCP gets its own
                        // budget so a slow scan cannot starve the handshake.
                        deadline = System.currentTimeMillis() + SSID_DHCP_TIMEOUT_MS;
                        Log.d(TAG, "Associated with '" + targetSsid + "'; waiting for framework");
                    } else {
                        long now = System.currentTimeMillis();
                        // Re-issue enableNetwork periodically. The framework
                        // performs its own association attempts when the
                        // target network is set as preferred, but gives up
                        // after 1-2 tries (observed: ~3 seconds). Without
                        // re-triggering, the framework falls back to the
                        // previous network and stays there, even though the
                        // target AP is reachable. Retrying every 12 seconds
                        // also gives the Android 9 scan throttle (4 scans
                        // per 2 minutes) a chance to recover between
                        // attempts.
                        if (now - lastRetry > 12000) {
                            Log.d(TAG, "Re-issuing enableNetwork for '" + targetSsid + "'");
                            tryConnectBySsid(wifiManager, targetSsid);
                            lastRetry = now;
                        }
                        if (now - lastLog > 3000) {
                            Log.d(TAG, "Waiting for '" + targetSsid + "'; current SSID='" + current + "'");
                            lastLog = now;
                        }
                        try {
                            Thread.sleep(POLL_INTERVAL_MS);
                        } catch (InterruptedException e) {
                            Thread.currentThread().interrupt();
                            return false;
                        }
                        continue;
                    }
                }
                // Once associated, keep polling until the framework also
                // reports an active network. This is what apps gate on.
                if (associated && hasActiveTransport()) {
                    return true;
                }
            } else {
                // No SSID tracking (P2P / local-only hotspot): the original
                // behaviour, hasActiveTransport() is the sole signal.
                if (hasActiveTransport()) {
                    return true;
                }
            }

            try {
                Thread.sleep(POLL_INTERVAL_MS);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                return false;
            }
        }

        // One last check at the deadline.
        if (wifiManager != null && targetSsid != null) {
            String current = normalizeSsid(wifiManager.getConnectionInfo().getSSID());
            if (!targetSsid.equals(current)) {
                Log.w(TAG, "Timed out: never associated with '" + targetSsid
                        + "' (current SSID='" + current + "')");
                return false;
            }
        }
        if (!hasActiveTransport()) {
            Log.w(TAG, "Timed out: associated but framework never reported connected");
            return false;
        }
        return true;
    }
}
