# 探索：生成 commit 信息（Windsurf 与 Zed 自带）

> 本仓库是 zed-industries/zed 的个人 fork。整体设计见 `../0.整体设计.md`，Windsurf 协议背景见 `../2.Windsurf协议验证.md` 和 `../../spike/结论.md`。

目的：把 Windsurf（Devin）和 Zed 自带的「生成 commit 信息」各自怎么实现记全。照着本文就能复刻任意一个，不需要再翻源码或重新抓包。

调查日期 2026-11-16。版本：

| 对象 | 版本 |
|---|---|
| Zed 上游 | `053d91cb14`（2026-11-16，fork 分支的 merge-base） |
| Windsurf 客户端 | Devin.app，`windsurfVersion` 3.10.48 |
| Windsurf LS | `codeiumVersion` 1.48.2，抓包用同一构建的 `language_server_linux_arm` |

## 结论速览

| | Windsurf | Zed 自带 |
|---|---|---|
| 谁拼提示词 | 本地 LS（闭源 Go），扩展只传仓库路径 | `git_ui` crate 里的 `GitPanel` |
| 模型调用 | 云端 `ApiServerService/GetChatMessage`，固定 `MODEL_PRIVATE_2` | 任意 LanguageModel provider，用户在设置里选 |
| diff 来源 | 有暂存改动用暂存区，否则用未暂存改动；不含未跟踪文件 | 同左（`git diff --staged` / `git diff`） |
| 风格参考 | `git log` 最近 10 条（完整原文）；没有历史时改为要求 conventional commit | 最近 3 条 commit 的 message |
| 额外上下文 | 无 | 项目规则文件、用户全局 `AGENTS.md`、设置里的自定义指令、用户已写的标题行 |
| diff 截断 | 500 行上限，另有一个按 token 的上限（数值未知） | 20 000 字节：先截超长行，再按文件轮流丢 hunk |
| 输出 | 一次性返回 `summary` / `description`，扩展只把 `summary` 填进输入框 | 流式写入 commit 编辑器 |
| 限制 | 只对付费套餐开放，服务端执行；Free 账号返回 `permission_denied` | 需要配置一个已认证的 LLM provider |

---

## 一、Windsurf

### 1.1 调用链

```
扩展 (extension.js)
  │  LanguageServerService/GenerateCommitMessage { metadata, plan_info, repo_root_uri }
  ▼
本地 LS (commit_messages.go)
  │  1. 读 git：最近 10 条 log、diff
  │  2. 拼系统提示词 + 用户消息
  │  3. 调云端
  ▼
云端 ApiServerService/GetChatMessage（Connect 服务端流）
  │  流式返回 delta_text
  ▼
LS 拆成 summary / description → GenerateCommitMessageResponse
  （成功后 LS 再上报 RecordCommitMessageGeneration，见 1.7）
```

**出处**：

- 扩展侧：`extension.js` 中的 `GenerateCommitMessageController` 和 `initializeGenerateCommitMessage`。文件从 Devin.app 拷出来，本机在 `.agents/worktree/spike/fork/spike/.cache/extension.js`，用 `grep -o 'GenerateCommitMessageController.\{0,2000\}'` 就能看到。
- LS 侧：抓包加 LS 日志。LS 在 `commit_messages.go:101` 用 debug 级别把完整提示词打进日志（`[debug] Generating commit message with system prompt:\n%s\n\nRequest prompt:\n%s`）。模板原文都是 LS 二进制里的字符串常量，见 1.4。
- proto：`fork/spike/proto-out/proto/exa/language_server_pb/language_server.proto`（`GenerateCommitMessage*`、`CommitMessageData`）、`api_server_pb/api_server.proto`（`GetChatMessageRequest`、`RecordCommitMessage*`）。

### 1.2 扩展侧（IDE ↔ LS）

**请求** `exa.language_server_pb.LanguageServerService/GenerateCommitMessage`（unary）：

```proto
message GenerateCommitMessageRequest {
  exa.codeium_common_pb.Metadata metadata = 1;
  exa.codeium_common_pb.PlanInfo plan_info = 2;
  string repo_root_uri = 3;          // 例如 "file:///path/to/repo"
}

message GenerateCommitMessageResponse {
  repeated CommitMessageData commit_messages = 1;
}

message CommitMessageData {
  string repo_root = 1;              // 文件系统路径（不是 URI）
  string commit_message_summary = 2;
  string commit_message_description = 3;
  repeated string changed_files_migrate_me_to_uri = 4 [deprecated = true];
  repeated string changed_file_uris = 5;
}
```

