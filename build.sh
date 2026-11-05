#!/usr/bin/env sh
# Linux / macOS 构建脚本：编译 release 并把产物放到 dist/。
# 用法： ./build.sh            普通构建（动态链接 glibc）
#        ./build.sh musl       静态链接（需要 x86_64-unknown-linux-musl target）
set -eu

cd "$(dirname "$0")"

TARGET=""
if [ "${1:-}" = "musl" ]; then
    TARGET="--target x86_64-unknown-linux-musl"
    command -v rustup >/dev/null 2>&1 && rustup target add x86_64-unknown-linux-musl || true
fi

cargo build --release $TARGET

if [ -n "$TARGET" ]; then
    SRC="target/x86_64-unknown-linux-musl/release/sshu"
    OUT="dist/sshu-linux-x86_64-musl"
else
    SRC="target/release/sshu"
    OUT="dist/sshu-$(uname -s | tr '[:upper:]' '[:lower:]')-$(uname -m)"
fi

mkdir -p dist
cp "$SRC" "$OUT"
strip "$OUT" 2>/dev/null || true

echo "OK: $OUT"
ls -l "$OUT"
