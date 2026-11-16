#!/usr/bin/env bash
# 打出本机架构的 Zed Dev 安装包（fork 构建）。设计见 fork/docs/7.上游同步与发布.md 的 7.4。
#
# 用法：fork/bundle.sh [选项]
#   --dry-run   只打印会做什么（版本号、环境变量、产物路径、前置检查结果），不编译
#   -h, --help  显示本说明
#
# 做的事：包装 script/bundle-mac，release 编译，使用本地临时签名（ad hoc，不需要证书），
# 产物 DMG 复制到 OUT_DIR，文件名形如：
#   Zed-Dev-<上游版本>-fork.<fork 短提交>[-dirty]-<架构>.dmg
#
# 版本字符串：通过构建时环境变量 ZED_COMMIT_SHA 注入 "<完整提交>-fork"，
# 「关于」窗口里的完整版本形如 1.30.0+dev.<提交>-fork。不改上游代码。
# 注意：完整版本里的 fork 标记在 + 之后的构建元数据里；「关于」窗口顶部的 1.30.0 仍是上游版本号。
#
# 要求：macOS、Rust 工具链、Xcode 命令行工具（SDK 26+）、node/npm（bundle-mac 会装 dmg-license）。
# 首次运行 bundle-mac 会 cargo install cargo-bundle，release 编译耗时很长。
# 需要先把 crates/zed/RELEASE_CHANNEL 设为 dev（挂钩 H10），否则会与官方 Zed.app 冲突，脚本会拒绝打包。
set -euo pipefail

# ---- 可配置项 ----
OUT_DIR="${FORK_BUILDS_DIR:-$HOME/workspace/zed-fork-builds}"
FORK_MARK="fork"
# ------------------

DRY_RUN=0
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=1 ;;
        -h | --help)
            sed -n '2,/^set -euo/p' "$0" | sed '/^set -euo/d' | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *) echo "错误：未知参数 $1（见 --help）" >&2; exit 2 ;;
    esac
    shift
done

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

problems=0
problem() { echo "问题：$*" >&2; problems=$((problems + 1)); }

# 上游版本号：crates/zed/Cargo.toml 的 [package] version
version="$(sed -n 's/^version = "\(.*\)"/\1/p' crates/zed/Cargo.toml | head -1)"
[ -n "$version" ] || { echo "错误：读不到 crates/zed/Cargo.toml 的 version" >&2; exit 1; }

full_sha="$(git rev-parse HEAD)"
short_sha="$(git rev-parse --short=9 HEAD)"
dirty=""
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    dirty="-dirty"
fi

channel="$(tr -d '[:space:]' < crates/zed/RELEASE_CHANNEL)"
if [ "$channel" != "dev" ]; then
    problem "crates/zed/RELEASE_CHANNEL 是 ${channel}，不是 dev；需要先加挂钩 H10（见 fork/HOOKS.md）"
fi

case "$(uname -m)" in
    arm64 | aarch64) arch="aarch64" ;;
    x86_64) arch="x86_64" ;;
    *) arch=""; problem "不支持的架构 $(uname -m)" ;;
esac
triple="${arch}-apple-darwin"

if [ "$(uname -s)" != "Darwin" ]; then
    problem "不是 macOS，无法运行 script/bundle-mac"
fi
for tool in cargo rustc xcrun npm; do
    command -v "$tool" >/dev/null 2>&1 || problem "找不到 $tool"
done
[ -x script/bundle-mac ] || problem "找不到 script/bundle-mac"

out_name="Zed-Dev-${version}-${FORK_MARK}.${short_sha}${dirty}-${arch}.dmg"
src_dmg="target/${triple}/release/Zed-${arch}.dmg"
export ZED_COMMIT_SHA="${full_sha}-${FORK_MARK}"

echo "上游版本：$version"
echo "fork 提交：$short_sha${dirty}"
echo "发布通道：$channel"
echo "目标架构：$triple"
echo "版本标识（ZED_COMMIT_SHA）：$ZED_COMMIT_SHA"
echo "签名方式：本地临时签名（ad hoc）"
echo "bundle-mac 产物：$src_dmg"
echo "复制到：$OUT_DIR/$out_name"
if [ -n "$dirty" ]; then
    echo "警告：工作区有未提交的改动，文件名带 -dirty"
fi

if [ "$DRY_RUN" -eq 1 ]; then
    if [ "$problems" -gt 0 ]; then
        echo "dry-run：实际运行会因上面 $problems 个问题中止"
    else
        echo "dry-run：前置检查通过，未编译"
    fi
    exit 0
fi

if [ "$problems" -gt 0 ]; then
    echo "错误：前置检查没通过，已中止" >&2
    exit 1
fi

# 强制走本地临时签名：不让环境里的证书、公证、sentry 变量触发正式签名流程。
unset MACOS_CERTIFICATE MACOS_CERTIFICATE_PASSWORD \
    APPLE_NOTARIZATION_KEY APPLE_NOTARIZATION_KEY_ID APPLE_NOTARIZATION_ISSUER_ID \
    MACOS_SIGNING_KEY SENTRY_AUTH_TOKEN

echo "开始打包（release 编译，耗时较长）……"
script/bundle-mac "$triple"

[ -f "$src_dmg" ] || { echo "错误：没找到产物 $src_dmg" >&2; exit 1; }
mkdir -p "$OUT_DIR"
cp "$src_dmg" "$OUT_DIR/$out_name"
echo "完成：$OUT_DIR/$out_name"
echo "安装：打开 DMG 把 Zed Dev 拖进 /Applications（与官方 Zed.app 并存）。"