**扩展的行为**（从 `extension.js` 反混淆）：

1. 命令 `GENERATE_COMMIT_MESSAGE`：没有传仓库时，取 VS Code git API 的仓库（多个仓库时取最后遍历到的那个）。先执行 `workbench.view.scm`，再调 controller。
2. 先取消上一次生成（`AbortController`）。
3. 若 `planInfo.canGenerateCommitMessages` 为假，弹出 `"AI commit message generation is only available for users on a paid plan."` 和按钮 `Upgrade Plan`，到此结束。
4. 把 SCM 面板的 `generateCommitMessageButton.isGenerating` 设为 true，然后调 LS：`{ metadata, planInfo, repoRootUri }`。
5. 在响应的 `commitMessages` 里找 `Uri.file(repoRoot) == 当前仓库 rootUri` 的那一条，取它的 `commitMessageSummary`。找不到就用第一条，并上报一个 `isSilent` 的错误事件。为空字符串时抛 `"Generated message is empty"`。
6. **只把 `commitMessageSummary` 赋给 `inputBox.value`**（整体替换，不是追加），`description` 不用。
7. 用户取消时上报 `WS_CANCELED_GENERATE_COMMIT_MESSAGE`，其他错误上报 `WS_GENERATE_COMMIT_MESSAGE_ERRORED` 并弹错误提示。另外还有取消命令 `CANCEL_GENERATE_COMMIT_MESSAGE`。

**提交后的上报**（`initializeCommitTelemetry`）：扩展对**每个仓库的每次 commit**（`onDidCommit`）都调用一次 LS 的 `RecordCommitMessageSave`，不管 message 是不是生成的。上报内容包括 `repo_root`、`branch_name`、`commit_hash`、`commit_message`（完整 message）、`commit_timestamp`、`parent_commit_hash`（多个父提交时取第一个）、`author_name`、`author_email`。LS 再转发到云端同名接口。复刻时这一步可以不做；要做的话要考虑隐私。

### 1.3 LS 读取 git 的规则（实测）

用 1.8 的脚本按场景构造仓库，看 LS 实际发出的提示词：

| 场景 | 仓库状态 | 提示词里的 diff | 提示词里的历史 |
|---|---|---|---|
| `unstaged` | 1 条历史；`vector.py` 改动未暂存；`README.md` 未跟踪 | `vector.py` 的改动 | 1 条 |
| `staged` | 同上，但 `vector.py` 已暂存 | `vector.py` 的改动 | 1 条 |
| `mixed` | 2 条历史（第 2 条有正文）；`vector.py` 已暂存；`other.py` 已跟踪、改动未暂存；`new.txt` 未跟踪 | **只有** `vector.py` | 2 条，带正文 |
| `nohistory` | 没有任何 commit；`other.py` 已暂存（新文件） | `other.py`（`new file mode`） | 没有，换成 conventional commit 段 |

由此得出：

- **diff**：有暂存改动时只用暂存区（相当于 `git diff --cached`），没有暂存改动时用未暂存改动（`git diff`）。未跟踪文件从不包含。没有暂存改动时，无法从实测区分 LS 用的是 `git diff` 还是 `git diff HEAD`，两者在这些场景下结果相同。diff 是 git 默认格式（`diff --git a/… b/…`、`index …`、`---`/`+++`、`@@`），不带颜色。
- **历史**：`git log -n 10` 的默认格式原样放进去，条目之间空一行，正文缩进 4 个空格：

  ```
  commit 272ff484357000bae36d72404e4514f33b391268
  Author: spike <spike@example.com>
  Date:   Mon Nov 16 23:49:20 2026 -0500

      Second commit

      With a body line.

  commit c0b7d2029f06615f75486e7365a6dc9e7d8ae06b
  Author: spike <spike@example.com>
  Date:   Mon Nov 16 23:49:20 2026 -0500

      Add vector sample
  ```

  模板里写的是 "the last 10 commit messages"，取 10 条是按文案推断的，实测仓库最多只有 2 条。

