#!/bin/bash
# 把编译好的二进制打包成 .deb
# 版本号单一来源：Cargo.toml 的 version（与 LitheZones 一致），
# control 的版本号由脚本自动写入，避免多处硬编码不同步。
#
# 产物存放约定（与 LitheZones 统一）：
#   本仓库（lithescape-release）是发布仓库，保持「纯源码、零产物」；
#   .deb 一律输出到开发目录 ../lithescape/，避免同一版本在多处重复存放。
#   需要传参指定其它输出目录时：./package.sh [输出目录]
#
#   本脚本是唯一一份，发布仓库与开发目录共用（开发目录用
#   ../lithescape-release/package.sh 调用），避免两份副本漂移。
#   源码与编译产物位置自动探测，从哪个目录调用都能正常工作。
set -e
HERE="$(cd "$(dirname "$0")" && pwd)"

# 源码根与开发目录：本仓库叫 lithescape-release，开发目录叫 lithescape。
REPO_ROOT="$HERE"
DEV_ROOT="$(dirname "$HERE")/lithescape"

# 源码与编译产物可能分别在发布仓库或开发目录，逐个探测。
SRC_ROOT=""
BINARY=""
for cand in "$REPO_ROOT" "$DEV_ROOT"; do
    [ -f "$cand/Cargo.toml" ] || continue
    [ -z "$SRC_ROOT" ] && SRC_ROOT="$cand"
    if [ -f "$cand/lithescape" ]; then
        BINARY="$cand/lithescape"
        break
    fi
done
[ -n "$SRC_ROOT" ] || { echo "找不到源码根目录（Cargo.toml）"; exit 1; }
[ -n "$BINARY" ] || { echo "找不到编译产物，请先运行 ./build.sh"; exit 1; }
cd "$SRC_ROOT"

# 从 Cargo.toml [package] 段读取 version
VERSION=$(awk -F'"' '/^version[[:space:]]*=/ {print $2; exit}' Cargo.toml)
[ -n "$VERSION" ] || { echo "无法从 Cargo.toml 读取版本号"; exit 1; }
echo "==> 打包版本: $VERSION"

cp "$BINARY" deb/usr/bin/lithescape
chmod 755 deb/usr/bin/lithescape

# 同步 control 的版本号（保留其余字段不变）
sed -i "s/^Version:.*/Version: $VERSION/" deb/DEBIAN/control

# 输出目录：默认为开发目录（唯一存放处），发布仓库内不落 deb。
OUT_DIR="${1:-$DEV_ROOT}"
mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"

DEB="$OUT_DIR/lithescape_${VERSION}_amd64.deb"
dpkg-deb --build --root-owner-group deb "$DEB"
echo "==> 已生成 $DEB"
dpkg-deb -I "$DEB"
