package com.genymobile.gnirehtet;

import android.os.Parcel;
import android.os.Parcelable;

import java.net.InetAddress;
import java.net.UnknownHostException;

public class VpnConfiguration implements Parcelable {

    private final InetAddress[] dnsServers;
    private final CIDR[] routes;
    private final String proxyHostPort;
    private final String[] proxyExclusionList;
    private final int mtu;
    private final String[] allowApps;
    private final String[] denyApps;

    /**
     * SSID of the dummy Wi-Fi hotspot the relay host is broadcasting, if any.
     *
     * <p>When non-null, the connectivity fallback in the client prefers this
     * exact SSID over any other {@code gnirehtet-*} network in range. This is
     * what allows a device connected to a specific host (over USB) to attach
     * to that host's AP even when several hosts are broadcasting in the same
     * area.
     *
     * <p>{@code null} means "no preference": the fallback will pick any
     * {@code gnirehtet-*} network, then any open AP.
     */
    private final String dummyWifiSsid;

    public VpnConfiguration() {
        this.dnsServers = new InetAddress[0];
        this.routes = new CIDR[0];
        this.proxyHostPort = null;
        this.proxyExclusionList = new String[0];
        this.mtu = 0x4000;
        this.allowApps = new String[0];
        this.denyApps = new String[0];
        this.dummyWifiSsid = null;
    }

    public VpnConfiguration(InetAddress[] dnsServers, CIDR[] routes) {
        this(dnsServers, routes, null, new String[0], 0x4000, new String[0], new String[0], null);
    }

    public VpnConfiguration(InetAddress[] dnsServers, CIDR[] routes, String proxyHostPort, String[] proxyExclusionList) {
        this(dnsServers, routes, proxyHostPort, proxyExclusionList, 0x4000, new String[0], new String[0], null);
    }

    public VpnConfiguration(InetAddress[] dnsServers, CIDR[] routes, String proxyHostPort, String[] proxyExclusionList, int mtu, String[] allowApps, String[] denyApps) {
        this(dnsServers, routes, proxyHostPort, proxyExclusionList, mtu, allowApps, denyApps, null);
    }

    public VpnConfiguration(InetAddress[] dnsServers, CIDR[] routes, String proxyHostPort, String[] proxyExclusionList, int mtu, String[] allowApps, String[] denyApps, String dummyWifiSsid) {
        this.dnsServers = dnsServers;
        this.routes = routes;
        this.proxyHostPort = proxyHostPort;
        this.proxyExclusionList = proxyExclusionList;
        this.mtu = mtu;
        this.allowApps = allowApps;
        this.denyApps = denyApps;
        this.dummyWifiSsid = dummyWifiSsid;
    }

    private VpnConfiguration(Parcel source) {
        int dnsCount = source.readInt();
        dnsServers = new InetAddress[dnsCount];
        try {
            for (int i = 0; i < dnsCount; ++i) {
                dnsServers[i] = InetAddress.getByAddress(source.createByteArray());
            }
        } catch (UnknownHostException e) {
            throw new AssertionError("Invalid address", e);
        }
        routes = source.createTypedArray(CIDR.CREATOR);
        proxyHostPort = source.readString();
        proxyExclusionList = source.createStringArray();
        mtu = source.readInt();
        allowApps = source.createStringArray();
        denyApps = source.createStringArray();
        // readString() returns null when the parcel stored a null String,
        // so this round-trips the "no preference" value correctly.
        dummyWifiSsid = source.readString();
    }

    public InetAddress[] getDnsServers() {
        return dnsServers;
    }

    public CIDR[] getRoutes() {
        return routes;
    }

    public String getProxyHostPort() {
        return proxyHostPort;
    }

    public String[] getProxyExclusionList() {
        return proxyExclusionList;
    }

    public int getMtu() {
        return mtu;
    }

    public String[] getAllowApps() {
        return allowApps;
    }

    public String[] getDenyApps() {
        return denyApps;
    }

    /**
     * SSID the client should prefer when auto-connecting to the dummy Wi-Fi,
     * or {@code null} if no preference was provided by the relay.
     */
    public String getDummyWifiSsid() {
        return dummyWifiSsid;
    }

    @Override
    public void writeToParcel(Parcel dest, int flags) {
        dest.writeInt(dnsServers.length);
        for (InetAddress addr : dnsServers) {
            dest.writeByteArray(addr.getAddress());
        }
        dest.writeTypedArray(routes, 0);
        dest.writeString(proxyHostPort);
        dest.writeStringArray(proxyExclusionList);
        dest.writeInt(mtu);
        dest.writeStringArray(allowApps);
        dest.writeStringArray(denyApps);
        // writeString(null) is supported and round-trips through readString().
        dest.writeString(dummyWifiSsid);
    }

    @Override
    public int describeContents() {
        return 0;
    }

    public static final Creator<VpnConfiguration> CREATOR = new Creator<VpnConfiguration>() {
        @Override
        public VpnConfiguration createFromParcel(Parcel source) {
            return new VpnConfiguration(source);
        }

        @Override
        public VpnConfiguration[] newArray(int size) {
            return new VpnConfiguration[size];
        }
    };
}
