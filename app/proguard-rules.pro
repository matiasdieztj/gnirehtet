# Add project specific ProGuard rules here.
# By default, the flags in this file are appended to flags specified
# in /home/rom/android/sdk/tools/proguard/proguard-android.txt
# You can edit the include path and order by changing the proguardFiles
# directive in build.gradle.
#
# For more details, see
#   http://developer.android.com/guide/developing/tools/proguard.html

# Add any project specific keep options here:

# If your project uses WebView with JS, uncomment the following
# and specify the fully qualified class name to the JavaScript interface
# class:
#-keepclassmembers class fqcn.of.javascript.interface.for.webview {
#   public *;
#}

# --- Gnirehtet keep rules ---

# Keep the Activity and Service declared in AndroidManifest.xml.
# R8 normally keeps manifest-declared classes, but being explicit
# avoids surprises if the manifest is ever refactored.
-keep class com.genymobile.gnirehtet.GnirehtetActivity { *; }
-keep class com.genymobile.gnirehtet.GnirehtetService { *; }

# Keep the VpnService subclass, since Android instantiates it by name.
-keep class * extends android.net.VpnService { *; }

# Keep BroadcastReceiver / Service subclasses referenced by name.
-keep class * extends android.app.Service { *; }

# Keep the Parcelable CREATOR fields: Android's Parcel unmarshalling
# accesses them reflectively, and R8 cannot see that usage.
-keepclassmembers class * implements android.os.Parcelable {
    public static final ** CREATOR;
}

# Keep all enums: R8 in full mode (AGP 8+) may strip enum values
# that are only accessed via reflection (e.g. Enum.valueOf).
-keepclassmembers enum * {
    public static **[] values();
    public static ** valueOf(java.lang.String);
}

# Preserve line numbers for readable crash reports. This does not
# affect APK size meaningfully.
-keepattributes SourceFile,LineNumberTable