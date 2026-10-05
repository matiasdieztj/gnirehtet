package com.genymobile.gnirehtet;

import androidx.multidex.MultiDexApplication;

/**
 * Application class for gnirehtet.
 *
 * <p>Extends {@link MultiDexApplication} to support the legacy flavor
 * (minSdk 19). On API 21+ multidex is native and this class is a no-op.
 * On API 19-20 it installs the auxiliary DEX files at startup, which is
 * required because {@code coreLibraryDesugaringEnabled} is active and the
 * desugared classes do not fit in the main DEX.
 *
 * <p>Registered in AndroidManifest.xml via {@code android:name=".GnirehtetApplication"}.
 */
public class GnirehtetApplication extends MultiDexApplication {
    // No custom logic. MultiDexApplication.attachBaseContext() handles
    // the DEX installation automatically on API 19-20, and does nothing
    // on API 21+.
}