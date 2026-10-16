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
import android.net.wifi.WifiNetworkSuggestion;
import android.net.wifi.SupplicantState;
import android.net.wifi.WifiInfo;

import java.util.List;
import java.util.Collections;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.net.InetAddress;
import java.net.NetworkInterface;
import java.net.SocketException;
import java.util.Enumeration;
import java.lang.reflect.Method;

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
 * {@code dummyWifiSsid} intent extra), the fallback tries to switch to
 * that SSID. On API 19-28 this is a forced switch via {@code addNetwork()}
 * and {@code enableNetwork()}; on API 29+ the SSID is registered as a
 * {@link WifiNetworkSuggestion} and the framework decides whether to
 * follow it. In practice the framework only switches when no other Wi-Fi
 * network is currently connected with internet: a suggestion is treated
 * as a preference, not a command. When the device is already on a
 * working Wi-Fi, the dummy AP is redundant — the existing network
 * already provides the non-VPN transport that the fallback chain needs —
 * and {@code hasActiveTransport()} returns true on the first check.
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
    private static final String KEY_ENABLED_NETWORK_IDS = "enabled_network_ids";

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
     * Poll timeout for the suggestion-based path on API 29+.
     *
     * <p>The framework schedules the scan that discovers a suggested AP on
     * its own timeline; on the A21s the observed delay between
     * {@code addNetworkSuggestions()} and {@code CMD_START_CONNECT} was
     * around 60 seconds. 90 seconds leaves room for a slower device
     * without blocking the fallback chain for too long.
     */
    private static final long SUGGESTION_ASSOC_TIMEOUT_MS = 90000;

    /**
     * Interval between best-effort {@code startScan()} calls while waiting
     * for the framework to associate with a suggested network.
     *
     * <p>{@code startScan()} may be silently throttled by the platform, so
     * it is called as a hint, not as a guarantee. The poll loop re-reads
     * {@code getConnectionInfo()} regardless of whether the scan call was
     * accepted.
     */
    private static final long SUGGESTION_SCAN_INTERVAL_MS = 20000;

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
        // Drop any network suggestion left over from the preferred-SSID path,
        // unconditionally, before checking whether a previous network was
        // saved. Suggestions persist across reboots, so the cleanup must run
        // on every stop even when the client never switched networks.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            removeAllSuggestions(context);
        }

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

        // KitKat: enableNetwork(..., true) permanently disabled every other
        // WiFi configuration. Restore them before attempting to switch back.
        if (Build.VERSION.SDK_INT == Build.VERSION_CODES.KITKAT) {
            reEnableRememberedNetworks(context, wifiManager);
        }

        String currentSsid = getConnectedSsid(context);
        Log.d(TAG, "Restore: previous='" + previousSsid + "' current='" + currentSsid + "'");
        if (previousSsid.equals(currentSsid)) {
            Log.d(TAG, "Already on previous network '" + previousSsid + "'");
            prefs.edit().remove(KEY_PREVIOUS_SSID).apply();
            return;
        }

        Log.d(TAG, "Restoring previous network '" + previousSsid + "'");

        // Locate the netId for the previous SSID. On KitKat the WiFi config
        // list can be briefly inconsistent after a network switch, so the
        // lookup is retried a few times before giving up.
        int netId = -1;
        for (int attempt = 1; attempt <= 3 && netId == -1; attempt++) {
            try {
                List<WifiConfiguration> configured = wifiManager.getConfiguredNetworks();
                if (configured != null) {
                    for (WifiConfiguration config : configured) {
                        if (previousSsid.equals(normalizeSsid(config.SSID))) {
                            netId = config.networkId;
                            break;
                        }
                    }
                }
                Log.d(TAG, "Restore lookup attempt " + attempt
                        + ": netId=" + netId
                        + " (configured size=" + (configured == null ? "null" : configured.size()) + ")");
            } catch (SecurityException e) {
                Log.w(TAG, "Cannot read configured networks (attempt " + attempt + ")", e);
            }
            if (netId == -1 && attempt < 3) {
                try {
                    Thread.sleep(1000);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    prefs.edit().remove(KEY_PREVIOUS_SSID).apply();
                    return;
                }
            }
        }

        if (netId == -1) {
            Log.w(TAG, "Previous network '" + previousSsid
                    + "' is not in the configured-networks list; cannot restore");
            prefs.edit().remove(KEY_PREVIOUS_SSID).apply();
            return;
        }

        // enableNetwork with disableOthers=true atomically switches from the
        // current network to this one. Do NOT call disconnect() first: on
        // KitKat that races with the state machine.
        boolean enableOk = false;
        try {
            enableOk = wifiManager.enableNetwork(netId, true);
            Log.d(TAG, "enableNetwork(" + netId + ", true) returned " + enableOk);
        } catch (SecurityException e) {
            Log.w(TAG, "enableNetwork threw", e);
        }

        // Wait for the framework to actually associate. On KitKat the
        // association is not triggered synchronously by enableNetwork: the
        // framework schedules it on its own timeline. If nothing happens
        // after 15 s, nudge with reconnect() once.
        boolean associated = waitForAssociation(context, previousSsid, 15000);
        if (!associated && enableOk) {
            Log.d(TAG, "No association after 15 s; issuing reconnect() as a nudge");
            try {
                wifiManager.reconnect();
            } catch (SecurityException e) {
                Log.w(TAG, "reconnect() threw", e);
            }
            associated = waitForAssociation(context, previousSsid, 15000);
        }

        if (associated) {
            Log.d(TAG, "Restored previous network '" + previousSsid + "'");
        } else {
            Log.w(TAG, "Could not restore previous network '" + previousSsid + "'");
        }

        prefs.edit().remove(KEY_PREVIOUS_SSID).apply();
    }

    /**
     * Re-enable the WiFi configurations that were enabled before the most
     * recent KitKat disableOthers=true switch.
     *
     * <p>On KitKat, {@code enableNetwork(id, false)} does not clear the
     * DISABLED status that a prior disableOthers=true switch set. The only
     * reliable way is to mutate the config's status and persist it via the
     * hidden {@code saveConfiguration(WifiConfiguration)} API.
     */
    @SuppressWarnings("deprecation")
    private static void reEnableRememberedNetworks(Context context, WifiManager wifiManager) {
        SharedPreferences prefs = context.getApplicationContext()
                .getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE);
        String raw = prefs.getString(KEY_ENABLED_NETWORK_IDS, null);
        if (raw == null || raw.isEmpty()) {
            Log.d(TAG, "No remembered network ids to re-enable");
            return;
        }

        List<WifiConfiguration> configured;
        try {
            configured = wifiManager.getConfiguredNetworks();
        } catch (SecurityException e) {
            Log.w(TAG, "Cannot list configured networks to re-enable", e);
            return;
        }
        if (configured == null) {
            Log.w(TAG, "Cannot re-enable: configured list is null");
            return;
        }

        // saveConfiguration() is hidden and not present on every KitKat ROM
        // (Fire OS 4 in particular lacks it). updateNetwork() is the public
        // API 23+ equivalent and exists as a hidden method since API 1, so
        // reflection usually reaches it. If neither is available, fall back
        // to enableNetwork(id, false).
        Method saveMethod = null;
        String methodName = null;
        try {
            saveMethod = WifiManager.class.getMethod(
                    "saveConfiguration", WifiConfiguration.class);
            methodName = "saveConfiguration";
        } catch (NoSuchMethodException e) {
            try {
                saveMethod = WifiManager.class.getMethod(
                        "updateNetwork", WifiConfiguration.class);
                methodName = "updateNetwork";
            } catch (NoSuchMethodException e2) {
                Log.w(TAG, "Neither saveConfiguration() nor updateNetwork() "
                        + "available; falling back to enableNetwork");
            }
        }

        int reenabled = 0;
        for (String part : raw.split(",")) {
            int id;
            try {
                id = Integer.parseInt(part.trim());
            } catch (NumberFormatException ignored) {
                continue;
            }
            for (WifiConfiguration config : configured) {
                if (config.networkId != id) {
                    continue;
                }
                boolean ok = false;
                if (config.status != WifiConfiguration.Status.ENABLED) {
                    config.status = WifiConfiguration.Status.ENABLED;
                    if (saveMethod != null) {
                        try {
                            Object result = saveMethod.invoke(wifiManager, config);
                            if ("updateNetwork".equals(methodName)) {
                                // updateNetwork returns the netId (>=0) on
                                // success, -1 on failure.
                                ok = (result instanceof Integer)
                                        && ((Integer) result) >= 0;
                            } else {
                                ok = Boolean.TRUE.equals(result);
                            }
                        } catch (Exception e) {
                            Log.w(TAG, methodName + " threw for id " + id, e);
                        }
                    }
                }
                if (!ok) {
                    try {
                        ok = wifiManager.enableNetwork(id, false);
                    } catch (SecurityException e) {
                        Log.w(TAG, "enableNetwork(" + id + ", false) threw", e);
                    }
                }
                if (ok) {
                    reenabled++;
                }
                break;
            }
        }
        Log.d(TAG, "Re-enabled " + reenabled + " network(s) via "
                + (methodName != null ? methodName : "enableNetwork"));
        prefs.edit().remove(KEY_ENABLED_NETWORK_IDS).apply();
    }

    /**
     * Poll until the supplicant reports the target SSID as fully connected.
     *
     * <p>Do not trust WifiInfo.getSSID() alone on KitKat: the framework
     * updates that field as soon as the "preferred" configuration changes,
     * even before the supplicant has associated. The reliable signals are
     * SupplicantState.COMPLETED together with a non-negative network id.
     */
    @SuppressWarnings("deprecation")
    private static boolean waitForAssociation(
            Context context, String targetSsid, long timeoutMs) {
        WifiManager wifiManager = (WifiManager)
                context.getApplicationContext().getSystemService(Context.WIFI_SERVICE);
        if (wifiManager == null) {
            return false;
        }
        long deadline = System.currentTimeMillis() + timeoutMs;
        while (System.currentTimeMillis() < deadline) {
            WifiInfo info = wifiManager.getConnectionInfo();
            if (info != null) {
                String current = normalizeSsid(info.getSSID());
                SupplicantState state = info.getSupplicantState();
                int netId = info.getNetworkId();
                if (targetSsid.equals(current)
                        && state == SupplicantState.COMPLETED
                        && netId != -1) {
                    Log.d(TAG, "Association confirmed: SSID='" + current
                            + "' state=" + state + " netId=" + netId);
                    return true;
                }
            }
            try {
                Thread.sleep(500);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                return false;
            }
        }
        WifiInfo last = wifiManager.getConnectionInfo();
        if (last != null) {
            Log.w(TAG, "Association not confirmed within " + timeoutMs
                    + " ms: SSID='" + normalizeSsid(last.getSSID())
                    + "' state=" + last.getSupplicantState()
                    + " netId=" + last.getNetworkId());
        }
        return false;
    }

    /**
     * Return true only when the supplicant reports the target SSID as
     * fully connected. On KitKat, WifiInfo.getSSID() returns the
     * "preferred" SSID as soon as enableNetwork() is called, before any
     * actual association. SupplicantState.COMPLETED together with a
     * non-negative network id is the reliable signal.
     */
    @SuppressWarnings("deprecation")
    private static boolean isFullyAssociated(WifiManager wifiManager, String targetSsid) {
        WifiInfo info = wifiManager.getConnectionInfo();
        if (info == null) {
            return false;
        }
        String current = normalizeSsid(info.getSSID());
        String bssid = info.getBSSID();
        // KitKat sets SupplicantState to COMPLETED and returns the target
        // netId as soon as enableNetwork() is called, before the supplicant
        // has actually associated. Requiring a valid BSSID filters out that
        // "optimistic" state: a real association always has a non-empty,
        // non-zero BSSID.
        return targetSsid.equals(current)
                && info.getSupplicantState() == SupplicantState.COMPLETED
                && info.getNetworkId() != -1
                && bssid != null
                && !bssid.isEmpty()
                && !"00:00:00:00:00:00".equals(bssid);
    }

    // ═══════════════════════════════════════════════════════════════════
    // Preferred SSID
    // ═══════════════════════════════════════════════════════════════════

    /**
     * Connect to the SSID the relay asked for.
     *
     * <p>Two paths depending on the API level:
     * <ul>
     *   <li><b>API 29 and above:</b> {@code WifiNetworkSuggestion}. The app
     *       hands the SSID to the framework and waits for the framework to
     *       associate. The app cannot force the switch.</li>
     *   <li><b>API 19-28:</b> {@code addNetwork()} + {@code enableNetwork()}.
     *       The app creates and activates the config itself.</li>
     * </ul>
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

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            return tryConnectViaSuggestion(wifiManager, preferredSsid);
        }

        if (!tryConnectBySsid(context, wifiManager, preferredSsid)) {
            Log.w(TAG, "Cannot add/connect network '" + preferredSsid + "'");
            return false;
        }

        return waitForTransport(PREFERRED_SSID_ASSOC_TIMEOUT_MS, wifiManager, preferredSsid);
    }

    /**
     * API 29+ path: register the preferred SSID as a network suggestion and
     * wait for the framework to associate with it.
     *
     * <p>The suggestion is registered with {@code setIsAppInteractionRequired(false)}
     * and {@code setIsUserInteractionRequired(false)}. On the first run per
     * package the system still shows a one-time approval dialog ("Allow
     * suggested Wi-Fi networks?"); once accepted, subsequent runs are
     * silent. The dialog is a platform requirement and cannot be bypassed
     * from inside the app.
     */
    private boolean tryConnectViaSuggestion(WifiManager wifiManager, String ssid) {
        // Remove any suggestion left behind by a previous run so the framework
        // does not keep trying to associate with a stale SSID, and so that
        // addNetworkSuggestions does not report ERROR_ADD_DUPLICATE.
        removeAllSuggestions(context);

        WifiNetworkSuggestion suggestion = new WifiNetworkSuggestion.Builder()
                .setSsid(ssid)
                .setIsAppInteractionRequired(false)
                .setIsUserInteractionRequired(false)
                .build();

        int result = wifiManager.addNetworkSuggestions(
                Collections.singletonList(suggestion));
        Log.d(TAG, "addNetworkSuggestions() returned " + describeSuggestionResult(result));

        if (result != WifiManager.STATUS_NETWORK_SUGGESTIONS_SUCCESS) {
            Log.w(TAG, "Suggestion was rejected; cannot use the API 29+ path");
            return false;
        }

        return waitForSuggestionTransport(SUGGESTION_ASSOC_TIMEOUT_MS, wifiManager, ssid);
    }

    /**
     * Wait until the framework associates with the suggested SSID.
     *
     * <p>Unlike {@link #waitForTransport}, this loop cannot re-issue
     * {@code enableNetwork()} because the app does not control a network id.
     * Instead it calls {@code startScan()} periodically as a hint; the
     * framework will still decide on its own schedule.
     */
    @SuppressWarnings("deprecation")
    private boolean waitForSuggestionTransport(
            long timeoutMs, WifiManager wifiManager, String targetSsid) {
        long deadline = System.currentTimeMillis() + timeoutMs;
        long lastLog = 0;
        long lastScan = 0;
        boolean associated = false;

        while (System.currentTimeMillis() < deadline) {
            String current = getConnectedSsid(context);

            if (!associated) {
                    if (targetSsid.equals(current)
                            && isFullyAssociated(wifiManager, targetSsid)) {
                        associated = true;
                    deadline = System.currentTimeMillis() + SSID_DHCP_TIMEOUT_MS;
                    Log.d(TAG, "Associated with '" + targetSsid + "'; waiting for framework");
                } else {
                    long now = System.currentTimeMillis();
                    if (now - lastScan > SUGGESTION_SCAN_INTERVAL_MS) {
                        try {
                            wifiManager.startScan();
                            Log.d(TAG, "startScan() issued while waiting for suggestion");
                        } catch (SecurityException e) {
                            Log.w(TAG, "startScan rejected: " + e.getMessage());
                        }
                        lastScan = now;
                    }
                    if (now - lastLog > 5000) {
                        Log.d(TAG, "Waiting for suggested '" + targetSsid
                                + "'; current SSID='" + current + "'");
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

                // Once associated, decide whether the transport is usable.
                if (associated) {
                    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.LOLLIPOP) {
                        // On KitKat, getActiveNetworkInfo() is unreliable
                        // while a VPN is active: the VPN obscures the
                        // underlying WiFi transport, and the framework
                        // transiently reports no active network during
                        // network transitions. The WiFi association
                        // itself (SupplicantState.COMPLETED with a valid
                        // netId) is the reliable signal.
                        return true;
                    }
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

        String current = getConnectedSsid(context);
        if (!targetSsid.equals(current)) {
            Log.w(TAG, "Timed out: suggestion did not lead to association with '"
                    + targetSsid + "' (current SSID='" + current + "')");
            return false;
        }
        if (!hasActiveTransport()) {
            Log.w(TAG, "Timed out: associated but framework never reported connected");
            return false;
        }
        return true;
    }

    /**
     * Remove every network suggestion currently registered by this package.
     *
     * <p>Suggestions persist across reboots and across app restarts, so the
     * app must explicitly clear them when the tunnel stops. No-op below
     * API 29.
     */
    public static void removeAllSuggestions(Context context) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.Q) {
            return;
        }
        WifiManager wifiManager = (WifiManager)
                context.getApplicationContext().getSystemService(Context.WIFI_SERVICE);
        if (wifiManager == null) {
            return;
        }
        try {
            List<WifiNetworkSuggestion> existing = wifiManager.getNetworkSuggestions();
            if (existing == null || existing.isEmpty()) {
                return;
            }
            int result = wifiManager.removeNetworkSuggestions(existing);
            Log.d(TAG, "Removed " + existing.size() + " suggestion(s): "
                    + describeSuggestionResult(result));
        } catch (SecurityException e) {
            Log.w(TAG, "removeNetworkSuggestions failed", e);
        }
    }

    private static String describeSuggestionResult(int result) {
        switch (result) {
            case WifiManager.STATUS_NETWORK_SUGGESTIONS_SUCCESS:
                return "SUCCESS";
            case WifiManager.STATUS_NETWORK_SUGGESTIONS_ERROR_INTERNAL:
                return "ERROR_INTERNAL";
            case WifiManager.STATUS_NETWORK_SUGGESTIONS_ERROR_APP_DISALLOWED:
                return "ERROR_APP_DISALLOWED";
            case WifiManager.STATUS_NETWORK_SUGGESTIONS_ERROR_ADD_DUPLICATE:
                return "ERROR_ADD_DUPLICATE";
            case WifiManager.STATUS_NETWORK_SUGGESTIONS_ERROR_ADD_EXCEEDS_MAX_PER_APP:
                return "ERROR_ADD_EXCEEDS_MAX_PER_APP";
            case WifiManager.STATUS_NETWORK_SUGGESTIONS_ERROR_ADD_NOT_ALLOWED:
                return "ERROR_ADD_NOT_ALLOWED";
            case WifiManager.STATUS_NETWORK_SUGGESTIONS_ERROR_REMOVE_INVALID:
                return "ERROR_REMOVE_INVALID";
            default:
                return "UNKNOWN(" + result + ")";
        }
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
                    if (tryConnectBySsid(context, wifiManager, result.SSID)
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
            if (tryConnectBySsid(context, wifiManager, bestSavedSecure.SSID)
                    && waitForTransport(OPEN_AP_ASSOC_TIMEOUT_MS, wifiManager, bestSavedSecure.SSID)) {
                return true;
            }
        }

        // ── Tier 3: strongest open network ──
        ScanResult bestOpen = pickStrongest(results, saved, false);
        if (bestOpen != null) {
            Log.d(TAG, "Trying strongest open network '"
                    + bestOpen.SSID + "' (signal " + bestOpen.level + ")");
            if (tryConnectBySsid(context, wifiManager, bestOpen.SSID)
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
     * Connect to the given SSID using the legacy API 19-28 path.
     *
     * <p>Before issuing enableNetwork(netId, true), record the network ids
     * that are currently ENABLED. KitKat's disableOthers flag permanently
     * marks the other configurations as DISABLED and does not restore them
     * when the target network is later dropped, so the restore path must
     * re-enable them explicitly.
     */
    @SuppressWarnings("deprecation")
    private static boolean tryConnectBySsid(Context context, WifiManager wifiManager, String ssid) {
        if (Build.VERSION.SDK_INT == Build.VERSION_CODES.KITKAT) {
            rememberEnabledNetworks(context, wifiManager);
        }

        int netId;
        if (ssid.startsWith(GNIREHTET_SSID_PREFIX)) {
            // Dummy AP: recreate the config as an open network. This is
            // safe because the relay's AP is genuinely open.
            netId = findOrAddOpenNetwork(wifiManager, ssid);
        } else {
            // User network: locate the existing config and reuse it,
            // preserving its PSK and key management.
            netId = findExistingNetId(wifiManager, ssid);
            if (netId == -1) {
                Log.w(TAG, "No existing config for '" + ssid + "'; refusing to "
                        + "create an open replacement that would drop the PSK");
                return false;
            }
        }

        if (netId == -1) {
            return false;
        }
        try {
            return wifiManager.enableNetwork(netId, true);
        } catch (SecurityException e) {
            return false;
        }
    }

    /**
     * Return the network id of the existing config for {@code ssid}, or -1
     * if there is none. Does not modify or recreate the config.
     */
    @SuppressWarnings("deprecation")
    private static int findExistingNetId(WifiManager wifiManager, String ssid) {
        try {
            List<WifiConfiguration> configured = wifiManager.getConfiguredNetworks();
            if (configured != null) {
                for (WifiConfiguration config : configured) {
                    if (ssid.equals(normalizeSsid(config.SSID))) {
                        return config.networkId;
                    }
                }
            }
        } catch (SecurityException e) {
            Log.w(TAG, "Cannot list configured networks", e);
        }
        return -1;
    }

    /**
     * Record the ids of all currently-enabled WiFi configurations in
     * SharedPreferences. Called before a disableOthers=true switch on
     * KitKat, so restorePreviousNetwork can re-enable them afterwards.
     */
    @SuppressWarnings("deprecation")
    private static void rememberEnabledNetworks(Context context, WifiManager wifiManager) {
        SharedPreferences prefs = context.getApplicationContext()
                .getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE);
        // Only save on the FIRST call. Subsequent calls come from
        // waitForTransport retries, whose state has already been mutated by
        // the earlier disableOthers=true. Saving here would overwrite the
        // good list with one that contains only the target network.
        if (prefs.contains(KEY_ENABLED_NETWORK_IDS)) {
            Log.d(TAG, "Enabled network ids already saved; not overwriting");
            return;
        }

        List<WifiConfiguration> configured;
        try {
            configured = wifiManager.getConfiguredNetworks();
        // ...resto sin cambios...
        } catch (SecurityException e) {
            Log.w(TAG, "Cannot list configured networks to remember", e);
            return;
        }
        if (configured == null) {
            Log.d(TAG, "rememberEnabledNetworks: no configured networks");
            return;
        }
        StringBuilder ids = new StringBuilder();
        int count = 0;
        for (WifiConfiguration config : configured) {
            if (config.status == WifiConfiguration.Status.ENABLED
                    || config.status == WifiConfiguration.Status.CURRENT) {
                if (count > 0) {
                    ids.append(",");
                }
                ids.append(config.networkId);
                count++;
            }
        }
        context.getApplicationContext()
                .getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
                .edit()
                .putString(KEY_ENABLED_NETWORK_IDS, ids.toString())
                .apply();
        Log.d(TAG, "Remembered " + count + " enabled network id(s): " + ids);
    }

    /**
     * Return the network id of an existing open-network config for {@code ssid},
     * creating one if needed.
     *
     * <p>This method is DESTRUCTIVE for the target config: it removes any
     * existing entry and recreates it as an open (no-key) network. It must
     * only be used for the relay's own dummy AP (whose SSID always starts
     * with {@code gnirehtet-} and is genuinely open). Applying it to a
     * user-configured WPA network would delete the stored PSK.
     *
     * <p>For non-dummy SSIDs, {@link #tryConnectBySsid} locates the existing
     * config and calls {@code enableNetwork} directly, without touching the
     * key material.
     */
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

    private boolean isConnectedToSsid(String ssid) {
        return ssid.equals(getConnectedSsid(context));
    }

    /**
     * Return the SSID of the Wi-Fi network the device is currently
     * associated with, or null if none.
     *
     * <p>On API 29+, the framework may report an association through
     * ConnectivityManager while WifiManager.getConnectionInfo() still says
     * "<unknown ssid>" — in particular when a VPN is active and has claimed
     * a suggested Wi-Fi network as its underlying transport. The suggested
     * network is present in ConnectivityManager.getAllNetworks() with its
     * SSID exposed through WifiInfo, which is the source we trust on
     * modern platforms.
     *
     * <p>Below API 29 the legacy WifiManager.getConnectionInfo() path is
     * used, since it is the only one available and is known to work there.
     */
    @SuppressWarnings("deprecation")
    private static String getConnectedSsid(Context context) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            ConnectivityManager cm = (ConnectivityManager)
                    context.getSystemService(Context.CONNECTIVITY_SERVICE);
            if (cm != null) {
                Network[] networks = cm.getAllNetworks();
                if (networks != null) {
                    for (Network network : networks) {
                        NetworkCapabilities caps = cm.getNetworkCapabilities(network);
                        if (caps == null) {
                            continue;
                        }
                        if (!caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)) {
                            continue;
                        }
                        // Skip the VPN, which can carry a synthetic WiFi
                        // transport alongside TRANSPORT_VPN.
                        if (caps.hasTransport(NetworkCapabilities.TRANSPORT_VPN)) {
                            continue;
                        }
                        android.net.TransportInfo ti = caps.getTransportInfo();
                        if (ti instanceof android.net.wifi.WifiInfo) {
                            String ssid = normalizeSsid(
                                    ((android.net.wifi.WifiInfo) ti).getSSID());
                            if (ssid != null
                                    && !ssid.isEmpty()
                                    && !"<unknown ssid>".equals(ssid)) {
                                return ssid;
                            }
                        }
                    }
                }
            }
            // Fall through to WifiManager as a secondary source. It is
            // known to be masked when a VPN is active, but there is no
            // harm in trying.
        }

        WifiManager wifiManager = (WifiManager)
                context.getSystemService(Context.WIFI_SERVICE);
        if (wifiManager == null) {
            return null;
        }
        return normalizeSsid(wifiManager.getConnectionInfo().getSSID());
    }

    private static String normalizeSsid(String ssid) {
        if (ssid == null) {
            return null;
        }
        if (ssid.length() >= 2 && ssid.startsWith("\"") && ssid.endsWith("\"")) {
            ssid = ssid.substring(1, ssid.length() - 1);
        }
        // KitKat transiently returns "0x" or "<unknown ssid>" during
        // network transitions. Both are placeholders, not real SSIDs.
        if ("0x".equals(ssid) || "<unknown ssid>".equals(ssid)) {
            return null;
        }
        return ssid;
    }

    private void saveCurrentSsid() {
        String current = getConnectedSsid(context);
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
                String current = getConnectedSsid(context);
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
                            tryConnectBySsid(context, wifiManager, targetSsid);
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
                // Once associated, decide whether the transport is usable.
                if (associated) {
                    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.LOLLIPOP) {
                        // On KitKat, getActiveNetworkInfo() is unreliable
                        // while a VPN is active: the VPN obscures the
                        // underlying WiFi transport, and the framework
                        // transiently reports no active network during
                        // network transitions. The WiFi association
                        // itself (SupplicantState.COMPLETED with a valid
                        // netId) is the reliable signal.
                        return true;
                    }
                    if (hasActiveTransport()) {
                        return true;
                    }
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
            String current = getConnectedSsid(context);
            if (!targetSsid.equals(current)
                    || !isFullyAssociated(wifiManager, targetSsid)) {
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
