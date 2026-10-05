/*
 * JNI helpers for the gnirehtet legacy (API 19) build.
 *
 * The only function here is setBlockingMode(), which clears O_NONBLOCK on a
 * file descriptor. On API 21+ this is unnecessary because VpnService.Builder
 * has setBlocking(). On API 19-20 we have to do it by hand.
 *
 * fcntl(F_SETFL) is async-signal-safe and does not allocate; this call is
 * safe from any thread and cannot fail due to memory pressure.
 */

#include <jni.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <string.h>
#include <android/log.h>

#define LOG_TAG "GnirehtetJNI"
#define LOGI(...) __android_log_print(ANDROID_LOG_INFO,  LOG_TAG, __VA_ARGS__)
#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, LOG_TAG, __VA_ARGS__)

JNIEXPORT void JNICALL
Java_com_genymobile_gnirehtet_VpnUtils_setBlockingMode(JNIEnv *env, jclass clazz, jint fd)
{
    (void)env;
    (void)clazz;

    if (fd < 0) {
        LOGE("setBlockingMode: invalid fd %d", fd);
        return;
    }

    int flags = fcntl(fd, F_GETFL, 0);
    if (flags == -1) {
        LOGE("setBlockingMode: fcntl(F_GETFL) failed on fd=%d: %s", fd, strerror(errno));
        return;
    }

    if ((flags & O_NONBLOCK) == 0) {
        /* Already blocking. Nothing to do. */
        LOGI("setBlockingMode: fd=%d already blocking", fd);
        return;
    }

    if (fcntl(fd, F_SETFL, flags & ~O_NONBLOCK) == -1) {
        LOGE("setBlockingMode: fcntl(F_SETFL) failed on fd=%d: %s", fd, strerror(errno));
        return;
    }

    LOGI("setBlockingMode: fd=%d set to blocking", fd);
}