- **截断**（3 组大 diff 实测）：

  | 场景 | 构造 | 实际放进提示词的 diff | 结尾 |
  |---|---|---|---|
  | `huge2` | 1 个新文件，20 000 行短行（`"0\n"`…） | 正好 500 行，2528 字节 | 追加 `"\n\n... (output truncated after 500 lines)"` |
  | `huge` | 12 个新文件 × 400 行 × 约 80 字节（`line N of file M xxx…`） | 146 行，11 311 字节，只有第一个文件的一部分 | 在行中间截断，没有任何标记 |
  | `huge3` | 1 个新文件 × 300 行 × 30 个随机英文单词 | 112 行，20 779 字节 | 在行中间截断，没有任何标记 |

  所以有两层截断：先按行截到 500 行（截断标记字符串在二进制里是 `"\n\n... (output truncated after %d lines)"`，和 Cascade 工具输出共用），再按某种 token 预算硬截断。后两组字节数差很多，说明不是按字节截的。具体预算和用的 tokenizer 没测出来。复刻时可以近似为「500 行，再加约 3–4K token 的上限」。

### 1.4 提示词（逐字）

LS 发给云端的 `GetChatMessageRequest` 有两段文本：`prompt` 字段是系统提示词，`chat_message_prompts[0]` 是用户消息。

**系统提示词**由三段拼接，段与段之间用 `"\n\n"` 连接。

A. 固定前缀，二进制偏移约 21144434，原文（注意 `"Here's"` 里的直引号）：

```text
Write a commit message for the provided git diff. Follow the commit message style from the repository history when available.

Only describe the +/- changes. Output as plain text without markdown formatting.

Output only the commit message itself. Do not include any preamble, explanation, or commentary such as "Based on the diff..." or "Here's the commit message:".
```

B1. 有历史时追加，二进制偏移约 21139114，`%s` 是 1.3 的 log 文本。注意 `match that: ` 末尾有一个空格，`</commit_history>` 后面紧跟一个空格再接 `If`：

```text
Here are the last 10 commit messages from this repository for reference. Follow the same style and format used in this project. Pay attention to the verbosity and level of detail and match that: 
<commit_history>
%s
</commit_history>
 If the history commit messages are only one line then you should keep your commit message to one line as well.
```

B2. 没有历史（仓库没有 commit）时改为追加这一段，二进制偏移约 21138087：

```text
Use conventional commit format:
- feat: New feature or user-facing change
- fix: Bug fix
- refactor: Code restructuring without changing external behavior
- docs: Documentation changes
- style: Code formatting (no logic change)
- perf: Performance improvements
- test: Adding or updating tests
- chore: Maintenance tasks, dependency updates
```

用伪代码写就是：

```text
system = A + "\n\n" + (history.is_empty() ? B2 : B1.format(history))
```

用 `repr` 核对过的实际值（`nohistory` 场景）：

```python
'Write a commit message for the provided git diff. Follow the commit message style from the repository history when available.\n\nOnly describe the +/- changes. Output as plain text without markdown formatting.\n\nOutput only the commit message itself. Do not include any preamble, explanation, or commentary such as "Based on the diff..." or "Here\'s the commit message:".\n\nUse conventional commit format:\n- feat: New feature or user-facing change\n- fix: Bug fix\n- refactor: Code restructuring without changing external behavior\n- docs: Documentation changes\n- style: Code formatting (no logic change)\n- perf: Performance improvements\n- test: Adding or updating tests\n- chore: Maintenance tasks, dependency updates'
```

结尾没有换行。有历史的版本以 `...one line as well.` 结尾，同样没有换行。

**用户消息**（二进制偏移约 20998178，`%s` 是 diff 原文，diff 末尾自带换行）：

```text
Generate a commit message that summarizes the changes in the following git diff:

%s
```

即 `"Generate a commit message that summarizes the changes in the following git diff:\n\n" + diff`，前缀共 82 字节。

### 1.5 云端请求（逐字段）

`POST https://server.codeium.com/exa.api_server_pb.ApiServerService/GetChatMessage`

- 走的是 **API 地址（`server.codeium.com`）**，不是 Tab / Tab Jump 用的推理地址（`inference…`）。
- Connect 服务端流。请求头为 `Content-Type: application/connect+proto`、`Connect-Protocol-Version: 1`、`Connect-Content-Encoding: gzip`、`Connect-Accept-Encoding: gzip`，`User-Agent: connect-go/1.18.1 (go1.26.6)`。
- 发送前 LS 会先调一次 `AuthService/GetUserJwt`，把 jwt 填进 `metadata.user_jwt`。

