package com.genymobile.gnirehtet;

/**
 * JNI bridge for VPN file-descriptor manipulation.
 *
 * <p>On API 21+, {@code VpnService.Builder.setBlocking(true)} requests a
 * blocking fd from the system. On API 19–20 that method does not exist and
 * the fd returned by {@code establish()} is non-blocking: a {@code read()}
 * on it returns -1 immediately with {@code errno=EAGAIN} if no data is
 * available.
 *
 * <p>The standard workaround, used by projects such as ics-openvpn, is to
 * call {@code fcntl(fd, F_SETFL, flags & ~O_NONBLOCK)} directly on the raw
 * fd. Java has no API for this, so we drop to JNI.
 *
 * <p>Usage:
 * <pre>{@code
 * ParcelFileDescriptor pfd = builder.establish();
 * if (VersionCompat.isLegacy()) {
 *     VpnUtils.setBlockingMode(pfd.getFd());
 * }
 * }</pre>
 *
 * <p>On API 21+ this class is never called, but the native library is still
 * loaded because {@code System.loadLibrary} runs at class-load time. The
 * library is tiny (a single C function) and its presence is harmless.
 */
public final class VpnUtils {

    static {
        System.loadLibrary("gnirehtet-jni");
    }

    private VpnUtils() {
    }

    /**
     * Sets the given file descriptor to blocking mode.
     *
     * <p>This is a direct wrapper around:
     * <pre>{@code
     * int flags = fcntl(fd, F_GETFL, 0);
     * fcntl(fd, F_SETFL, flags & ~O_NONBLOCK);
     * }</pre>
     *
     * <p>Failures are logged via {@code __android_log_print} and do not
     * throw. A failed call leaves the fd in its previous mode (non-blocking),
     * which callers should treat as a degraded state.
     *
     * @param fd raw file descriptor from {@code ParcelFileDescriptor.getFd()}
     */
    public static native void setBlockingMode(int fd);
}
