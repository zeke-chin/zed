#!/usr/bin/env bash
# 把 fork 分支同步（变基）到上游的某个版本。设计见 fork/docs/7.上游同步与发布.md。
#
# 用法：fork/sync-upstream.sh [选项] <目标>
#   <目标>        上游稳定版 tag（如 v1.30.1），或 upstream/main
#
# 选项：
#   --skip-cargo  只做 git 部分和 check-hooks，跳过 cargo check 和 cargo test
#   --base <引用> 手动指定当前基准（默认自动判断，见下）
#   --fork <分支> fork 分支名（默认 fork）
#   -h, --help    显示本说明
#
# 步骤：
#   1. git fetch upstream --tags
#   2. 把本地 main 快进到 upstream/main（不切换分支）
#   3. 给 fork 打备份标签 fork-backup/<日期时间>
#   4. 计算当前基准，git rebase --onto <目标> <旧基准> fork
#   5. fork/check-hooks.sh <目标>；cargo check -p zed；cargo test（各 fork_ crate）
#
# 当前基准的自动判断：在「fork 与 upstream/main 的分叉点」和「fork 可达的最近的 v* tag」
# 里，选离 fork 更近（之间提交更少）的那个。
#
# 任何一步失败都会停下并打印恢复命令；不自动解决冲突，不推送。
# 需要在检出 fork 分支（或能检出 fork）的主仓库里运行，工作区必须干净。
# cargo 步骤需要在装有 Rust 工具链的机器（宿主机）上运行；git 部分哪里都能跑。
# worktree 里跑 cargo 时，先设 CARGO_BUILD_BUILD_DIR 复用主仓库的编译缓存（见 0.整体设计.md 第 9 节）。
set -euo pipefail

FORK_BRANCH="fork"
SKIP_CARGO=0
OLD_BASE_ARG=""
TARGET=""
FORK_CRATES="fork_ext fork_copy_anchor fork_json_explorer fork_windsurf fork_search_panel"

usage() {
    # 打印文件头部的注释作为帮助（只用 POSIX sed 语法，兼容 BSD sed）。
    sed -n '2,/^set -euo/p' "$0" | sed '/^set -euo/d' | sed 's/^# \{0,1\}//'
}

while [ $# -gt 0 ]; do
    case "$1" in
        -h | --help) usage; exit 0 ;;
        --skip-cargo) SKIP_CARGO=1 ;;
        --base)
            [ $# -ge 2 ] || { echo "错误：--base 需要参数" >&2; exit 2; }
            OLD_BASE_ARG="$2"; shift ;;
        --fork)
            [ $# -ge 2 ] || { echo "错误：--fork 需要参数" >&2; exit 2; }
            FORK_BRANCH="$2"; shift ;;
        -*) echo "错误：未知选项 $1" >&2; usage >&2; exit 2 ;;
        *)
            if [ -n "$TARGET" ]; then
                echo "错误：只能指定一个目标" >&2; exit 2
            fi
            TARGET="$1" ;;
    esac
    shift
done

if [ -z "$TARGET" ]; then
    echo "错误：缺少目标（上游稳定版 tag 或 upstream/main）" >&2
    usage >&2
    exit 2
fi

FORK_REF="refs/heads/$FORK_BRANCH"
repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

BACKUP_TAG="fork-backup/$(date +%Y%m%d-%H%M%S)"
BACKUP_CREATED=0

say() { echo "==> $*"; }