```jsonc
{
  "metadata": {
    "ide_name": "windsurf", "ide_version": "3.10.48",
    "extension_name": "windsurf", "extension_version": "1.48.2",
    "extension_path": "…", "locale": "en", "os": "{…json…}", "hardware": "{…json…}",
    "api_key": "…", "user_jwt": "…", "session_id": "<uuid>", "request_id": "7",
    "ls_timestamp": "…", "device_fingerprint": "…", "plan_name": "…", "id": "…", "f": "…"
    // 与 GetTab 的 metadata 同一套，构造方式见 crates/fork_windsurf/src/native.rs 的 metadata()
  },
  "prompt": "<1.4 的系统提示词>",
  "chat_message_prompts": [
    { "source": "CHAT_MESSAGE_SOURCE_USER", "prompt": "<1.4 的用户消息>" }
  ],
  "internal_chat_model": "MODEL_PRIVATE_2",            // 枚举值 220（字段 6，proto 里标了 deprecated，但 LS 仍在用）
  "request_type": "CHAT_MESSAGE_REQUEST_TYPE_GENERAL",  // 1
  "configuration": {
    "num_completions": 1,
    "max_tokens": 1024,
    "max_newlines": 200,
    "temperature": 0.4,
    "first_temperature": 0.4,
    "top_k": 50,
    "top_p": 1.0,
    "stop_patterns": ["<|user|>", "<|bot|>", "<|context_request|>", "<|endoftext|>", "<|end_of_turn|>"],
    "fim_eot_prob_threshold": 1.0
  }
  // 以下字段都没有设置：provider_source、experiment_config、chat_model_uid、
  // chat_model_name、use_internal_chat_model、prompt_id、tools 等
}
```

和 Tab Jump 的 `GetChatMessage` 比（见 `../5.Windsurf完整上下文.md`）：commit 生成不设 `provider_source`、不带 `experiment_config`，`request_type` 是 `GENERAL` 而不是 `COMMAND`，还多一个 `max_newlines`，模型和地址也不同。

**响应**是 `GetChatMessageResponse` 流：累加 `delta_text`，用 `stop_reason` 判断结束。累加逻辑可以直接复用 `crates/fork_windsurf/src/native/get_chat_message.rs` 的 `ChatStream::push`。

**Free 账号实测**：流的结束帧带错误：

```json
{"_end": {"error": {"code": "permission_denied",
  "message": "This model is no longer available for your account. Please switch to a different model."}}}
```

LS 收到后对 IDE 返回 HTTP 500 `{"code":"unknown","message":"failed to generate commit message: error grabbing LLM response: stream error"}`。同一账号的 `GetUserStatus` 显示 `plan_name: "Free"`、`teams_tier: TEAMS_TIER_DEVIN_FREE`，`PlanInfo.can_generate_commit_messages`（字段 25）没有设置。

### 1.6 未验证：成功时的输出与拆分

本次只有 Free 账号，**没有拿到成功的响应**，下面是推断，需要用付费账号补测：

- LS 怎么把模型输出拆成 `commit_message_summary` / `commit_message_description`：推测第一行是 summary，空行之后是 description。扩展只用 summary，所以在 Windsurf 里用户看到的永远是一行。
- `changed_file_uris` 推测是本次 diff 涉及的文件。
- 成功后 LS 会调云端 `ApiServerService/RecordCommitMessageGeneration`。按 proto，上报内容包括 `generation_duration_ms`、`system_prompt`、`full_prompt`、`prompt_tokens`、`completion_tokens`、`repo_root`、`branch_name`、`latest_commit_hash`、`num_files_changed`、`num_lines_added`、`num_lines_deleted` 和 summary/description。这一步复刻时可以不做。

补测方法：把 `.env` 换成付费账号的 key，跑 1.8 的脚本，看 `GenerateCommitMessage` 的响应和后续云端调用。

### 1.7 复刻要点

