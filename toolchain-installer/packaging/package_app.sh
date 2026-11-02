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
plutil -lint "$app_dir/Contents/Info.plist"
codesign --force --deep --sign - "$app_dir"

echo "$app_dir"
