#!/usr/bin/env bash
# 建、删带构建缓存的 git worktree。设计见 fork/docs/15.worktree构建缓存.md。
#
# 用法：
#   fork/worktree.sh new <名字> [基准]   在 .agents/worktree/<名字> 建 worktree（分支 wt/<名字>，基准默认 fork），
#                                       再把母本的 target/debug 用 APFS 克隆（cp -c）到 worktree 的 target/debug
#   fork/worktree.sh cache <名字>        给已有 worktree 补克隆母本缓存（target/debug 已存在时不动）
#   fork/worktree.sh rm <名字> [--force] 删除 worktree（target/ 一起删），再删已合并的分支 wt/<名字>
#   fork/worktree.sh base               打印母本路径和大小
#   fork/worktree.sh base-update [基准]  把母本 worktree（.agents/worktree/_base）切到基准（默认 fork）并 cargo build -p zed
#
# 母本由专用 worktree .agents/worktree/_base 构建（它的 target 指向 .cache/zed-base）。不用主仓库：
# worktree 在主仓库目录下面，cargo 会把两份 .cargo/config.toml 的 rustflags 拼起来，
# 单元哈希和主仓库对不上；_base 和其他 worktree 在同一层，哈希一致。
set -euo pipefail

usage() {
    sed -n '2,/^set -euo/p' "$0" | sed '/^set -euo/d' | sed 's/^# \{0,1\}//'
}

common_dir="$(cd "$(git rev-parse --git-common-dir)" && pwd -P)"
main_root="$(dirname "$common_dir")"
worktrees_dir="$main_root/.agents/worktree"
base_name="_base"
base_dir="$worktrees_dir/$base_name/target"

worktree_path() {
    [[ "$1" =~ ^[A-Za-z0-9._-]+$ ]] || { echo "错误：名字只能用字母、数字、. _ -：$1" >&2; exit 2; }
    echo "$worktrees_dir/$1"
}

clone_cache() {
    local path="$1"
    if [ -e "$path/target/debug" ]; then
        echo "已有 $path/target/debug，不覆盖"
        return 0
    fi
    if [ ! -d "$base_dir/debug" ]; then
        echo "提示：母本 $base_dir/debug 不存在，这个 worktree 会从零编译（先运行 fork/worktree.sh base-update）"
        return 0
    fi
    mkdir -p "$path/target"
    local start=$SECONDS
    cp -c -R "$base_dir/debug" "$path/target/debug"
    # cargo 按修改时间判断是否重编：在母本构建之前改过的文件看起来比母本产物旧，
    # 会被当成没变而沿用母本里的旧产物。克隆后把工作区文件统一更新到现在。
    git -C "$path" ls-files -z --cached --others --exclude-standard |
        (cd "$path" && xargs -0 touch -c)
    echo "已克隆母本缓存到 $path/target/debug（$((SECONDS - start)) 秒）"
}

check_build_dir() {
    local path="$1"
    if ! grep -q '^build-dir = "{workspace-root}/target"' "$path/.cargo/config.toml" 2>/dev/null; then
        echo "警告：$path/.cargo/config.toml 没有 build-dir = \"{workspace-root}/target\"，" \
            "构建会落到用户级的 build-dir，用不上克隆的缓存" >&2
    fi
}

command="${1:-}"
case "$command" in
    new)
        [ $# -ge 2 ] && [ $# -le 3 ] || { usage; exit 2; }
        path="$(worktree_path "$2")"
        start_point="${3:-fork}"
        [ ! -e "$path" ] || { echo "错误：$path 已存在" >&2; exit 1; }
        git -C "$main_root" worktree add "$path" -b "wt/$2" "$start_point"
        check_build_dir "$path"
        clone_cache "$path"
        ;;
    cache)
        [ $# -eq 2 ] || { usage; exit 2; }
        path="$(worktree_path "$2")"
        [ -d "$path" ] || { echo "错误：$path 不存在" >&2; exit 1; }
        check_build_dir "$path"
        clone_cache "$path"
        ;;
    rm)
        [ $# -ge 2 ] && [ $# -le 3 ] || { usage; exit 2; }
        [ "$2" != "$base_name" ] || { echo "错误：$base_name 是母本，不能用 rm 删" >&2; exit 1; }
        path="$(worktree_path "$2")"
        force=()
        if [ "${3:-}" = "--force" ]; then
            force=(--force)
        elif [ -n "${3:-}" ]; then
            usage
            exit 2
        fi
        git -C "$main_root" worktree remove ${force[@]+"${force[@]}"} "$path"
        if git -C "$main_root" show-ref --verify --quiet "refs/heads/wt/$2"; then
            git -C "$main_root" branch -d "wt/$2" ||
                echo "分支 wt/$2 还没合并，已保留；确认不要了再 git branch -D wt/$2"
        fi
        ;;
    base)
        [ $# -eq 1 ] || { usage; exit 2; }
        echo "母本：$base_dir -> $(cd "$base_dir" 2>/dev/null && pwd -P || echo '不存在')"
        [ -d "$base_dir" ] && du -sh "$base_dir/" | cut -f1
        ;;
    base-update)
        [ $# -le 2 ] || { usage; exit 2; }
        path="$worktrees_dir/$base_name"
        [ -d "$path" ] || { echo "错误：母本 worktree $path 不存在，设置见设计文档 15" >&2; exit 1; }
        # _base 只用来编母本，不在里面改代码，强制切换。
        git -C "$path" checkout --force --detach "${2:-fork}"
        check_build_dir "$path"
        (cd "$path" && cargo build -p zed)
        ;;
    -h | --help | "")
        usage
        ;;
    *)
        echo "错误：未知命令 $command" >&2
        usage
        exit 2
        ;;
esac