- **最省事**：已有 LS 桥接后端（`crates/fork_windsurf/src/language_server.rs`），直接调 `GenerateCommitMessage { repo_root_uri }`，读 `commit_messages[0]`。git 读取、截断、提示词都由 LS 完成。
- **原生直连**：按 1.3 取 diff 和 log，按 1.4 拼提示词，按 1.5 发 `GetChatMessage`。注意地址是 API 地址（`server.codeium.com`），可复用 `native/connect.rs` 和 `ChatStream`。模型名建议做成可配置，因为官方换模型时不用改代码，Tab 也是这么做的。
- 套餐限制在服务端执行，Free 账号走哪条路都是 `permission_denied`。

### 1.8 怎么复现这些结论

需要 spike 环境：`fork/spike/.env` 里有 key，`.cache/linux/...` 里有 Linux 版 LS，在 Linux 容器里运行。LS→云端那一段靠 `lib/mitm.py` 做 TLS 终止才能抓到，见 `../../spike/结论.md`「两段抓包的做法」。

脚本放在 `fork/spike/` 以外的任意位置，用 spike 的 venv 运行，`PYTHONPATH` 指向 `fork/spike/lib`：

```python
# commit_probe.py <场景>
import os, subprocess, sys, time
import common, ls_driver

api_key = common.require_api_key()
mode = sys.argv[1]
ws = ls_driver.prepare_workspace("ws-commit-" + mode)   # .lsrun/ 下的独立 git 仓库，带 vector.py
g = lambda *a: subprocess.run(["git", "-C", ws, *a], check=True, capture_output=True)
g("config", "user.email", "spike@example.com"); g("config", "user.name", "spike")
w = lambda name, text: open(os.path.join(ws, name), "w").write(text)
if mode == "mixed":
    w("other.py", "x = 1\n"); g("add", "-A"); g("commit", "-qm", "Add vector sample")
    g("commit", "-q", "--allow-empty", "-m", "Second commit\n\nWith a body line.")
    s = open(os.path.join(ws, "vector.py")).read()
    w("vector.py", s.replace("def add(a, b):", "def sub(a, b):\n    return a\n\n\ndef add(a, b):"))
    g("add", "vector.py")
    w("other.py", "x = 2  # unstaged\n")
    w("new.txt", "untracked\n")
elif mode == "nohistory":
    w("other.py", "x = 1\n"); g("add", "other.py")
elif mode == "huge2":
    g("add", "-A"); g("commit", "-qm", "init")
    w("short.txt", "".join("%d\n" % n for n in range(20000))); g("add", "-A")
# …其他场景按 1.3 的表构造

sess_dir = os.path.join(ls_driver.CAPTURES, time.strftime("%Y%m%d-%H%M%S") + "-commit-" + mode)
sess = ls_driver.LSSession(sess_dir, ws, api_key).start()   # Linux 上自动启用 mitm
try:
    ls_driver.refresh_startup(sess); time.sleep(3)
    try:
        print(sess.call("GenerateCommitMessage", "GenerateCommitMessageRequest",
                        timeout=90, repo_root_uri="file://" + ws))
    except Exception as e:
        print("ERR", e)
    time.sleep(4)
finally:
    sess.stop()
print(sess_dir)
```

```sh
cd fork/spike && source lib/bootstrap.sh
"$PY" /path/to/commit_probe.py mixed
./run decode captures/<时间戳>-commit-mixed --full --no-trunc --path GetChatMessage   # 云端请求与响应
grep -A40 commit_messages.go captures/<时间戳>-commit-mixed/ls.log                   # LS 自己打的提示词
```

模板原文从 LS 二进制里搜：

```python
import re
b = open("language_server_linux_arm", "rb").read()
for p in [b"Write a commit message for the provided", b"<commit_history>",
          b"Use conventional commit format", b"Generate a commit message that summarizes",
          b"output truncated after"]:
    m = re.search(re.escape(p), b); print(m.start(), b[m.start()-50:m.end()+600].decode("latin1"))
```

本次的抓包记录在本机 spike worktree 的 `fork/spike/captures/20261116-23*-commit-*`。按 `.gitignore` 不提交，里面有打码前的凭据。

---

## 二、Zed 自带

截图里那条 "Configure an LLM provider to generate commit messages." 是生成按钮的配置错误提示：没有可用的 commit 模型或 provider 没认证时，按钮变灰，悬停显示这条提示。

代码全部在 `crates/git_ui/src/git_panel.rs`（行号以上游 `053d91cb14` 为准），提示词在 `crates/git_ui/src/commit_message_prompt.txt`。

