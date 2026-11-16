"""自己构造 GetTab 请求（不经过 LS，不重放）。结构与官方 LS 1.48.2 抓包一致，见 结论.md。

这是阶段 4 Rust 实现的参照模型：
  TabSession    一个编辑会话：维护用户轨迹（编辑步骤）、轨迹缓存哈希、user_jwt
  edit_step()   一次编辑 → CORTEX_STEP_TYPE_CODE_ACTION 步骤（行级 unified diff，上下文 ±10 行）
  build()       当前文档 + 光标 → GetTabRequest
  send()        流式发送，拼出最终预测；遇到 MissingInformation 时清缓存、发全量重试一次
"""
import base64
import difflib
import json
import time
import uuid

import cloud
import common
import identity

INSTRUCTION = ("Please modify the following mentioned code block with the logical next edit. "
               "The <|cursor|> in the code block represents where my cursor is.")
MODEL = "MODEL_TAB_EXPERIMENTAL_4"  # 来自 LS 的 SUPERCOMPLETE_MODEL_CONFIG 实验值；其他值服务端报错
CONTEXT_LINES = 10   # 编辑步骤 diff 的上下文行数（LS 实测：变更行前 10 行）
SEL_RADIUS = 5       # command_intent_info 选区：[row-5, row+6)，去掉首尾空行，按文件边界截断（LS 实测，unleash SUPERCOMPLETE_LINE_RADIUS）

LANG = {"python": "LANGUAGE_PYTHON", "rust": "LANGUAGE_RUST", "typescript": "LANGUAGE_TYPESCRIPT",
        "javascript": "LANGUAGE_JAVASCRIPT", "go": "LANGUAGE_GO", "plaintext": "LANGUAGE_PLAINTEXT"}


def enum(full, name):
    return common.pool().FindEnumTypeByName(full).values_by_name[name].number


def utf8_offset(text, char_offset):
    return len(text[:char_offset].encode("utf-8"))


def row_col(text, char_offset):
    """行从 0 开始；列按 UTF-8 字节（待核实，见 结论.md 问题 15）。"""
    before = text[:char_offset]
    row = before.count("\n")
    col = len(before[before.rfind("\n") + 1:].encode("utf-8"))
    return row, col


def jwt_exp(jwt):
    try:
        p = jwt.split(".")[1]
        return json.loads(base64.urlsafe_b64decode(p + "=" * (-len(p) % 4))).get("exp", 0)
    except Exception:
        return 0


def edit_step(uri, old_text, new_text, original_content=None):
    """一次编辑 → CODE_ACTION 步骤。original_content：该文件在本轨迹里的第一步才带（编辑前全文）。"""
    a, b = old_text.split("\n"), new_text.split("\n")
    sm = difflib.SequenceMatcher(a=a, b=b, autojunk=False)
    ops = [o for o in sm.get_opcodes() if o[0] != "equal"]
    if not ops:
        return None
    first_new, last_new = ops[0][3], ops[-1][4]
    first_old, last_old = ops[0][1], ops[-1][2]
    start = max(0, first_new - CONTEXT_LINES)
    end = min(len(b) - 1, max(last_new, first_new) + CONTEXT_LINES)
    # 行列表：用旧文本前缀上下文 + 变更 + 新文本后缀上下文
    lines = []
    U, I, D = (enum("exa.diff_action_pb.UnifiedDiffLineType", "UNIFIED_DIFF_LINE_TYPE_" + n)
               for n in ("UNCHANGED", "INSERT", "DELETE"))
    old_start = first_old - (first_new - start)
    for tag, i1, i2, j1, j2 in sm.get_opcodes():
        for k in range(i1, i2):
            if tag == "equal":
                j = j1 + (k - i1)
                if start <= j <= end:
                    lines.append((U, a[k]))
            else:
                if old_start <= k:
                    lines.append((D, a[k]))
        if tag != "equal":
            for k in range(j1, j2):
                lines.append((I, b[k]))
    st = common.new("exa.cortex_pb.CortexTrajectoryStep")
    st.type = enum("exa.cortex_pb.CortexStepType", "CORTEX_STEP_TYPE_CODE_ACTION")
    st.status = enum("exa.cortex_pb.CortexStepStatus", "CORTEX_STEP_STATUS_DONE")
    st.metadata.created_at.GetCurrentTime()
    st.metadata.source = enum("exa.cortex_pb.CortexStepSource", "CORTEX_STEP_SOURCE_USER_EXPLICIT")
    lr = st.code_action.action_spec.command.line_range
    lr.absolute_uri, lr.start_line, lr.end_line = uri, start, end
    ed = st.code_action.action_result.edit
    ed.absolute_uri = uri
    ed.diff.start_line, ed.diff.end_line = start, end
    for t, s in lines:
        ln = ed.diff.unified_diff.lines.add()
        ln.type = t
        if s:
            ln.text = s
    if original_content is not None:
        ed.original_content = original_content
    return st


