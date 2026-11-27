#!/bin/bash
# 把编译好的二进制打包成 .deb
# 版本号单一来源：Cargo.toml 的 version（与 LitheZones 一致），
# control 的版本号由脚本自动写入，避免多处硬编码不同步。
set -e
cd "$(dirname "$0")"
[ -f ./lithescape ] || { echo "请先运行 ./build.sh"; exit 1; }

# 从 Cargo.toml [package] 段读取 version
VERSION=$(awk -F'"' '/^version[[:space:]]*=/ {print $2; exit}' Cargo.toml)
[ -n "$VERSION" ] || { echo "无法从 Cargo.toml 读取版本号"; exit 1; }
echo "==> 打包版本: $VERSION"

cp ./lithescape deb/usr/bin/lithescape
chmod 755 deb/usr/bin/lithescape

# 同步 control 的版本号（保留其余字段不变）
sed -i "s/^Version:.*/Version: $VERSION/" deb/DEBIAN/control

DEB="lithescape_${VERSION}_amd64.deb"
dpkg-deb --build --root-owner-group deb "$DEB"
echo "==> 已生成 $DEB"
dpkg-deb -I "$DEB"
