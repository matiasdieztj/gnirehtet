/*
 * Copyright (C) 2017 Genymobile
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
import android.content.Intent;
import android.content.pm.PackageManager.NameNotFoundException;
import android.net.ConnectivityManager;
import android.net.LinkAddress;
import android.net.LinkProperties;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.VpnService;
import android.os.Build;
import android.os.Handler;
import android.os.Message;
import android.os.ParcelFileDescriptor;
import android.util.Log;

import java.io.IOException;
import java.net.InetAddress;
import java.util.Arrays;
import java.util.List;

public class GnirehtetService extends VpnService {

    public static final boolean VERBOSE = false;

    private static final String ACTION_START_VPN = "com.genymobile.gnirehtet.START_VPN";
    private static final String ACTION_CLOSE_VPN = "com.genymobile.gnirehtet.CLOSE_VPN";
    private static final String EXTRA_VPN_CONFIGURATION = "vpnConfiguration";

    private static final String TAG = GnirehtetService.class.getSimpleName();

    private static final InetAddress VPN_ADDRESS = Net.toInetAddress(new byte[] {10, 0, 0, 2});

    private final Notifier notifier = new Notifier(this);
    private final Handler handler = new RelayTunnelConnectionStateHandler(this);

    private ParcelFileDescriptor vpnInterface = null;
    private Forwarder forwarder;

    public static void start(Context context, VpnConfiguration config) {
        Intent intent = new Intent(context, GnirehtetService.class);
        intent.setAction(ACTION_START_VPN);
        intent.putExtra(GnirehtetService.EXTRA_VPN_CONFIGURATION, config);
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            context.startForegroundService(intent);
        } else {
            context.startService(intent);
        }
    }

    public static void stop(Context context) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            context.startForegroundService(createStopIntent(context));
        } else {
            context.startService(createStopIntent(context));
        }
    }

    static Intent createStopIntent(Context context) {
        Intent intent = new Intent(context, GnirehtetService.class);
        intent.setAction(ACTION_CLOSE_VPN);
        return intent;
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        String action = intent.getAction();
        Log.d(TAG, "Received request " + action);
        if (ACTION_START_VPN.equals(action)) {
            if (isRunning()) {
                // If the framework dropped our VPN (e.g. the fd was closed
                // after an idle period on KitKat, or a previous tunnel
                // failure left things in a bad state), the stale vpnInterface
                // makes isRunning() lie. Re-establish unconditionally: it is
                // safe because close() nulls the field and setupVpn() will
                // create a fresh ParcelFileDescriptor.
                Log.d(TAG, "VPN already running, restarting to ensure fresh state");
                close();
            }
            VpnConfiguration config = intent.getParcelableExtra(EXTRA_VPN_CONFIGURATION);
            if (config == null) {
                config = new VpnConfiguration();
            }
            startVpn(config);
        } else if (ACTION_CLOSE_VPN.equals(action)) {
            close();
        }
        return START_NOT_STICKY;
    }

    private boolean isRunning() {
        return vpnInterface != null;
    }

    private void startVpn(VpnConfiguration config) {
        notifier.start();
        if (setupVpn(config)) {
            startForwarding();
        }
    }

    @SuppressWarnings("checkstyle:MagicNumber")
    private boolean setupVpn(VpnConfiguration config) {
        Builder builder = new Builder();
        builder.addAddress(VPN_ADDRESS, 32);
        builder.setSession(getString(R.string.app_name));

        CIDR[] routes = config.getRoutes();
        if (routes.length == 0) {
            // no routes defined, redirect the whole network traffic
            builder.addRoute("0.0.0.0", 0);
            // IPv6 routes require API 21+. On API 19-20, only IPv4 is routed
            // through the tunnel — IPv6 traffic will fall through to the
            // underlying network (or be dropped if none). This is acceptable
            // because gnirehtet's relay is primarily IPv4-focused, and the
            // original gnirehtet never supported API 19 at all.
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP) {
                builder.addRoute("::0", 0);
            }
        } else {
            for (CIDR route : routes) {
                builder.addRoute(route.getAddress(), route.getPrefixLength());
            }
        }

        InetAddress[] dnsServers = config.getDnsServers();
        if (dnsServers.length == 0) {
            // no DNS server defined, use Google DNS
            builder.addDnsServer("8.8.8.8");
        } else {
            for (InetAddress dnsServer : dnsServers) {
                // addDnsServer(InetAddress) requires API 21+.
                // On API 19-20, use the String overload which is available
                // since API 14.
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP) {
                    builder.addDnsServer(dnsServer);
                } else {
                    builder.addDnsServer(dnsServer.getHostAddress());
                }
            }
        }

        // non-blocking by default, but FileChannel is not selectable, that's stupid!
        // so switch to synchronous I/O to avoid polling.
        //
        // setBlocking() requires API 21+. On API 19-20 the fd returned by
        // establish() is forced to blocking mode via JNI, right after
        // establish() returns. See the VpnUtils.setBlockingMode call below.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP) {
            builder.setBlocking(true);
        }

        // setMtu() requires API 21+. On API 19-20 the system uses a default
        // MTU which may differ from the relay's configured MTU. In practice
        // this has not caused issues for ICMP/TCP; UDP fragmentation may be
        // slightly less optimal.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP) {
            builder.setMtu(config.getMtu());
        }

        // Indicar al sistema que la VPN no tiene límite de datos (para descargas en Play Store y Galaxy Store)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            builder.setMetered(false);
        }

        if (Build.VERSION.SDK_INT >= 29) {
            String proxyHostPort = config.getProxyHostPort();
            if (proxyHostPort != null) {
                String[] parts = proxyHostPort.split(":");
                String host = parts[0];
                int port = parts.length > 1 ? Integer.parseInt(parts[1]) : 8080;
                android.net.ProxyInfo proxyInfo = android.net.ProxyInfo.buildDirectProxy(host, port, Arrays.asList(config.getProxyExclusionList()));
                builder.setHttpProxy(proxyInfo);
            }
        }

        // Per-app routing requires API 21+. On API 19-20 all apps go through
        // the VPN unconditionally. This is a functional limitation, not a bug:
        // the platform simply does not offer per-app routing at that API level.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.LOLLIPOP) {
            try {
                for (String pkg : config.getAllowApps()) {
                    builder.addAllowedApplication(pkg);
                }
                for (String pkg : config.getDenyApps()) {
                    builder.addDisallowedApplication(pkg);
                }
            } catch (NameNotFoundException e) {
                Log.w(TAG, "Package not found for per-app routing", e);
            }
        } else {
            Log.d(TAG, "Per-app routing not available on API " + Build.VERSION.SDK_INT
                    + " (requires API 21+); ignoring allow/deny lists");
        }

        vpnInterface = builder.establish();
        if (vpnInterface == null) {
            Log.w(TAG, "VPN starting failed, please retry");
            // establish() may return null if the application is not prepared or is revoked
            return false;
        }

        // On API 19-20, builder.setBlocking() does not exist, so the fd is
        // non-blocking by default. Force it to blocking mode via JNI so that
        // the Forwarder can use synchronous reads without polling.
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.LOLLIPOP) {
            try {
                VpnUtils.setBlockingMode(vpnInterface.getFd());
            } catch (Throwable t) {
                Log.e(TAG, "Failed to set VPN fd to blocking mode", t);
            }
        }

        setAsUndernlyingNetwork();
        return true;
    }

    @SuppressWarnings("checkstyle:MagicNumber")
    private void setAsUndernlyingNetwork() {
        if (Build.VERSION.SDK_INT < 22) {
            Log.w(TAG, "Cannot set underlying network, API version "
                + Build.VERSION.SDK_INT + " < 22");
            return;
        }
        Network physical = findPhysicalNetwork();
        if (physical != null) {
            setUnderlyingNetworks(new Network[]{physical});
        } else {
            setUnderlyingNetworks(null);   // "no underlying networks"
        }
    }

    private Network findPhysicalNetwork() {
        ConnectivityManager cm = (ConnectivityManager)
            getSystemService(Context.CONNECTIVITY_SERVICE);
        if (cm == null) {
            return null;
        }
        // getAllNetworks() requires API 21+. This method is only called from
        // setAsUndernlyingNetwork(), which is gated to API 22+, so the branch
        // is unreachable on pre-21. The explicit check here is for the
        // Dalvik verifier, which inspects the whole method regardless of the
        // caller's gate.
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.LOLLIPOP) {
            return null;
        }
        for (Network network : cm.getAllNetworks()) {
            NetworkCapabilities caps = cm.getNetworkCapabilities(network);
            if (caps == null) {
                continue;
            }
            if (caps.hasTransport(NetworkCapabilities.TRANSPORT_VPN)) {
                continue;
            }
            if (!caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)) {
                continue;
            }
            return network;
        }
        return null;
    }

    private void startForwarding() {
        forwarder = new Forwarder(this, vpnInterface.getFileDescriptor(), new RelayTunnelListener(handler));
        forwarder.forward();
    }

    private void close() {
        if (!isRunning()) {
            // already closed
            return;
        }

        notifier.stop();

        try {
            forwarder.stop();
            forwarder = null;
            vpnInterface.close();
            vpnInterface = null;
        } catch (IOException e) {
            Log.w(TAG, "Cannot close VPN file descriptor", e);
        }
    }

    private static final class RelayTunnelConnectionStateHandler extends Handler {

        private final GnirehtetService vpnService;

        private RelayTunnelConnectionStateHandler(GnirehtetService vpnService) {
            this.vpnService = vpnService;
        }

        @Override
        public void handleMessage(Message message) {
            if (!vpnService.isRunning()) {
                // if the VPN is not running anymore, ignore obsolete events
                return;
            }
            switch (message.what) {
                case RelayTunnelListener.MSG_RELAY_TUNNEL_CONNECTED:
                    Log.d(TAG, "Relay tunnel connected");
                    vpnService.notifier.setFailure(false);
                    break;
                case RelayTunnelListener.MSG_RELAY_TUNNEL_DISCONNECTED:
                    Log.d(TAG, "Relay tunnel disconnected");
                    vpnService.notifier.setFailure(true);
                    break;
                default:
            }
        }
    }
}
