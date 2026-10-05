package com.genymobile.gnirehtet;

import android.annotation.TargetApi;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.os.Build;

/**
 * Manage the notification necessary for the foreground service (mandatory since Android O).
 *
 * <p>Compatibility notes for API 19 (Android 4.4):
 *
 * <ul>
 *   <li>{@code Notification.Action.Builder} was added in API 20. On API 19, the
 *       deprecated {@code Notification.Builder.addAction(int, CharSequence, PendingIntent)}
 *       overload is used instead.</li>
 *   <li>{@code NotificationChannel} and the channel-aware {@code Notification.Builder}
 *       constructor require API 26, and are gated.</li>
 *   <li>{@code PendingIntent.FLAG_IMMUTABLE} requires API 23, and is gated at API 31
 *       (the only API level where it matters for foreground services).</li>
 * </ul>
 */
public class Notifier {

    private static final int NOTIFICATION_ID = 42;
    private static final String CHANNEL_ID = "Gnirehtet";

    private final Service context;
    private boolean failure;

    public Notifier(Service context) {
        this.context = context;
    }

    private Notification createNotification(boolean failure) {
        Notification.Builder notificationBuilder = createNotificationBuilder();
        notificationBuilder.setContentTitle(context.getString(R.string.app_name));
        if (failure) {
            notificationBuilder.setContentText(context.getString(R.string.relay_disconnected));
            notificationBuilder.setSmallIcon(R.drawable.ic_report_problem_24dp);
        } else {
            notificationBuilder.setContentText(context.getString(R.string.relay_connected));
            notificationBuilder.setSmallIcon(R.drawable.ic_usb_24dp);
        }
        addStopAction(notificationBuilder);
        return notificationBuilder.build();
    }

    @SuppressWarnings("deprecation")
    private Notification.Builder createNotificationBuilder() {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            return new Notification.Builder(context, CHANNEL_ID);
        }
        return new Notification.Builder(context);
    }

    /**
     * Adds a "Stop VPN" action to the notification.
     *
     * <p>Two code paths:
     * <ul>
     *   <li>API 20+ ({@code KITKAT_WATCH}): {@code Notification.Action.Builder}</li>
     *   <li>API 19 ({@code KITKAT}): the deprecated
     *       {@code Notification.Builder.addAction(int, CharSequence, PendingIntent)}</li>
     * </ul>
     *
     * <p>The deprecated API-16 overload is functionally equivalent for the single
     * action we add here. The modern builder path is kept because it is the
     * recommended form on all current Android versions.
     */
    private void addStopAction(Notification.Builder notificationBuilder) {
        Intent stopIntent = GnirehtetService.createStopIntent(context);
        int flags = PendingIntent.FLAG_ONE_SHOT;
        if (Build.VERSION.SDK_INT >= 31) {
            flags |= PendingIntent.FLAG_IMMUTABLE;
        }
        PendingIntent stopPendingIntent = PendingIntent.getService(context, 0, stopIntent, flags);

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.KITKAT_WATCH) {
            // API 20+: Notification.Action.Builder is available.
            @SuppressWarnings("deprecation")
            Notification.Action.Builder actionBuilder = new Notification.Action.Builder(
                    R.drawable.ic_close_24dp,
                    context.getString(R.string.stop_vpn),
                    stopPendingIntent);
            notificationBuilder.addAction(actionBuilder.build());
        } else {
            // API 19: Action.Builder does not exist. Use the deprecated overload
            // that takes (icon, title, intent) directly on the builder.
            notificationBuilder.addAction(
                    R.drawable.ic_close_24dp,
                    context.getString(R.string.stop_vpn),
                    stopPendingIntent);
        }
    }

    @TargetApi(26)
    private void createNotificationChannel() {
        NotificationChannel channel = new NotificationChannel(CHANNEL_ID, context.getString(R.string.app_name), NotificationManager
                .IMPORTANCE_DEFAULT);
        getNotificationManager().createNotificationChannel(channel);
    }

    @TargetApi(26)
    private void deleteNotificationChannel() {
        getNotificationManager().deleteNotificationChannel(CHANNEL_ID);
    }

    public void start() {
        failure = false; // reset failure flag
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            createNotificationChannel();
        }
        context.startForeground(NOTIFICATION_ID, createNotification(false));
    }

    public void stop() {
        context.stopForeground(true);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            deleteNotificationChannel();
        }
    }

    public void setFailure(boolean failure) {
        if (this.failure != failure) {
            this.failure = failure;
            Notification notification = createNotification(failure);
            getNotificationManager().notify(NOTIFICATION_ID, notification);
        }
    }

    private NotificationManager getNotificationManager() {
        return (NotificationManager) context.getSystemService(Context.NOTIFICATION_SERVICE);
    }
}
