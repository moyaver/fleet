#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
# Builds the ANR fixture app with the installed Android SDK only: no Gradle, no downloads.
# The APK is signed with a debug key that keytool makes in a fresh temporary directory, so no
# key is kept in the repository. Usage: build.sh <out-dir>   (writes <out-dir>/anr-app.apk)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=${1:?usage: build.sh <out-dir>}
sdk=${ANDROID_HOME:-$HOME/Library/Android/sdk}
bt=$sdk/build-tools/36.0.0
jar=$sdk/platforms/android-33/android.jar
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$out" "$work/classes" "$work/dex"

javac --release 11 -classpath "$jar" -d "$work/classes" $(find "$here/src" -name '*.java')
"$bt/d8" --release --min-api 26 --lib "$jar" --output "$work/dex" $(find "$work/classes" -name '*.class')
"$bt/aapt2" link -o "$work/unsigned.apk" -I "$jar" --manifest "$here/AndroidManifest.xml"
zip -q -j "$work/unsigned.apk" "$work/dex/classes.dex"
"$bt/zipalign" -f -p 4 "$work/unsigned.apk" "$work/aligned.apk"
keytool -genkeypair -keystore "$work/debug.keystore" -storepass android -keypass android \
    -alias androiddebugkey -keyalg RSA -keysize 2048 -validity 10000 \
    -dname "CN=Fleet ANR fixture, O=Fleet, C=GB" >/dev/null 2>&1
"$bt/apksigner" sign --ks "$work/debug.keystore" --ks-key-alias androiddebugkey \
    --ks-pass pass:android --key-pass pass:android --out "$out/anr-app.apk" "$work/aligned.apk"
echo "built $out/anr-app.apk"
