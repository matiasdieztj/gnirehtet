# ----------------------------------------------------------------------------
# Build configuration for the gnirehtet JNI helper library.
#
# Produces libgnirehtet-jni.so, loaded by VpnUtils via System.loadLibrary().
# Only referenced from the legacy flavor's code path (API 19-20), but built
# into both flavors because the class is in src/main/.
#
# If you want the modern flavor to skip this entirely, move VpnUtils.java and
# the cpp/ directory into src/legacy/ and update the externalNativeBuild path
# accordingly. The cost saving is negligible (a few KB); not worth the
# complexity unless you specifically want to prove the modern APK has no JNI.
# ----------------------------------------------------------------------------

cmake_minimum_required(VERSION 3.22.1)

project("gnirehtet-jni" LANGUAGES C)

add_library(
    gnirehtet-jni
    SHARED
    vpn_utils.c
)

# Link against liblog for __android_log_print.
find_library(
    log-lib
    log
)

target_link_libraries(
    gnirehtet-jni
    ${log-lib}
)

# -Wall -Wextra to catch mistakes early; -Werror to make CI fail on warnings.
# If a future NDK introduces a spurious warning, suppress that specific flag
# rather than turning -Werror off globally.
target_compile_options(
    gnirehtet-jni
    PRIVATE
    -Wall
    -Wextra
    -Werror
)
