#!/usr/bin/env bash
# 检查 fork 对上游文件的改动是否都登记在 fork/HOOKS.md 里。
#
# 用法：fork/check-hooks.sh [基准]
#   基准默认是当前 HEAD 与 upstream/main 的分叉点；基于稳定版 tag 时传入 tag，例如 v1.30.1。
#
# 检查项：
#   1. 相对基准改动过的上游文件（包括未提交和未跟踪的），都在 HOOKS.md 的清单里。
#   2. 清单里改动过的 .rs / .toml 文件都带有 FORK: 标记。
#   3. 上游文件里出现 FORK: 标记的，都在清单里。
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

base="${1:-$(git merge-base HEAD upstream/main)}"
hooks_file="fork/HOOKS.md"

is_fork_owned() {
    case "$1" in
        fork/* | crates/fork_*/* | crates/settings_content/src/fork.rs | crates/git_ui/src/git_panel/fork_vscode_layout.rs | crates/git_ui/src/git_panel/fork_vscode_layout/*) return 0 ;;
        # 仓库规则要求的 PR 自查标记，每处改动都要带上，见 HOOKS.md。
        README.md) return 0 ;;
        # 新增 crate 带来的锁文件变化，变基冲突时重新生成即可，见 HOOKS.md。
        Cargo.lock) return 0 ;;
        *) return 1 ;;
    esac
}

# 清单：HOOKS.md 里形如 "| H<n> | `<路径>` |" 的行。
allowed_files="$(awk -F'`' '/^[|] H[0-9]+ [|] `/ { print $2 }' "$hooks_file" | sort -u)"
if [[ -z "$allowed_files" ]]; then
    echo "错误：没能从 $hooks_file 解析出挂钩清单" >&2
    exit 1
fi

is_allowed() {
    grep -qxF -- "$1" <<<"$allowed_files"
}

changed_files="$(
    {
        git -c core.quotepath=off diff --name-only "$base"
        git -c core.quotepath=off ls-files --others --exclude-standard
    } | sort -u
)"

failed=0
upstream_changed=()

while IFS= read -r file; do
    [[ -z "$file" ]] && continue
    is_fork_owned "$file" && continue
    upstream_changed+=("$file")
    if ! is_allowed "$file"; then
        echo "未登记的上游改动：$file"
        failed=1
        continue
    fi
    case "$file" in
        *.rs | *.toml)
            if [[ -f "$file" ]] && ! grep -q "FORK:" "$file"; then
                echo "挂钩文件缺少 FORK: 标记：$file"
                failed=1
            fi
            ;;
    esac
done <<<"$changed_files"

while IFS= read -r file; do
    [[ -z "$file" ]] && continue
    is_fork_owned "$file" && continue
    if ! is_allowed "$file"; then
        echo "带 FORK: 标记但不在清单里：$file"
        failed=1
    fi
done < <(git -c core.quotepath=off grep -l "FORK:" -- . ':(exclude)fork' ':(exclude)crates/fork_*' || true)

echo "基准：$(git rev-parse --short "$base")"
echo "改动的上游文件（${#upstream_changed[@]} 个）："
for file in ${upstream_changed[@]+"${upstream_changed[@]}"}; do
    echo "  $file"
done

if [[ "$failed" -ne 0 ]]; then
    echo "检查失败：请更新 $hooks_file 或撤销多余的上游改动" >&2
    exit 1
fi
echo "检查通过"