### 2.1 入口与 UI

| 项 | 位置 | 说明 |
|---|---|---|
| 动作 | `git::GenerateCommitMessage`（`crates/git/src/git.rs:132`） | git 面板和 commit 弹窗（`commit_modal.rs:622`）都注册了处理函数，最终调 `GitPanel::generate_commit_message` |
| 键位 | macOS `alt-tab`，Linux/Windows `alt-l` | context 为 `CommitEditor > Editor` 和 `GitCommit > Editor && mode == auto_height` |
| 按钮 | `render_generate_commit_message_button`（`:6840`） | 图标 `IconName::AiEdit`。`AgentSettings.enabled(cx)` 为假时（`agent.enabled = false` 或 `disable_ai`）不显示 |
| 禁用条件 | 同上 | `!can_commit()`（没有暂存或已跟踪的改动，或有未解决的冲突，见 `:6744`）时提示 "No Changes to Commit"；有配置错误时显示截图里的 `GenerateCommitMessageConfigurationTooltip`（`:9876`），按钮分别打开设置的 `llm_providers` 页和 provider 文档 |
| 生成中 | 同上 | 按钮换成红色 `Stop` 图标和 "Generating Commit…"，点 Stop 就丢弃 `generate_commit_message_task`，任务随之取消 |

配置错误由 `LanguageModelRegistry::configuration_error(commit_message_model(cx))` 判断（`crates/language_model/src/registry.rs:266`），三种情况：一个已认证的 provider 都没有（`NoProvider`）、找不到模型（`ModelNotFound`）、provider 未认证（`ProviderNotAuthenticated`）。

### 2.2 模型选择

`LanguageModelRegistry::commit_message_model`（`registry.rs:502`）：

```rust
self.commit_message_model.clone()        // 设置 agent.commit_message_model
    .or_else(|| self.default_fast_model(cx))
    .or_else(|| self.default_model())
```

`agent.commit_message_model` 由 `agent_ui` 在设置变化时调 `select_commit_message_model` 写入（`crates/agent_ui/src/agent_ui.rs:1035`），格式和其他模型设置一样：`{ "provider": "...", "model": "..." }`。

温度：`AgentSettings::temperature_for_model`，按 `agent.model_parameters` 从后往前找第一条匹配 provider/model 的 `temperature`，找不到为 `None`（用 provider 默认值）。

### 2.3 相关设置

```jsonc
"agent": {
  "enabled": true,                                  // 为 false 时整个功能隐藏
  "commit_message_model": { "provider": "anthropic", "model": "…" },  // 可选
  "commit_message_include_project_rules": true,     // 默认 true
  "commit_message_instructions": "…",               // 可选，自定义指令
  "model_parameters": [{ "provider": "…", "model": "…", "temperature": 0.2 }]
}
```

### 2.4 流程（`generate_commit_message`，`:4724`）

1. **前置检查**：`can_commit()` 且 agent 已启用，并且能拿到 commit 模型和它的 provider，否则直接返回（拿不到 provider 时弹 `Failed to generate commit message: …`）。
2. 记一个遥测事件 `Git Commit Message Generated`。
3. **diff**：`has_staged_changes()` 为真时用 `DiffType::HeadToIndex` → `git diff --staged`，否则用 `DiffType::HeadToWorktree` → `git diff`（`crates/git/src/repository.rs:2532`）。注意 `git diff` 比较的是暂存区和工作区，名字虽叫 HeadToWorktree，但不是 `git diff HEAD`。不含未跟踪文件。
4. **最近 commit**：`RECENT_COMMIT_COUNT = 3`（`:120`），依次 `repo.show("HEAD")`、`show("HEAD~1")`、`show("HEAD~2")`，取 `CommitDetails.message`（完整 message，含正文）。失败的跳过，只有第 0 条失败时打 debug 日志。
5. 后台任务里：provider 未认证就先 `authenticate` 一次（失败只记日志）。
6. diff 出错就弹 `Failed to generate commit message: {err}`，然后结束。
7. **压缩 diff**：`compress_commit_diff(diff, 20_000)`，见 2.6。
8. **项目规则**（`commit_message_include_project_rules` 为真时）：
   - `load_project_rules`（`:4619`）：在仓库工作目录下的 worktree 里，按 `RULES_FILE_NAMES` 的顺序找第一个存在的文件（`crates/prompt_store/src/prompts.rs:22`）：`.rules`、`.cursorrules`、`.windsurfrules`、`.clinerules`、`.github/copilot-instructions.md`、`AGENT.md`、`AGENTS.md`、`CLAUDE.md`、`GEMINI.md`。读出后 `trim()`，为空当作没有。
   - 用户全局 `AGENTS.md`：`UserAgentsMd` global，文件位置是 `paths::agents_file()` = `<config_dir>/AGENTS.md`（macOS/Linux 上是 `~/.config/zed/AGENTS.md`）。
