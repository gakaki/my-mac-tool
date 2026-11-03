#!/bin/zsh
set -euo pipefail

project_dir="$(cd "$(dirname "$0")/.." && pwd)"
version="${1:-1.0.0}"
version="${version#v}"
arch="$(uname -m)"
app_name="我的 Mac 设置和开发工具安装"
app_dir="$project_dir/dist/$app_name.app"
dist_dir="$project_dir/dist"
dmg_name="my-mac-tool-${version}-macOS-${arch}.dmg"
dmg_path="$dist_dir/$dmg_name"

if [[ "${SKIP_APP_BUILD:-0}" != "1" ]]; then
  "$project_dir/packaging/package_app.sh" "$version"
fi

staging_dir="$(mktemp -d "${TMPDIR:-/tmp}/my-mac-tool-dmg.XXXXXX")"
trap 'rm -rf "$staging_dir"' EXIT

ditto "$app_dir" "$staging_dir/$app_name.app"
ln -s /Applications "$staging_dir/Applications"

mkdir -p "$dist_dir"
rm -f "$dmg_path" "$dmg_path.sha256"
hdiutil create \
  -volname "My Mac Tool" \
  -srcfolder "$staging_dir" \
  -format UDZO \
  -ov \
  "$dmg_path"
hdiutil verify "$dmg_path"

(
  cd "$dist_dir"
  shasum -a 256 "$dmg_name" > "$dmg_name.sha256"
)

echo "$dmg_path"
