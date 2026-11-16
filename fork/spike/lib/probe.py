"""GetTab 字段替换实验（低频、串行，每次间隔 >= 2 秒）。

用法：run probe <实验名> [--session 抓包目录]
实验以最近一次抓到的 LS→云端 GetTab 为基础，每次只改一处，打印服务端结果（凭据打码）。
"""
import argparse
import json
import copy
import sys
import uuid

import capture_util
import cloud
import common
import identity

GT = "exa.api_server_pb.GetTabRequest"
GT_RESP = "exa.api_server_pb.GetTabResponse"


def fresh_jwt(md):
    req = common.new("exa.auth_pb.GetUserJwtRequest")
    req.metadata.CopyFrom(md)
    req.metadata.ClearField("user_jwt")
    req.metadata.ClearField("f")
    r = cloud.unary(cloud.API_SERVER, "/exa.auth_pb.AuthService/GetUserJwt", req, "exa.auth_pb.GetUserJwtResponse")
    return r.user_jwt


def base_request(session=None):
    """取最近一条「首次请求」形状的 GetTab（cortex_trajectory.steps 非空、无 hashes）。"""
    ss = [session] if session else reversed(capture_util.sessions())
    for s in ss:
        for c in capture_util.cloud_calls(s, "/GetTab"):
            p = capture_util.request_payloads(c)[0]
            m = common.msg_class(GT)()
            m.ParseFromString(p)
            if m.unified_prompt_components.cortex_trajectory.steps and not m.trajectory_steps_v2.hashes:
                return s, m
    sys.exit("captures/ 里没有 GetTab 抓包，先运行 run ls-tab（Linux 容器内）")


def run_once(req, label, url=cloud.INFERENCE_SERVER):
    out = []
    try:
        for dt, m in cloud.stream(url, "/exa.api_server_pb.ApiServerService/GetTab", req, GT_RESP):
            out.append((dt, m))
        kinds = []
        for dt, m in out:
            d = common.to_json(m)
            kinds.append("%.0fms:%s" % (dt * 1000, ",".join(sorted(d.keys()))))
        print("[%s] OK %d 条 %s" % (label, len(out), " ".join(kinds)[:400]))
        if VERBOSE:
            for dt, m in out:
                d = common.to_json(m)
                if "pre_response" in d and "prompt" in d["pre_response"]:
                    d["pre_response"]["prompt"] = d["pre_response"]["prompt"][:VERBOSE]
                print("   ", json.dumps(d, ensure_ascii=False)[:VERBOSE])
    except cloud.ConnectError as e:
        print("[%s] ERR %s %s" % (label, e.code, e.message))
    return out


EXPERIMENTS = {}
VERBOSE = 0


def exp(f):
    EXPERIMENTS[f.__name__] = f
    return f


@exp
def replay(req):
    """原样重放（只换新的 user_jwt）"""
    return req


@exp
def no_f(req):
    req.metadata.ClearField("f")
    return req


@exp
def no_jwt(req):
    req.metadata.ClearField("user_jwt")
    return req


@exp
def no_chat(req):
    del req.unified_prompt_components.chat_messages[:]
    return req


def active_doc_step(req, with_cursor_marker=True):
    """按 command_intent_info 里的选区，造一个 supercomplete_active_doc 轨迹步骤。"""
    ci = req.unified_prompt_components.command_intent_info
    doc = ci.input_document
    text = doc.text
    off = len(text.encode()[:doc.cursor_offset].decode("utf-8", "ignore"))
    lines = text.split("\n")
    s0, e0 = ci.selection_start_line, min(ci.selection_end_line, len(lines) - 1)
    t = text[:off] + ("<|cursor|>" if with_cursor_marker else "") + text[off:]
    sel = "\n".join(t.split("\n")[s0:e0 + 1])
    st = common.new("exa.cortex_pb.CortexTrajectoryStep")
    st.type = 86  # SUPERCOMPLETE_ACTIVE_DOC
    st.status = 3  # DONE
    st.metadata.source = 3  # USER_IMPLICIT
    st.metadata.created_at.GetCurrentTime()
    st.supercomplete_active_doc.instruction = ci.instruction
    st.supercomplete_active_doc.selection_with_cursor.content = sel
    src = st.supercomplete_active_doc.selection_with_cursor.source
    src.absolute_uri = doc.absolute_uri
    src.start_line = s0
    src.end_line = e0
    return st


@exp
def new_path(req):
    """去掉 chat_messages，trajectory_steps_v2 里放一个 active_doc 步骤"""
    del req.unified_prompt_components.chat_messages[:]
    req.trajectory_steps_v2.steps_after_hashes.append(active_doc_step(req))
    return req


@exp
def new_path_keep_chat(req):
    """保留 chat_messages，同时加 active_doc 步骤"""
    req.trajectory_steps_v2.steps_after_hashes.append(active_doc_step(req))
    return req