9. **subject**：commit 编辑器当前文本的第一行，没有时为空字符串。
10. 拼提示词（2.5），组成请求：

    ```rust
    LanguageModelRequest {
        intent: Some(CompletionIntent::GenerateGitCommitMessage),
        messages: vec![LanguageModelRequestMessage { role: Role::User, content: vec![content.into()], cache: false, .. }],
        tools: vec![], tool_choice: None, stop: vec![],
        temperature,                 // 2.2
        thinking_allowed: false, thinking_effort: None,
        max_output_tokens: None,
        thread_id/prompt_id/prompt_cache_key/speed/compact_at_tokens: None,
        ..
    }
    ```

    **只有一条 user 消息，没有 system 消息**，指令全部放在 user 消息里。`intent` 只有 Copilot Chat provider 会用（`crates/copilot_chat/src/model.rs:107/1030`）。

11. `provider.stream_completion_text(&model, request, cx)`，流式写入：
    - subject 不为空时，先在 commit 编辑器末尾插一个 `"\n"`，再把生成的文本追加在后面。用户写的内容保留，生成的内容接在下一行。
    - subject 为空时直接追加（编辑器本来就是空的，或者第一行是空白）。
    - 每个文本块都插在 buffer 末尾；流中途出错就弹错误并停止，已写入的内容保留。
    - 不做任何后处理，不会去掉 markdown 或 `<think>` 之类的内容。

### 2.5 提示词（逐字）

`commit_message_prompt.txt`：

```text
You are an expert at writing Git commits. Your job is to write a short clear commit message that summarizes the changes.

If you can accurately express the change in just the subject line, don't include anything in the message body. Only use the body when it is providing *useful* information.

Don't repeat information from the subject line in the message body.

Only return the commit message in your response. Do not include any additional meta-commentary about the task. Do not include the raw diff output in the commit message.

Follow good Git style:

- Separate the subject from the body with a blank line
- Try to limit the subject line to 50 characters
- Capitalize the subject line
- Do not end the subject line with any punctuation
- Use the imperative mood in the subject line
- Wrap the body at 72 characters
- Keep the body short and concise (omit it entirely if not useful)
```

`build_commit_message_prompt`（`:4665`）的拼接方式，每一段都是可选的，缺省时为空字符串：

```rust
format!("{prompt}{user_agents_md_section}{rules_section}{instructions_section}{recent_commit_messages_section}{subject_section}\nHere are the changes in this commit:\n{diff_text}")
```

各段原文（`\n` 表示换行）：

```text
user_agents_md_section =
"\n\nThe user has provided the following rules that you should follow when writing the commit message. Project-specific rules may override these instructions when they conflict:\n<rules>\n{user_agents_md}\n</rules>\n"

rules_section =
"\n\nThe user has provided the following rules specific to this project that you should follow when writing the commit message:\n<project_rules>\n{rules}\n</project_rules>\n"

instructions_section   // instructions.trim() 非空时才加
"\n\nThe user has provided the following instructions for writing commit messages that you should follow:\n<commit_message_instructions>\n{instructions}\n</commit_message_instructions>\n"

recent_commit_messages_section   // 每条 message 先 trim()，再写成 "Commit {i+1}:\n{message}"，条目之间用 "\n\n---\n\n" 连接
"\n\nUse the following recent commit messages only as style examples. Keep the generated message consistent with them:\n<recent_commit_messages>\n{joined}\n</recent_commit_messages>\n"

subject_section        // subject.trim() 非空时才加
"\nHere is the user's subject line:\n{subject}"
```

注意：`commit_message_prompt.txt` 以一个 `"\n"` 结尾，`include_str!` 会保留它。所以各可选段（以 `"\n\n"` 开头）前面实际有两个空行；没有任何可选段时，`"\nHere are the changes in this commit:\n"` 前面有一个空行。

