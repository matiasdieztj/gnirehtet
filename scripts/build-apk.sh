#!/bin/bash
# Build the gnirehtet Android APK in release mode. Requires JDK 17+ and Android SDK.
#
# The release APK is signed with the built-in debug keystore (AGP
# auto-generates it at ~/.android/debug.keystore on first use). No
# manual keystore configuration is required.
set -e

cd "$(dirname "$0")/.."

# Auto-detect Android SDK if local.properties doesn't exist
if [ ! -f local.properties ]; then
    for sdk in "$HOME/Android/Sdk" /usr/lib/android-sdk /opt/android-sdk; do
        if [ -d "$sdk" ] && ( [ -d "$sdk/platforms" ] || [ -d "$sdk/build-tools" ] ); then
            echo "sdk.dir=$sdk" > local.properties
            echo "Android SDK found at $sdk"
            break
        fi
    done
fi

echo "Building release APK..."
./gradlew :app:assembleRelease

APK=app/build/outputs/apk/release/app-release.apk
if [ ! -f "$APK" ]; then
    echo "ERROR: $APK not found." >&2
    echo "If you see app-release-unsigned.apk instead, check that" >&2
    echo "'signingConfig signingConfigs.debug' is set in app/build.gradle" >&2
    exit 1
fi

echo "APK at $APK"