class TabResult:
    def __init__(self):
        self.path = ""
        self.old_str = ""
        self.new_str = ""
        self.cursor = None
        self.jump = None
        self.noop = False
        self.stop_reason = None
        self.pre = None
        self.t_first = None
        self.t_done = None
        self.n_deltas = 0
        self.missing = None


class TabSession:
    def __init__(self, api_key, workspace_uri):
        self.api_key = api_key
        self.workspace_uri = workspace_uri
        self.mainline_id = str(uuid.uuid4())   # 用户主轨迹 id，即服务端缓存键 cache_key
        self.steps = []                        # 完整轨迹
        self.cached_hashes = []                # 服务端上次回传的 cache_info_v2.hashes
        self.seen_uris = set()
        self.jwt = ""
        self.jwt_exp = 0
        self.full_every_time = False

    # ---- 认证：api_key → user_jwt（15 分钟），过期前 60 秒刷新 ----
    def metadata(self, trigger_id):
        if not self.jwt or time.time() > self.jwt_exp - 60:
            req = common.new("exa.auth_pb.GetUserJwtRequest")
            req.metadata.CopyFrom(identity.metadata(self.api_key))
            r = cloud.unary(cloud.API_SERVER, "/exa.auth_pb.AuthService/GetUserJwt", req,
                            "exa.auth_pb.GetUserJwtResponse")
            self.jwt, self.jwt_exp = r.user_jwt, jwt_exp(r.user_jwt)
        md = identity.metadata(self.api_key, trigger_id=trigger_id)
        md.user_jwt = self.jwt
        md.ls_timestamp.GetCurrentTime()
        return md

    # ---- 轨迹 ----
    def record_edit(self, uri, old_text, new_text):
        orig = None if uri in self.seen_uris else old_text
        self.seen_uris.add(uri)
        st = edit_step(uri, old_text, new_text, orig)
        if st is not None:
            self.steps.append(st)

    # ---- 请求 ----
    def build(self, uri, text, cursor, language, other_documents=(), diagnostics=()):
        trigger_id = str(uuid.uuid4())
        req = common.new("exa.api_server_pb.GetTabRequest")
        req.metadata.CopyFrom(self.metadata(trigger_id))
        req.provider_source = enum("exa.codeium_common_pb.ProviderSource", "PROVIDER_SOURCE_SUPERCOMPLETE")
        req.prompt_id = str(uuid.uuid4())
        req.language = enum("exa.codeium_common_pb.Language", LANG.get(language, "LANGUAGE_PLAINTEXT"))
        req.model = enum("exa.codeium_common_pb.Model", MODEL)
        req.streaming_completion_version = 2
        e = req.experiment_config.experiments.add()
        e.key_string = "SUPERCOMPLETE_MODEL_CONFIG"
        e.json = '{ "model_name": "%s" }' % MODEL
        upc = req.unified_prompt_components
        ct = upc.cortex_trajectory
        ct.trajectory_id = str(uuid.uuid4())
        ct.trajectory_type = enum("exa.cortex_pb.CortexTrajectoryType", "CORTEX_TRAJECTORY_TYPE_SUPERCOMPLETE")
        ct.metadata.created_at.GetCurrentTime()
        pr = ct.parent_references.add()
        pr.trajectory_id = self.mainline_id
        pr.trajectory_type = enum("exa.cortex_pb.CortexTrajectoryType", "CORTEX_TRAJECTORY_TYPE_USER_MAINLINE")
        pr.step_index = -1
        # 缓存：服务端回传的 hashes 是完整轨迹前缀时，只发之后的步骤；否则发全量
        n = len(self.cached_hashes)
        if not self.full_every_time and 0 < n <= len(self.steps):
            req.trajectory_steps_v2.hashes.extend(self.cached_hashes)
            req.trajectory_steps_v2.steps_after_hashes.extend(self.steps[n:])
        else:
            ct.steps.extend(self.steps)
        # 当前文档与光标
        row, _ = row_col(text, cursor)
        lines = text.split("\n")
        s0, s1 = max(0, row - SEL_RADIUS), min(len(lines), row + SEL_RADIUS + 1)
        while s0 < row and not lines[s0].strip():      # 去掉窗口首尾空行（与 LS 一致）
            s0 += 1
        while s1 - 1 > row and not lines[s1 - 1].strip():
            s1 -= 1
        ci = upc.command_intent_info
        ci.selection = "\n".join(lines[s0:s1])
        ci.instruction = INSTRUCTION
        ci.selection_start_line, ci.selection_end_line = s0, s1
        ci.request_source = enum("exa.codeium_common_pb.CommandRequestSource", "COMMAND_REQUEST_SOURCE_SUPERCOMPLETE")
        d = ci.input_document
        d.text = text
        d.editor_language = language
        d.cursor_offset = utf8_offset(text, cursor)
        d.line_ending = "\n"
        d.absolute_uri = uri
        d.workspace_uri = self.workspace_uri
        d.timestamp.GetCurrentTime()
        d.is_dirty = True
        for od in other_documents:
            upc.other_documents.add().CopyFrom(od)
        for dg in diagnostics:
            upc.code_diagnostics.add().CopyFrom(dg)
        upc.supercomplete_feedback_context.SetInParent()
        return req

    def send(self, req, _retry=True):
        res = TabResult()
        for dt, m in cloud.stream(cloud.INFERENCE_SERVER, "/exa.api_server_pb.ApiServerService/GetTab", req,
                                  "exa.api_server_pb.GetTabResponse"):
            kind = m.WhichOneof("message")
            if kind == "missing_information":
                res.missing = m.missing_information
            elif kind == "pre_response":
                res.pre = m.pre_response
                self.cached_hashes = list(m.pre_response.cache_info_v2.hashes)
            elif kind == "streaming_completion_v2":
                dtab = m.streaming_completion_v2
                if res.t_first is None:
                    res.t_first = dt
                res.n_deltas += 1
                if dtab.path:
                    res.path = dtab.path
                which = dtab.WhichOneof("delta")
                if which == "delta_supercomplete":
                    ds = dtab.delta_supercomplete
                    res.old_str += ds.delta_old_str
                    res.new_str += ds.delta_new_str
                    if ds.HasField("cursor_position"):
                        res.cursor = (ds.cursor_position.row, ds.cursor_position.col)
                elif which == "delta_tab_jump":
                    res.jump = (dtab.delta_tab_jump.cursor_position.row, dtab.delta_tab_jump.cursor_position.col)
                elif which == "delta_noop":
                    res.noop = True
                if dtab.stop_reason:
                    res.stop_reason = dtab.stop_reason
                res.t_done = dt
        if res.missing is not None and _retry:
            # 缓存失效：清掉哈希，发全量重试一次
            self.cached_hashes = []
            ct = req.unified_prompt_components.cortex_trajectory
            del ct.steps[:]
            ct.steps.extend(self.steps)
            req.ClearField("trajectory_steps_v2")
            return self.send(req, _retry=False)
        return res