### 2.6 diff 压缩算法

`compress_commit_diff(diff_text, max_bytes = 20_000)`（`:4593`）：

1. 长度不超过 `max_bytes` 就原样返回。
2. 把超过 256 字节的行截成 `前 256 字节（向前对齐到字符边界）+ "...[truncated]"`，每行补回 `"\n"`。
3. 还超就调 `truncate_iteratively`（`:4548`）：
   - `split_patch`（`:4529`）：遇到以 `---` 开头的行，且当前块不为空时，切出一个新块。注意它按 `---` 切，不是按 `diff --git` 切，所以每块的「头」是 `--- a/x` 和 `+++ b/x` 两行，上一个文件的 `diff --git`/`index` 行会挂在前一块的末尾。
   - 每块用 `TruncatedPatch::from_unified_diff`（`:1176`）解析：前两行作为 header，之后按 `@@` 切 hunk。少于 2 行或没有 hunk 的块会被**丢弃**（如果所有块都被丢弃，就返回第 2 步的结果）。
   - 循环：找出 `hunks_to_keep > 1` 的块中 `hunks_to_keep` 最大的一个，减 1，重新计算总大小，直到不超过 `max_bytes`，或者所有块都只剩 1 个 hunk。
   - 输出：每块写 header 加保留的 hunk，丢过 hunk 的块末尾加 `"[...skipped N hunks...]\n"`，块之间用 `"\n"` 连接。
4. 每个文件至少保留 1 个 hunk，所以结果**可能仍然超过** `max_bytes`，没有最终的硬截断。

### 2.7 出处

全部来自上游源码，没有抓包：

- `crates/git_ui/src/git_panel.rs`：`RECENT_COMMIT_COUNT`（:120）、`TruncatedPatch`（:1176）、`generate_commit_message_action`（:4520）、`split_patch`（:4529）、`truncate_iteratively`（:4548）、`compress_commit_diff`（:4593）、`load_project_rules`（:4619）、`build_commit_message_prompt`（:4665）、`generate_commit_message`（:4724）、`show_commit_message_error`（:6675）、`can_commit`（:6744）、`render_generate_commit_message_button`（:6840）、`GenerateCommitMessageConfigurationTooltip`（:9876）
- `crates/git_ui/src/commit_message_prompt.txt`
- `crates/git_ui/src/commit_modal.rs`：:356–445（按钮）、:622（动作）
- `crates/language_model/src/registry.rs`：:266、:332、:435、:502
- `crates/agent_settings/src/agent_settings.rs`：:225–227、:264、:268
- `crates/settings_content/src/agent.rs`：:299–307；`assets/settings/default.json`：:1181
- `crates/prompt_store/src/prompts.rs`：:22
- `crates/git/src/repository.rs`：:2532
- `assets/keymaps/default-{macos,linux,windows}.json`：搜 `git::GenerateCommitMessage`

---

## 三、两者差异与 fork 接入方式

| 维度 | Windsurf | Zed |
|---|---|---|
| 消息结构 | system 加 user | 只有 user |
| 风格参考 | 10 条完整 `git log`（含 hash/作者/日期） | 3 条 message |
| 没有历史时 | 要求 conventional commit | 不处理 |
| 「一行」约束 | 历史都是一行时要求只写一行 | 能一行说清时不要正文 |
| 格式约束 | 纯文本、不用 markdown | 50/72 规则、祈使句、首字母大写 |
| 用户输入 | 无（覆盖输入框） | subject 会作为上下文，生成结果追加在后面 |
| 采样参数 | temperature 0.4，max_tokens 1024，max_newlines 200 | 温度由设置决定，不限 max tokens |
| 截断 | 500 行，再加 token 上限，硬截断 | 20 000 字节，截长行、丢 hunk |

fork 接入方式（详见对话里的可行性结论）：

- **A. LS 桥接**：调 `GenerateCommitMessage`。需要新增挂钩，接管 `GitPanel::generate_commit_message`。
- **B. 原生直连**：按第一部分复刻提示词，发 `GetChatMessage`。同样需要挂钩。
- **C. 注册一个 Windsurf 的 LanguageModel provider**：用户在 `agent.commit_message_model` 里选它，沿用 Zed 的提示词和 UI，不用改上游代码。

三种方式都受 1.5 的套餐限制。
