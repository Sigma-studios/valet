#!/usr/bin/env bash
# Builds valet's Android app and installs it on the phone, plugged in or over
# Wi-Fi (wireless debugging, paired once: see the README). Does nothing when
# the phone already has this build, so it can run after every deploy.
#
#   android/install.sh [--force]
#
# ANDROID_SERIAL picks the phone when adb sees several devices; VALET_PHONE
# (ip:port) is connected to when adb finds none.
set -euo pipefail

cd "$(dirname "$0")"
FORCE=0
[ "${1:-}" = --force ] && FORCE=1

# Android Studio's JDK, unless one is set.
STUDIO_JDK="/Applications/Android Studio.app/Contents/jbr/Contents/Home"
[ -z "${JAVA_HOME:-}" ] && [ -d "$STUDIO_JDK" ] && export JAVA_HOME="$STUDIO_JDK"

# One version per commit touching android/: the phone tells which build it has.
VERSION="$(git rev-list --count HEAD -- .)"
DIRTY="$(git status --porcelain -- .)"

devices() { adb devices | awk 'NR > 1 && $2 == "device" { print $1 }'; }

# A paired phone with wireless debugging on is found over mDNS, in a moment.
adb start-server > /dev/null
[ -z "$(devices)" ] && [ -n "${VALET_PHONE:-}" ] && adb connect "$VALET_PHONE" > /dev/null || true
for _ in $(seq 1 20); do
    [ -n "$(devices)" ] && break
    sleep 0.5
done
if [ -z "${ANDROID_SERIAL:-}" ]; then
    case "$(devices | wc -l | tr -d ' ')" in
        0) if adb devices | grep -q unauthorized; then
               echo "unlock the phone and allow USB debugging from this computer, then run again"
           else
               echo "no phone: plug it in (USB debugging on) or turn on wireless debugging"
           fi
           exit 1 ;;
        1) ;;
        *) echo "several devices, pick one with ANDROID_SERIAL=…:"; devices; exit 1 ;;
    esac
fi

INSTALLED="$(adb shell dumpsys package app.valet | sed -n 's/.*versionCode=\([0-9]*\).*/\1/p' | head -n 1)"
if [ "$FORCE" = 0 ] && [ -z "$DIRTY" ] && [ "$INSTALLED" = "$VERSION" ]; then
    echo "the phone already has build $VERSION"
    exit 0
fi

./gradlew -q assembleDebug -PversionCode="$VERSION"
# -d: an older build (an earlier checkout) replaces a newer one too.
adb install -r -d app/build/outputs/apk/debug/app-debug.apk
echo "build $VERSION installed${DIRTY:+ (with uncommitted changes)}"