@exp
def steps_in_cortex(req):
    """active_doc 步骤同时放进 cortex_trajectory.steps 和 trajectory_steps_v2"""
    del req.unified_prompt_components.chat_messages[:]
    st = active_doc_step(req)
    req.unified_prompt_components.cortex_trajectory.steps.append(st)
    req.trajectory_steps_v2.steps_after_hashes.append(st)
    return req


@exp
def steps_only_cortex(req):
    """active_doc 步骤只放进 cortex_trajectory.steps"""
    del req.unified_prompt_components.chat_messages[:]
    req.unified_prompt_components.cortex_trajectory.steps.append(active_doc_step(req))
    return req


@exp
def no_command_intent(req):
    """去掉 command_intent_info"""
    req.unified_prompt_components.ClearField("command_intent_info")
    return req


@exp
def no_chat_no_intent(req):
    """同时去掉 chat_messages 和 command_intent_info"""
    del req.unified_prompt_components.chat_messages[:]
    req.unified_prompt_components.ClearField("command_intent_info")
    return req


@exp
def min_metadata(req):
    """metadata 只留 api_key、ide_name、ide_version、extension_version、user_jwt"""
    keep = {"api_key", "ide_name", "ide_version", "extension_version", "user_jwt"}
    for fd, _ in list(req.metadata.ListFields()):
        if fd.name not in keep:
            req.metadata.ClearField(fd.name)
    return req


@exp
def jwt_only(req):
    """metadata 只留 user_jwt（不带 api_key）"""
    jwt = req.metadata.user_jwt
    req.ClearField("metadata")
    req.metadata.user_jwt = jwt
    return req


@exp
def api_key_only(req):
    """metadata 只留 api_key（不带 user_jwt）"""
    k = req.metadata.api_key
    req.ClearField("metadata")
    req.metadata.api_key = k
    return req


@exp
def no_experiment_config(req):
    req.ClearField("experiment_config")
    return req


@exp
def no_model(req):
    req.ClearField("model")
    return req


@exp
def model_tab_base(req):
    req.model = 501  # MODEL_TAB_BASE_1
    return req


@exp
def no_provider_source(req):
    req.ClearField("provider_source")
    return req


@exp
def no_parent_ref(req):
    del req.unified_prompt_components.cortex_trajectory.parent_references[:]
    return req


@exp
def fake_hash(req):
    """把步骤挪到 trajectory_steps_v2，并带一个编造的哈希"""
    ct = req.unified_prompt_components.cortex_trajectory
    steps = list(ct.steps)
    del ct.steps[:]
    req.trajectory_steps_v2.hashes.append(1234567890123456789)
    req.trajectory_steps_v2.steps_after_hashes.extend(steps[1:])
    return req


@exp
def cache_followup(req):
    """先发全量拿到 cache_info_v2，再只发 hashes + 新步骤（模拟第二次触发）"""
    out = run_once(copy.deepcopy(req), "cache_followup/全量")
    info = [m.pre_response.cache_info_v2 for _, m in out if m.HasField("pre_response")]
    if not info:
        print("  没拿到 cache_info_v2")
        return None
    ct = req.unified_prompt_components.cortex_trajectory
    steps = list(ct.steps)
    del ct.steps[:]
    req.trajectory_steps_v2.hashes.extend(info[0].hashes)
    print("  cache_key=%s hashes=%d 个，步骤 %d 个" % (info[0].cache_key, len(info[0].hashes), len(steps)))
    # 新步骤：重复最后一步（内容一样、时间不同），模拟又敲了一个字
    st = copy.deepcopy(steps[-1])
    st.metadata.created_at.GetCurrentTime()
    req.trajectory_steps_v2.steps_after_hashes.append(st)
    req.metadata.trigger_id = str(uuid.uuid4())
    return req


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("names", nargs="*")
    ap.add_argument("--session")
    ap.add_argument("--list", action="store_true")
    ap.add_argument("-v", type=int, default=0, help="打印每条响应的前 N 个字符")
    a = ap.parse_args()
    global VERBOSE
    VERBOSE = a.v
    if a.list or not a.names:
        for k, f in EXPERIMENTS.items():
            print("%-24s %s" % (k, (f.__doc__ or "").strip()))
        return
    common.require_api_key()
    s, base = base_request(a.session)
    print("基础请求：%s" % s)
    jwt = fresh_jwt(base.metadata)
    print("新 user_jwt：%s" % common.mask(jwt))
    for n in a.names:
        r = copy.deepcopy(base)
        r.metadata.user_jwt = jwt
        r.metadata.trigger_id = str(uuid.uuid4())
        r.prompt_id = str(uuid.uuid4())
        r = EXPERIMENTS[n](r)
        if r is not None:
            run_once(r, n)


if __name__ == "__main__":
    main()