# 打印当前状态和恢复命令，然后以失败退出。
# 参数：失败原因
stop_here() {
    echo >&2
    echo "!!! 同步已停止：$1" >&2
    echo "--- 当前状态 ---" >&2
    git status -sb 2>&1 | head -30 >&2 || true
    if [ -d "$(git rev-parse --git-path rebase-merge)" ] || [ -d "$(git rev-parse --git-path rebase-apply)" ]; then
        echo "--- 变基进行中，冲突文件 ---" >&2
        git diff --name-only --diff-filter=U >&2 || true
    fi
    echo "--- 恢复命令 ---" >&2
    if [ -d "$(git rev-parse --git-path rebase-merge)" ] || [ -d "$(git rev-parse --git-path rebase-apply)" ]; then
        echo "  放弃本次变基（回到变基前）：  git rebase --abort" >&2
        echo "  或手动解决冲突后继续：        git add <文件> && git rebase --continue" >&2
        echo "                                （完成后重跑 fork/check-hooks.sh $TARGET 和 cargo 检查）" >&2
    fi
    if [ "$BACKUP_CREATED" -eq 1 ]; then
        echo "  彻底回到同步前（丢弃变基结果）：" >&2
        echo "    git rebase --abort 2>/dev/null; git checkout $FORK_BRANCH && git reset --hard $BACKUP_TAG" >&2
    else
        echo "  尚未创建备份标签，fork 分支没有被改动。" >&2
    fi
    exit 1
}

# --- 前置检查 ---

for d in rebase-merge rebase-apply; do
    if [ -d "$(git rev-parse --git-path "$d")" ]; then
        echo "错误：当前已有进行中的变基，先处理完（git rebase --abort / --continue）再运行" >&2
        exit 1
    fi
done

if ! git rev-parse --verify --quiet "refs/heads/$FORK_BRANCH" >/dev/null; then
    echo "错误：本地没有分支 $FORK_BRANCH" >&2
    exit 1
fi

# 只看已跟踪文件的改动；未跟踪文件不影响变基。
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "错误：工作区有未提交的改动，先提交或 git stash" >&2
    git status --short --untracked-files=no >&2
    exit 1
fi

# --- 1. fetch ---

say "1/5 git fetch upstream --tags"
git fetch upstream --tags || stop_here "git fetch upstream 失败（网络或远程配置）"

# --- 2. 快进 main ---

say "2/5 把本地 main 快进到 upstream/main"
if [ "$(git symbolic-ref --short -q HEAD || true)" = "main" ]; then
    stop_here "当前就在 main 上，无法用 git fetch . 更新；先切到 $FORK_BRANCH"
fi
git fetch . upstream/main:main || stop_here "main 无法快进到 upstream/main（main 上有自己的提交，或被其他 worktree 检出）"

# --- 解析目标 ---

if git rev-parse --verify --quiet "refs/tags/$TARGET" >/dev/null; then
    TARGET_KIND="tag"
elif [ "$TARGET" = "upstream/main" ]; then
    TARGET_KIND="main"
else
    stop_here "目标 $TARGET 既不是上游 tag，也不是 upstream/main"
fi
TARGET_SHA="$(git rev-parse --verify "$TARGET^{commit}")"
say "目标：${TARGET}（${TARGET_SHA}）"

# --- 计算当前基准 ---

# 列出的候选里选离 fork 最近的：基准..fork 的提交数最少。
commits_ahead() { git rev-list --count "$1..$FORK_REF"; }

if [ -n "$OLD_BASE_ARG" ]; then
    OLD_BASE="$(git rev-parse --verify "$OLD_BASE_ARG^{commit}")" || stop_here "--base $OLD_BASE_ARG 无法解析"
    OLD_BASE_DESC="手动指定 $OLD_BASE_ARG"
else
    OLD_BASE="$(git merge-base "$FORK_REF" upstream/main || true)"
    OLD_BASE_DESC="与 upstream/main 的分叉点"
    best=""
    if [ -n "$OLD_BASE" ]; then
        best="$(commits_ahead "$OLD_BASE")"
    fi
    tag_base="$(git describe --tags --abbrev=0 --match 'v[0-9]*' "$FORK_REF" 2>/dev/null || true)"
    if [ -n "$tag_base" ]; then
        tag_sha="$(git rev-parse "$tag_base^{commit}")"
        tag_ahead="$(commits_ahead "$tag_sha")"
        if [ -z "$best" ] || [ "$tag_ahead" -lt "$best" ]; then
            OLD_BASE="$tag_sha"
            OLD_BASE_DESC="旧 tag $tag_base"
            best="$tag_ahead"
        fi
    fi
    [ -n "$OLD_BASE" ] || stop_here "算不出当前基准，请用 --base 指定"
