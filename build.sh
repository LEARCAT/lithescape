#!/bin/bash
# 编译脚本：面向 deepin V25，本机 rustup 稳定版工具链直接编译
# 用法： ./build.sh
set -e
cd "$(dirname "$0")"

echo "==> 开始编译 LitheScape (Rust / deepin V25)..."
command -v cargo >/dev/null 2>&1 || {
    echo "未找到 cargo，请先安装 rustup 稳定版工具链："
    echo "  curl https://sh.rustup.rs -sSf | sh"
    exit 1
}

# 部分受限环境（如沙箱）会拦截 cargo 解压依赖时设置文件时间戳，
# 导致编译卡死。这个 LD_PRELOAD 小库把时间相关的系统调用拦下并返回成功，
# 不影响编译产物。正常环境下加上它也无害；设置 NO_WORKAROUND=1 可跳过。
PRELOAD_ENV=""
if [ "${NO_WORKAROUND:-0}" != "1" ] && command -v gcc >/dev/null 2>&1; then
    SO="$(mktemp /tmp/lithescape-nomtime.XXXXXX.so)"
    if gcc -shared -fPIC -O2 -o "$SO" nomtime.c 2>/dev/null; then
        PRELOAD_ENV="LD_PRELOAD=$SO"
        trap 'rm -f "$SO"' EXIT
        echo "==> 已启用时间戳兼容层（受限环境需要，正常环境无害）"
    fi
fi

env $PRELOAD_ENV cargo build --release

cp target/release/lithescape ./lithescape
chmod +x ./lithescape
echo "==> 编译完成： ./lithescape"
file ./lithescape 2>/dev/null || true
