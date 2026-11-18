#!/bin/sh
# 重新 vendor Scalar 文档 UI 资产（@scalar/api-reference standalone bundle + 字体）。
# 用法: ./vendor.sh <版本号>   例: ./vendor.sh 1.80.0
# 完成后需手动同步 src/router_docs/scalar_assets.rs 的 SCALAR_ASSET_VERSION，
# 详见同目录 README.md。
set -eu

VER="${1:?usage: vendor.sh <@scalar/api-reference version>}"
DIR="$(cd "$(dirname "$0")" && pwd)"
SRC="$(mktemp -d)"
trap 'rm -rf "$SRC"' EXIT

curl -fsSL "https://cdn.jsdelivr.net/npm/@scalar/api-reference@${VER}/dist/browser/standalone.js" \
  -o "$SRC/standalone.js"

# bundle @font-face 引用的全部字体分片（升级时若新增字体需同步
# scalar_assets.rs 的 SCALAR_FONTS 清单）
FONTS="inter-cyrillic-ext inter-cyrillic inter-greek-ext inter-greek inter-latin-ext inter-latin inter-symbols inter-vietnamese mono-cyrillic-ext mono-cyrillic mono-greek mono-latin-ext mono-latin mono-vietnamese"
for f in $FONTS; do
  curl -fsSL "https://fonts.scalar.com/${f}.woff2" -o "$SRC/${f}.woff2"
done

# 改写：字体 URL 同源化 + 头部溯源注释
{
  printf '/*! Scalar API Reference standalone bundle - vendored from @scalar/api-reference@%s (MIT, see LICENSE). Font URLs rewritten to same-origin /api/docs/scalar/assets/fonts/ - do not edit; re-run vendor.sh to upgrade. */\n' "$VER"
  sed 's|https://fonts\.scalar\.com/|/api/docs/scalar/assets/fonts/|g' "$SRC/standalone.js"
} > "$DIR/scalar-api-reference.js"

mkdir -p "$DIR/fonts"
cp "$SRC"/*.woff2 "$DIR/fonts/"

echo "vendored @scalar/api-reference@${VER}:"
ls -l "$DIR/scalar-api-reference.js"
ls "$DIR/fonts" | wc -l
echo "剩余外链检查（应无输出）:"
grep -c 'fonts\.scalar\.com\|jsdelivr' "$DIR/scalar-api-reference.js" || true
echo "下一步: 同步 scalar_assets.rs 的 SCALAR_ASSET_VERSION（含新增字体时同步 SCALAR_FONTS）。"
