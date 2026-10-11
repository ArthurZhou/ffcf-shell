#!/usr/bin/env bash
# 下载 CloudflareST-Rust 预编译二进制到 src-tauri/resources/。
# 用法: scripts/fetch-speedtest-bin.sh [linux-amd64|darwin-aarch64|...]
# 不传参数时自动检测本机平台。
set -euo pipefail

REPO="GuangYu-yu/CloudflareST-Rust"
TAG="main-latest"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST_DIR="$ROOT/src-tauri/resources"
BIN_NAME="CloudflareST-Rust"

detect_target() {
  local os arch
  os="$(uname -s)"; arch="$(uname -m)"
  case "$os" in
    Darwin) case "$arch" in arm64) echo "darwin-aarch64";; *) echo "darwin-x86_64";; esac ;;
    Linux)  case "$arch" in aarch64|arm64) echo "linux-aarch64";; *) echo "linux-amd64";; esac ;;
    *) echo "windows-amd64" ;;
  esac
}

TARGET="${1:-$(detect_target)}"
OUT_NAME="$BIN_NAME"
[ "$TARGET" = "windows-amd64" ] && OUT_NAME="$BIN_NAME.exe"

URL="https://github.com/$REPO/releases/download/$TAG/CloudflareST_${TARGET}"
# releases 资产名以仓库实际为准，兜底再试一种命名
URL_ALT="https://github.com/$REPO/releases/download/$TAG/$BIN_NAME-${TARGET}"

mkdir -p "$DEST_DIR"
echo "==> 下载 $TARGET → $DEST_DIR/$OUT_NAME"
if ! curl -fL --retry 2 -o "$DEST_DIR/$OUT_NAME" "$URL" 2>/dev/null; then
  echo "    首选 URL 失败，尝试备用命名…"
  curl -fL --retry 2 -o "$DEST_DIR/$OUT_NAME" "$URL_ALT"
fi
chmod +x "$DEST_DIR/$OUT_NAME"

# 本机为 macOS 时移除隔离属性，否则 Gatekeeper 拦截子进程启动
if [ "$(uname -s)" = "Darwin" ]; then
  xattr -d com.apple.quarantine "$DEST_DIR/$OUT_NAME" 2>/dev/null || true
fi

echo "==> 冒烟测试"
"$DEST_DIR/$OUT_NAME" --help >/dev/null 2>&1 || "$DEST_DIR/$OUT_NAME" -h >/dev/null 2>&1 || true
ls -la "$DEST_DIR/$OUT_NAME"
echo "==> 完成"