fi

fork_count="$(commits_ahead "$OLD_BASE")"
say "当前基准：${OLD_BASE}（${OLD_BASE_DESC}），fork 上有 $fork_count 个自己的提交"
if [ "$fork_count" -eq 0 ]; then
    stop_here "基准算出来 fork 上没有自己的提交，基准大概不对，请用 --base 指定"
fi
git log --oneline "$OLD_BASE..$FORK_REF" | head -40

if [ "$OLD_BASE" = "$TARGET_SHA" ]; then
    echo "fork 已经基于目标，无需变基。"
    exit 0
fi

# --- 3. 备份标签 ---

say "3/5 备份：$BACKUP_TAG -> $FORK_BRANCH ($(git rev-parse --short "$FORK_REF"))"
git tag "$BACKUP_TAG" "$FORK_REF" || stop_here "创建备份标签失败"
BACKUP_CREATED=1

# --- 4. 变基 ---

say "4/5 git rebase --onto $TARGET $(git rev-parse --short "$OLD_BASE") $FORK_BRANCH"
if ! git rebase --onto "$TARGET_SHA" "$OLD_BASE" "$FORK_BRANCH"; then
    stop_here "变基冲突或失败（不会自动解决冲突）"
fi
say "变基完成：$FORK_BRANCH 现在是 $(git rev-parse --short HEAD)"

h10_hint() {
    if [ "$TARGET_KIND" = "tag" ] && [ "$(tr -d '[:space:]' < crates/zed/RELEASE_CHANNEL)" != "dev" ]; then
        echo
        echo "提示：目标是稳定版 tag，crates/zed/RELEASE_CHANNEL 现在是 $(tr -d '[:space:]' < crates/zed/RELEASE_CHANNEL)。"
        echo "  需要加挂钩 H10（手动处理，脚本不会自动改）："
        echo "    1. 把 crates/zed/RELEASE_CHANNEL 改为 dev，在同一个提交里（fork(hook): …）"
        echo "    2. 在 fork/HOOKS.md 把 H10 从「计划中」移到挂钩清单"
        echo "  不改的话，编出来的应用会和官方 Zed.app 共用标识和数据目录，并检查更新。"
    fi
}

# --- 5. 检查 ---

say "5/5 检查"
echo "-- fork/check-hooks.sh $TARGET"
bash fork/check-hooks.sh "$TARGET" || stop_here "check-hooks 失败：有未登记的上游改动，或缺少 FORK: 标记"

if [ "$SKIP_CARGO" -eq 1 ]; then
    echo "-- 已按 --skip-cargo 跳过 cargo check 和 cargo test"
else
    command -v cargo >/dev/null 2>&1 || stop_here "找不到 cargo（需要在宿主机上运行，或加 --skip-cargo）"
    case "$repo_root" in
        */.agents/worktree/*)
            if [ -z "${CARGO_BUILD_BUILD_DIR:-}" ]; then
                echo "提示：在 worktree 里运行，未设 CARGO_BUILD_BUILD_DIR，cargo 会冷编译（见 0.整体设计.md 第 9 节）"
            fi ;;
    esac
    echo "-- cargo check -p zed"
    cargo check -p zed || stop_here "cargo check -p zed 失败：多半是上游接口变了，按编译错误适配，并记入 fork/CHANGELOG.md"
    test_args=""
    for c in $FORK_CRATES; do
        test_args="$test_args -p $c"
    done
    echo "-- cargo test$test_args"
    # shellcheck disable=SC2086
    cargo test $test_args || stop_here "fork 各 crate 的测试失败"
fi

echo
say "同步成功：${FORK_BRANCH} 已基于 ${TARGET}。备份标签：$BACKUP_TAG"
h10_hint
echo
echo "后续（手动）："
echo "  1. 过一遍 fork/回归清单.md"
echo "  2. 有接口适配的话记入 fork/CHANGELOG.md"
echo "  3. 确认没问题后强制推送：git push --force-with-lease origin $FORK_BRANCH"
echo "  4. 不满意可回到同步前：git reset --hard $BACKUP_TAG"
