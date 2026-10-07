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

import android.net.LocalSocket;
import android.net.LocalSocketAddress;
import android.net.VpnService;
import android.os.Build;
import android.util.Log;

import java.io.Closeable;
import java.io.DataInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.SocketTimeoutException;

public final class RelayTunnel implements Tunnel {

    private static final String TAG = RelayTunnel.class.getSimpleName();

    private static final String LOCAL_ABSTRACT_NAME = "gnirehtet";
    private static final int FORWARD_PORT = 31416;
    private static final int FORWARD_ACCEPT_TIMEOUT_MS = 30_000;

    /**
     * Shared listener for forward mode, created once per process.
     *
     * MUST be static. On Android KitKat, ServerSocket.close() propagates
     * the close to sockets it has accepted, so we must NOT close the
     * listener between reconnects — otherwise the accepted socket dies
     * too, and the relay loops endlessly reconnecting every 2 ms.
     */
    private static ServerSocket sharedServerSocket;

    /**
     * Currently accepted socket in forward mode.
     *
     * MUST be static too. When the client re-establishes the VPN, a new
     * RelayTunnel instance is created, but the old accepted socket is still
     * held by the previous instance and remains connected to the relay. If
     * we didn't expose it as a static field, the new instance could not
     * close it, the relay would keep the old TCP connection alive, and the
     * new instance would block in accept() forever.
     */
    private static Socket sharedAcceptedSocket;

    private final boolean forwardMode;

    private LocalSocket localSocket;

    private RelayTunnel(boolean forwardMode) {
        this.forwardMode = forwardMode;
    }

    @SuppressWarnings("unused")
    public static RelayTunnel open(VpnService vpnService) throws IOException {
        boolean forward = Build.VERSION.SDK_INT < Build.VERSION_CODES.LOLLIPOP;
        Log.d(TAG, "Opening relay tunnel (" + (forward ? "forward" : "reverse") + " mode)");
        return new RelayTunnel(forward);
    }

    public void connect() throws IOException {
        if (forwardMode) {
            connectForward();
        } else {
            connectReverse();
        }
    }

    private void connectReverse() throws IOException {
        localSocket = new LocalSocket();
        localSocket.connect(new LocalSocketAddress(LOCAL_ABSTRACT_NAME));
        readClientId(localSocket.getInputStream());
    }

    private void connectForward() throws IOException {
        synchronized (RelayTunnel.class) {
            // Ensure the shared listener exists.
            if (sharedServerSocket == null || sharedServerSocket.isClosed()) {
                sharedServerSocket = new ServerSocket(FORWARD_PORT, 8, InetAddress.getByName("127.0.0.1"));
                Log.d(TAG, "Bound shared ServerSocket on 127.0.0.1:" + FORWARD_PORT);
            }

            // Close any stale accepted socket left over from a previous
            // RelayTunnel instance. This forces the relay to observe EOF on
            // the old TCP connection and reconnect with a fresh one, which
            // the accept() below will pick up.
            if (sharedAcceptedSocket != null) {
                Log.d(TAG, "Closing stale accepted socket from previous tunnel");
                closeQuietly(sharedAcceptedSocket);
                sharedAcceptedSocket = null;
            }

            sharedServerSocket.setSoTimeout(FORWARD_ACCEPT_TIMEOUT_MS);
            Log.d(TAG, "Waiting for relay on 127.0.0.1:" + FORWARD_PORT + "...");
            try {
                sharedAcceptedSocket = sharedServerSocket.accept();
            } catch (SocketTimeoutException e) {
                throw new IOException("Timed out waiting for relay to connect", e);
            }
            Log.d(TAG, "Relay connected from " + sharedAcceptedSocket.getRemoteSocketAddress());
            readClientId(sharedAcceptedSocket.getInputStream());
        }
    }

    @Override
    public void send(byte[] packet, int len) throws IOException {
        if (GnirehtetService.VERBOSE) {
            Log.v(TAG, "Sending packet: " + Binary.buildPacketString(packet, len));
        }
        outputStream().write(packet, 0, len);
    }

    @Override
    public int receive(byte[] packet) throws IOException {
        int r = inputStream().read(packet);
        if (GnirehtetService.VERBOSE) {
            Log.v(TAG, "Receiving packet: " + Binary.buildPacketString(packet, r));
        }
        return r;
    }

    private InputStream inputStream() throws IOException {
        return forwardMode ? sharedAcceptedSocket.getInputStream() : localSocket.getInputStream();
    }

    private OutputStream outputStream() throws IOException {
        return forwardMode ? sharedAcceptedSocket.getOutputStream() : localSocket.getOutputStream();
    }

    @Override
    public void close() {
        try {
            if (forwardMode) {
                synchronized (RelayTunnel.class) {
                    if (sharedAcceptedSocket != null) {
                        closeQuietly(sharedAcceptedSocket);
                        sharedAcceptedSocket = null;
                    }
                    // The shared ServerSocket is intentionally NOT closed:
                    // it is reused across reconnect attempts.
                }
            } else {
                if (localSocket != null) {
                    if (localSocket.getFileDescriptor() != null) {
                        localSocket.shutdownInput();
                        localSocket.shutdownOutput();
                    }
                    localSocket.close();
                }
            }
        } catch (IOException e) {
            throw new RuntimeException(e);
        }
    }

    private static void closeQuietly(Closeable c) {
        if (c == null) {
            return;
        }
        try {
            c.close();
        } catch (IOException ignored) {
            // best effort
        }
    }

    /**
     * The relay server sends its client id immediately after the
     * connection is established. Reading it validates that the relay is
     * alive and gives the client a stable identifier for logging.
     */
    private static void readClientId(InputStream inputStream) throws IOException {
        Log.d(TAG, "Requesting client id");
        int clientId = new DataInputStream(inputStream).readInt();
        Log.d(TAG, "Connected to the relay server as #" + Binary.unsigned(clientId));
    }
}