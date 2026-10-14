package com.genymobile.gnirehtet;

import android.Manifest;
import android.app.Activity;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.net.VpnService;
import android.os.Build;
import android.os.Bundle;
import android.util.Log;

public class GnirehtetActivity extends Activity {

    private static final String TAG = GnirehtetActivity.class.getSimpleName();

    public static final String ACTION_GNIREHTET_START = "com.genymobile.gnirehtet.START";
    public static final String ACTION_GNIREHTET_STOP = "com.genymobile.gnirehtet.STOP";

    public static final String EXTRA_DNS_SERVERS = "dnsServers";
    public static final String EXTRA_ROUTES = "routes";
    public static final String EXTRA_PROXY_HOST_PORT = "proxyHostPort";
    public static final String EXTRA_PROXY_EXCLUSION_LIST = "proxyExclusionList";
    public static final String EXTRA_MTU = "mtu";
    public static final String EXTRA_ALLOW_APPS = "allowApps";
    public static final String EXTRA_DENY_APPS = "denyApps";

    /**
     * SSID of the dummy Wi-Fi hotspot the relay host is broadcasting. Passed
     * as a single String extra (not an array) so it works on API 19, where
     * `am start --esa` is not supported. When present, the client's
     * connectivity fallback prefers this exact SSID over any other
     * {@code gnirehtet-*} network in range.
     */
    public static final String EXTRA_DUMMY_WIFI_SSID = "dummyWifiSsid";

    private static final int VPN_REQUEST_CODE = 0;
    private static final int NOTIFICATION_PERMISSION_CODE = 1;
    private static final int LOCATION_PERMISSION_CODE = 2;

    private VpnConfiguration requestedConfig;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        handleIntent(getIntent());
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        Log.d(TAG, "Received request (onNewIntent) " + intent.getAction());
        handleIntent(intent);
    }

    private void handleIntent(Intent intent) {
        String action = intent.getAction();
        Log.d(TAG, "Received request " + action);
        boolean finish = true;
        if (ACTION_GNIREHTET_START.equals(action)) {
            VpnConfiguration config = createConfig(intent);
            finish = startGnirehtet(config);
        } else if (ACTION_GNIREHTET_STOP.equals(action)) {
            stopGnirehtet();
        }

        if (finish) {
            finish();
        }
    }

    private static VpnConfiguration createConfig(Intent intent) {
        String[] dnsServers = intent.getStringArrayExtra(EXTRA_DNS_SERVERS);
        if (dnsServers == null) {
            dnsServers = new String[0];
        }
        String[] routes = intent.getStringArrayExtra(EXTRA_ROUTES);
        if (routes == null) {
            routes = new String[0];
        }
        String proxyHostPort = intent.getStringExtra(EXTRA_PROXY_HOST_PORT);
        String[] proxyExclusionList = intent.getStringArrayExtra(EXTRA_PROXY_EXCLUSION_LIST);
        if (proxyExclusionList == null) {
            proxyExclusionList = new String[0];
        }
        int mtu = intent.getIntExtra(EXTRA_MTU, 0x4000);
        String[] allowApps = intent.getStringArrayExtra(EXTRA_ALLOW_APPS);
        if (allowApps == null) {
            allowApps = new String[0];
        }
        String[] denyApps = intent.getStringArrayExtra(EXTRA_DENY_APPS);
        if (denyApps == null) {
            denyApps = new String[0];
        }
        // getStringExtra returns null when the extra is absent, which is the
        // correct "no preference" value for the connectivity fallback.
        String dummyWifiSsid = intent.getStringExtra(EXTRA_DUMMY_WIFI_SSID);
        return new VpnConfiguration(
                Net.toInetAddresses(dnsServers),
                Net.toCIDRs(routes),
                proxyHostPort,
                proxyExclusionList,
                mtu,
                allowApps,
                denyApps,
                dummyWifiSsid);
    }

    private boolean startGnirehtet(VpnConfiguration config) {
    // Request notification permission on Android 13+ before starting the VPN
    if (Build.VERSION.SDK_INT >= 33) {
        if (checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS)
                != PackageManager.PERMISSION_GRANTED) {
            this.requestedConfig = config;
            requestPermissions(new String[]{Manifest.permission.POST_NOTIFICATIONS},
                    NOTIFICATION_PERMISSION_CODE);
            return false;
        }
    }

    // Location permission is required on API 23+ for the connectivity
    // fallback to work at all:
    //   - WifiManager.getConfiguredNetworks() and addNetwork() throw
    //     SecurityException without it (kills Fallback 1 and Fallback 4).
    //   - WifiManager.getScanResults() returns an empty list (kills Fallback 4).
    //   - WifiManager.startLocalOnlyHotspot() throws (kills Fallback 3).
    // Request it before attempting VpnService.prepare().
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.M) {
        if (checkSelfPermission(Manifest.permission.ACCESS_FINE_LOCATION)
                != PackageManager.PERMISSION_GRANTED) {
            this.requestedConfig = config;
            requestPermissions(new String[]{Manifest.permission.ACCESS_FINE_LOCATION},
                    LOCATION_PERMISSION_CODE);
            return false;
        }
    }

    Intent vpnIntent = VpnService.prepare(this);
        if (vpnIntent == null) {
            GnirehtetService.start(this, config);
            return true;
        }

        Log.w(TAG, "VPN requires the authorization from the user, requesting...");
        requestAuthorization(vpnIntent, config);
        return false;
    }

    @Override
    public void onRequestPermissionsResult(int requestCode, String[] permissions, int[] grantResults) {
        if (requestCode == NOTIFICATION_PERMISSION_CODE
                || requestCode == LOCATION_PERMISSION_CODE) {
            boolean granted = grantResults.length > 0
                    && grantResults[0] == PackageManager.PERMISSION_GRANTED;
            Log.d(TAG, "Permission result for code " + requestCode + ": granted=" + granted);
            if (requestedConfig != null) {
            VpnConfiguration config = requestedConfig;
            requestedConfig = null;
            // Re-enter startGnirehtet: it will re-check all permissions and
            // chain to the next one, or proceed to VpnService.prepare() if
            // everything is now granted.
            startGnirehtet(config);
            }
            finish();
        }
    }

    private void stopGnirehtet() {
        GnirehtetService.stop(this);
    }

    private void requestAuthorization(Intent vpnIntent, VpnConfiguration config) {
        this.requestedConfig = config;
        startActivityForResult(vpnIntent, VPN_REQUEST_CODE);
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode == VPN_REQUEST_CODE && resultCode == RESULT_OK) {
            GnirehtetService.start(this, requestedConfig);
        }
        requestedConfig = null;
        finish();
    }
}
