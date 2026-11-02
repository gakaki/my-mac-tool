#!/bin/zsh
set -euo pipefail

project_dir="$(cd "$(dirname "$0")/.." && pwd)"
xcode_sdk="/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk"
xcode_linker="/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/clang"

if [[ -d "$xcode_sdk" && -x "$xcode_linker" ]]; then
  export SDKROOT="$xcode_sdk"
  export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER="$xcode_linker"
  export MACOSX_DEPLOYMENT_TARGET=11.0
fi

cd "$project_dir"
cargo build --release --bin my-mac-installer --bin toolchain-installer

app_dir="$project_dir/dist/我的 Mac 设置和开发工具安装.app"
rm -rf "$app_dir"
mkdir -p "$app_dir/Contents/MacOS" "$app_dir/Contents/Resources"

cp "$project_dir/target/release/my-mac-installer" "$app_dir/Contents/MacOS/my-mac-installer"
cp "$project_dir/target/release/toolchain-installer" "$app_dir/Contents/MacOS/toolchain-installer"
cp "$project_dir/packaging/Info.plist" "$app_dir/Contents/Info.plist"

icon_work="$(mktemp -d)"
trap 'rm -rf "$icon_work"' EXIT
iconset="$icon_work/AppIcon.iconset"
mkdir -p "$iconset"
qlmanage -t -s 1024 -o "$icon_work" "$project_dir/packaging/AppIcon.svg" >/dev/null 2>&1
icon_png="$icon_work/AppIcon.svg.png"
if [[ ! -f "$icon_png" ]]; then
  echo "failed to render AppIcon.svg" >&2
  exit 1
fi
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$icon_png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  retina=$((size * 2))
  sips -z "$retina" "$retina" "$icon_png" --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
cp "$icon_png" "$iconset/icon_512x512@2x.png"
iconutil -c icns "$iconset" -o "$app_dir/Contents/Resources/AppIcon.icns"

plutil -lint "$app_dir/Contents/Info.plist"
codesign --force --deep --sign - "$app_dir"

echo "$app_dir"
