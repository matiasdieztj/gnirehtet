package com.genymobile.gnirehtet;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.content.Context;
import android.net.ConnectivityManager;
import android.net.Network;
import android.net.NetworkInfo;
import android.net.VpnService;
import android.os.Build;

/**
 * Centralized compatibility layer for API-gated calls.
 *
 * <p>Rationale: scattering {@code if (Build.VERSION.SDK_INT >= X)} checks
 * across the codebase makes them impossible to audit and easy to miss when
 * adding new code. All version checks live here, in one file, so that:
 *
 * <ul>
 *   <li>There is exactly one place to review when Android's SDK changes.</li>
 *   <li>Callers express intent ("set blocking if supported") instead of
 *       mechanism ("compare SDK_INT to LOLLIPOP").</li>
 *   <li>The legacy flavor's behavior is documented in one place.</li>
 * </ul>
 *
 * <p>Naming convention: methods ending in {@code IfSupported} are no-ops on
 * platforms that lack the API. Callers do not need to branch.
 *
 * <p>See {@code docs/legacy-api-gates.md} for the full table of what is
 * gated and why.
 */
public final class VersionCompat {

    private VersionCompat() {
    }

    // ------------------------------------------------------------------------
    // Version predicates
    // ------------------------------------------------------------------------

    /** True on Android 4.4 (API 19) through 5.0 (API 20). */
    public static boolean isLegacy() {
        return Build.VERSION.SDK_INT < Build.VERSION_CODES.LOLLIPOP;
    }

    /** API 21+: VpnService.Builder.setBlocking() exists. */
    public static boolean hasBlockingMode() {
        return Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP;
    }

    /** API 22+: VpnService.setUnderlyingNetworks() exists. */
    public static boolean hasUnderlyingNetworks() {
        return Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP_MR1;
    }

    /** API 23+: ConnectivityManager.getActiveNetwork() exists. */
    public static boolean hasActiveNetwork() {
        return Build.VERSION.SDK_INT >= Build.VERSION_CODES.M;
    }

    /** API 26+: NotificationChannel exists. */
    public static boolean hasNotificationChannels() {
        return Build.VERSION.SDK_INT >= Build.VERSION_CODES.O;
    }

    /** API 29+: VpnService.Builder.setMetered() exists. */
    public static boolean hasMeteredSetter() {
        return Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q;
    }

    // ------------------------------------------------------------------------
    // VpnService.Builder gates
    // ------------------------------------------------------------------------

    /**
     * Calls {@code builder.setBlocking(blocking)} on API 21+, no-op below.
     *
     * <p>On API 19–20, the fd returned by {@code establish()} is non-blocking.
     * The caller must invoke {@link VpnUtils#setBlockingMode(int)} with the
     * raw fd after establish() to obtain blocking semantics. See
     * {@code GnirehtetService.setupVpn()}.
     */
    public static void setBlockingIfSupported(VpnService.Builder builder, boolean blocking) {
        if (hasBlockingMode()) {
            builder.setBlocking(blocking);
        }
        // pre-21: caller must call VpnUtils.setBlockingMode(fd) after establish().
    }

    /**
     * Calls {@code builder.setMetered(metered)} on API 29+, no-op below.
     *
     * <p>On pre-29 there is no concept of a "metered" VPN, so the parameter
     * is meaningless and correctly ignored.
     */
    public static void setMeteredIfSupported(VpnService.Builder builder, boolean metered) {
        if (hasMeteredSetter()) {
            builder.setMetered(metered);
        }
    }

    // ------------------------------------------------------------------------
    // Notifications
    // ------------------------------------------------------------------------

    /**
     * Creates a NotificationChannel on API 26+, no-op below.
     *
     * <p>Pre-26 Android has no channels; the channel id passed to the
     * notification builder is simply ignored by the system.
     */
    public static void createNotificationChannelIfSupported(Context context, String channelId, String channelName) {
        if (!hasNotificationChannels()) {
            return;
        }
        NotificationManager nm = context.getSystemService(NotificationManager.class);
        if (nm != null) {
            NotificationChannel channel = new NotificationChannel(
                    channelId, channelName, NotificationManager.IMPORTANCE_LOW);
            nm.createNotificationChannel(channel);
        }
    }

    /**
     * Returns a Notification.Builder appropriate for the running API level.
     *
     * <p>On API 26+, uses the channel-aware constructor. Below, uses the
     * deprecated constructor (which still works and is the only option).
     */
    public static Notification.Builder newNotificationBuilder(Context context, String channelId) {
        if (hasNotificationChannels()) {
            return new Notification.Builder(context, channelId);
        }
        @SuppressWarnings("deprecation")
        Notification.Builder legacyBuilder = new Notification.Builder(context);
        return legacyBuilder;
    }

    // ------------------------------------------------------------------------
    // ConnectivityManager
    // ------------------------------------------------------------------------

    /**
     * Returns the active network on API 23+, or null below.
     *
     * <p>Callers that need connectivity information on API 19–22 must fall
     * back to {@link #getActiveNetworkInfo(ConnectivityManager)}.
     */
    public static Network getActiveNetworkOrNull(ConnectivityManager cm) {
        if (hasActiveNetwork()) {
            return cm.getActiveNetwork();
        }
        return null;
    }

    /**
     * Legacy connectivity query. Deprecated since API 29 but functional on
     * every API level from 1 through 36. Safe to call unconditionally.
     */
    @SuppressWarnings("deprecation")
    public static NetworkInfo getActiveNetworkInfo(ConnectivityManager cm) {
        return cm.getActiveNetworkInfo();
    }
}
