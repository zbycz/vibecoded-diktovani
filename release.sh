#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT_DIR"

BIN_NAME="whispering-mvp"
APP_NAME="Diktovani"
VERSION="${APP_VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)}"
APP_DIR="target/release/bundle/${APP_NAME}.app"
CONTENTS_DIR="${APP_DIR}/Contents"
MACOS_DIR="${CONTENTS_DIR}/MacOS"
RESOURCES_DIR="${CONTENTS_DIR}/Resources"
ICONSET_DIR="assets/AppIcon.appiconset"
ICON_NAME="AppIcon"

build_icns() {
    local iconset_dir="$1"
    local output_icns="$2"
    local tmp_dir

    tmp_dir="$(mktemp -d)"
    trap 'rm -rf "$tmp_dir"' RETURN

    mkdir -p "${tmp_dir}/${ICON_NAME}.iconset"

    cp "${iconset_dir}/icon_16x16.png" "${tmp_dir}/${ICON_NAME}.iconset/icon_16x16.png"
    cp "${iconset_dir}/icon_32x32.png" "${tmp_dir}/${ICON_NAME}.iconset/icon_16x16@2x.png"
    cp "${iconset_dir}/icon_32x32.png" "${tmp_dir}/${ICON_NAME}.iconset/icon_32x32.png"
    cp "${iconset_dir}/icon_64x64.png" "${tmp_dir}/${ICON_NAME}.iconset/icon_32x32@2x.png"
    cp "${iconset_dir}/icon_128x128.png" "${tmp_dir}/${ICON_NAME}.iconset/icon_128x128.png"
    cp "${iconset_dir}/icon_256x256.png" "${tmp_dir}/${ICON_NAME}.iconset/icon_128x128@2x.png"
    cp "${iconset_dir}/icon_256x256.png" "${tmp_dir}/${ICON_NAME}.iconset/icon_256x256.png"
    cp "${iconset_dir}/icon_512x512.png" "${tmp_dir}/${ICON_NAME}.iconset/icon_256x256@2x.png"
    cp "${iconset_dir}/icon_512x512.png" "${tmp_dir}/${ICON_NAME}.iconset/icon_512x512.png"
    cp "${iconset_dir}/icon_1024x1024.png" "${tmp_dir}/${ICON_NAME}.iconset/icon_512x512@2x.png"

    iconutil -c icns "${tmp_dir}/${ICON_NAME}.iconset" -o "$output_icns"
}

cargo build --release

rm -rf "$APP_DIR"
mkdir -p "$MACOS_DIR" "$RESOURCES_DIR"

cp "target/release/${BIN_NAME}" "${MACOS_DIR}/${BIN_NAME}"
chmod +x "${MACOS_DIR}/${BIN_NAME}"

if [[ -d "$ICONSET_DIR" ]]; then
    build_icns "$ICONSET_DIR" "${RESOURCES_DIR}/${ICON_NAME}.icns"
fi

cat > "${CONTENTS_DIR}/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>${APP_NAME}</string>
    <key>CFBundleExecutable</key>
    <string>${BIN_NAME}</string>
    <key>CFBundleIconFile</key>
    <string>${ICON_NAME}.icns</string>
    <key>CFBundleIdentifier</key>
    <string>com.example.${APP_NAME}</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>${APP_NAME}</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>LSUIElement</key>
    <true/>
</dict>
</plist>
EOF

echo "Built ${APP_DIR}"
