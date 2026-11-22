#!/usr/bin/env sh
# dsh-mini 在 Linux / WSL / 车机上的构建入口。
#
#   ./scripts/build.sh                 # 本机（WSL 里就是 Linux）release 构建
#   ./scripts/build.sh debug           # dev 构建 + 跑测试
#   TARGET=aarch64-unknown-linux-gnu ./scripts/build.sh   # 交叉编译到 ARM64 车机
#
# 两个在 WSL 上会白白浪费几十分钟的坑，脚本里已经处理：
#
# 1. **别在 /mnt/c、/mnt/d 上编译**。那是 9p 文件系统，每次 stat/open 都要过一层
#    虚拟机边界，增量编译会比 Linux 原生盘慢一个数量级。这里检测到在 /mnt 下就
#    自动把 CARGO_TARGET_DIR 挪到 $HOME。
# 2. **别和 Windows 共用 target/**。两边产物格式不同（.exe / ELF、不同的
#    fingerprint），共用同一个目录会让 cargo 不停重编。所以 Linux 侧固定用
#    target/linux-<target> 之外的独立目录。
set -eu

PROFILE="${1:-release}"

# --- 定位仓库根（脚本可能从任意目录调用） -------------------------------------
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)
cd "$ROOT"

# --- 避开 9p 文件系统 ---------------------------------------------------------
case "$ROOT" in
/mnt/*)
    CACHE_ROOT="${XDG_CACHE_HOME:-$HOME/.cache}/dsh-mini"
    # 每个 target 一个目录，避免 x86_64 和 aarch64 互相污染。
    export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$CACHE_ROOT/target-${TARGET:-native}}"
    echo "检测到 /mnt 下的源码目录（9p），构建产物改放：$CARGO_TARGET_DIR" >&2
    ;;
esac

# --- 需要时设置交叉编译目标 ---------------------------------------------------
if [ -n "${TARGET:-}" ]; then
    echo "交叉编译目标：$TARGET" >&2
    # 交叉编译需要对应的链接器与 C 工具链（rustls 会编 C 代码）：
    #   aarch64: apt install gcc-aarch64-linux-gnu
    #   export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
    # 或者直接用 `cross build --target aarch64-unknown-linux-gnu`。
fi

# --- 构建 ---------------------------------------------------------------------
if [ "$PROFILE" = "debug" ]; then
    cargo build --workspace --all-targets ${TARGET:+--target "$TARGET"}
    cargo test --workspace ${TARGET:+--target "$TARGET"}
    BIN_DIR="debug"
else
    cargo build --release --workspace ${TARGET:+--target "$TARGET"}
    BIN_DIR="release"
fi

TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
if [ -n "${TARGET:-}" ]; then
    OUT="$TARGET_DIR/$TARGET/$BIN_DIR"
else
    OUT="$TARGET_DIR/$BIN_DIR"
fi

echo "" >&2
echo "构建完成：$OUT/dsh-mini" >&2
ls -l "$OUT/dsh-mini" "$OUT/mock-mcp-server" >&2 2>/dev/null || true
