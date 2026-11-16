"""抓包场景：自己驱动官方 LS（模拟 IDE 扩展的调用顺序），同时在容器里用 TLS 终止录制 LS → 云端。

一次运行只拉起一个 LS，按顺序跑各场景（轨迹会自然累积，和真实使用一致），场景之间插入 mark。
每次触发都带 trigger_id；两段流量用 trigger_id（IDE→LS 的 metadata.trigger_id = LS→云端的 metadata.trigger_id）和时间对应。

  run capture --list
  run capture                全部场景
  run capture first_open typing accept
"""
import json
import os
import time

import common
import identity
import ls_driver
from ls_driver import (CAPTURES, LSSession, prepare_workspace, refresh, refresh_startup, sel_changed, summarize_tab,
                       tab)

SHAPES_PY = '''from vector import Vector


def area(width, height):
    return width * height


def perimeter(width, height):
    return 2 * (width + height)
'''

UNICODE_LINE = '    label = "长度🙂" + str(v.len'


def change_types():
    e = common.pool().FindEnumTypeByName("exa.diff_action_pb.DiffChangeType")
    return {v.name: v.number for v in e.values}


def final_text(sess, rel, resp):
    """按 character_diff 拼出接受后的全文（old_str 覆盖的范围之外保持不变）。"""
    ct = change_types()
    u = sess.uri(rel)
    text = sess.texts[u]
    d = resp.diff
    new_part = "".join(c.text for c in d.character_diff.changes if c.type != ct.get("DIFF_CHANGE_TYPE_DELETE"))
    old = d.old_str
    i = text.find(old)
    if i < 0:
        return None
    return text[:i] + new_part + text[i + len(old):]


def _replace(sess, rel, old, new):
    # 求最小差异区间，发一次增量 didChange
    a = 0
    while a < min(len(old), len(new)) and old[a] == new[a]:
        a += 1
    b = 0
    while b < min(len(old), len(new)) - a and old[-1 - b] == new[-1 - b]:
        b += 1
    sess.edit(rel, a, len(old) - a - b, new[a:len(new) - b])
    return a + len(new) - a - b


def feedback(sess, resp, accepted, rel, intentional=False, source=6):
    sess.call("ProvideCompletionFeedback", "ProvideCompletionFeedbackRequest",
              completion_id=resp.completion_id, prompt_id=resp.prompt_id, is_accepted=accepted, source=source,
              document=_doc(sess, rel), feedback_delay_ms=800, is_intentional_reject=intentional)


def _doc(sess, rel):
    from ls_driver import document
    u = sess.uri(rel)
    return document(sess, rel, 0, sess.langs.get(u, "plaintext"))


def show(label, resp, dt):
    d = summarize_tab(resp)
    kind = resp.WhichOneof("suggestion")
    brief = {"kind": kind, "ms": round(dt * 1000)}
    if kind == "diff":
        ct = change_types()
        brief["insert"] = "".join(c.text for c in resp.diff.character_diff.changes if c.type == ct.get("DIFF_CHANGE_TYPE_INSERT"))[:120]
        brief["delete"] = "".join(c.text for c in resp.diff.character_diff.changes if c.type == ct.get("DIFF_CHANGE_TYPE_DELETE"))[:120]
    elif kind == "tab_jump":
        brief["jump"] = d.get("tab_jump")
    elif d.get("filter_reason"):
        brief["filter"] = d.get("filter_reason")
    print("[%s] %s" % (label, json.dumps(brief, ensure_ascii=False)))


# ---------------- 场景 ----------------

def s_first_open(sess):
    """打开文件后第一次触发（只移动光标，不编辑）"""
    rel = "vector.py"
    refresh(sess, rel, 0, 4)
    t = sess.texts[sess.uri(rel)]
    off = t.index("    w = add")
    refresh(sess, rel, off, 2)
    time.sleep(0.6)
    return [("first_open", tab(sess, rel, off, trigger=2, label="first_open"))]


def s_typing(sess):
    """连续输入：逐字输入一行，中途和末尾各触发一次"""
    rel = "vector.py"
    t = sess.texts[sess.uri(rel)]
    off = len(t)
    cb = sel_changed(sess, rel)
    off = sess.type_text(rel, off, "    u = v.sc", delay=0.12, on_char=cb)
    r1 = tab(sess, rel, off, label="typing_mid")
    off = sess.type_text(rel, off, "ale(", delay=0.12, on_char=cb)
    r2 = tab(sess, rel, off, label="typing_end")
    sess.state["typing_resp"] = r2[0]
    return [("typing_mid", r1), ("typing_end", r2)]


def s_accept(sess):
    """接受：把上一次的 Diff 应用到文档，上报 is_accepted=true，然后以 SUPERCOMPLETE_ACCEPT 触发下一次"""
    rel = "vector.py"
    resp = sess.state.get("typing_resp")
    out = []
    if resp is None or resp.WhichOneof("suggestion") != "diff":
        r = tab(sess, rel, len(sess.texts[sess.uri(rel)]), label="accept_pre")
        out.append(("accept_pre", r))
        resp = r[0]
    new = final_text(sess, rel, resp) if resp.WhichOneof("suggestion") == "diff" else None
    if new is None:
        print("  [accept] 没有可接受的 Diff，跳过")
        return out
    off = _replace(sess, rel, sess.texts[sess.uri(rel)], new)
    feedback(sess, resp, True, rel)
    out.append(("after_accept", tab(sess, rel, off, trigger=6, label="after_accept")))
    return out


def s_reject(sess):
    """拒绝：触发一次后按 Esc（is_intentional_reject），再输入继续"""
    rel = "vector.py"
    t = sess.texts[sess.uri(rel)]
    off = t.index("def add")
    off = sess.type_text(rel, off, "# helper\n", delay=0.1, on_char=sel_changed(sess, rel))
    r = tab(sess, rel, off, label="reject_pre")
    feedback(sess, r[0], False, rel, intentional=True)
    off = sess.type_text(rel, off, "#", delay=0.1)
    return [("reject_pre", r), ("after_reject", tab(sess, rel, off, label="after_reject"))]


def s_rename(sess):
    """改名后的联动修改：把 scale 的参数 factor 改成 k（只改定义处）"""
    rel = "vector.py"
    t = sess.texts[sess.uri(rel)]
    off = t.index("factor):")
    sess.edit(rel, off, len("factor"), "")
    off = sess.type_text(rel, off, "k", delay=0.1, on_char=sel_changed(sess, rel))
    return [("rename", tab(sess, rel, off, label="rename"))]


def s_jump(sess):
    """跳转：在文件顶部改类名 Vector → Vec，用处在远处"""
    rel = "vector.py"
    t = sess.texts[sess.uri(rel)]
    off = t.index("class Vector") + len("class Vec")
    sess.edit(rel, off, len("tor"), "")
    refresh(sess, rel, off, 2)
    return [("jump", tab(sess, rel, off, label="jump"))]


def s_switch_file(sess):
    """切换文件：打开 shapes.py，在其中输入；other_documents 带上 vector.py"""
    rel = "shapes.py"
    with open(os.path.join(sess.workspace, rel), "w") as fh:
        fh.write(SHAPES_PY)
    sess.open(rel, "python")
    refresh(sess, rel, 0, 4)
    t = sess.texts[sess.uri(rel)]
    off = len(t)
    off = sess.type_text(rel, off, "\n\ndef diag", delay=0.12, on_char=sel_changed(sess, rel))
    others = [_doc(sess, "vector.py")]
    return [("switch_file", tab(sess, rel, off, label="switch_file", other_docs=others))]


def s_diagnostics(sess):
    """有诊断错误时：shapes.py 里引用未定义的名字，带一条 error 诊断"""
    rel = "shapes.py"
    t = sess.texts[sess.uri(rel)]
    off = len(t)
    off = sess.type_text(rel, off, "\n\ndef volume(w, h, d):\n    return w * h * depht", delay=0.05,
                         on_char=sel_changed(sess, rel))
    t = sess.texts[sess.uri(rel)]
    line = t.count("\n")
    col = len(t.split("\n")[-1]) - len("depht")
    dg = common.new("exa.codeium_common_pb.CodeDiagnostic")
    dg.range.start_position.row = line
    dg.range.start_position.col = col
    dg.range.end_position.row = line
    dg.range.end_position.col = col + 5
    dg.message = '"depht" is not defined'
    dg.severity = "error"
    dg.source = "Pyright"
    dg.uri = sess.uri(rel)
    return [("diagnostics", tab(sess, rel, off, label="diagnostics", diagnostics=[dg]))]


def s_clipboard(sess):
    """复制粘贴后：剪贴板里是 vector.py 的一行，粘到 shapes.py，再触发（clipboard_entry 带上剪贴板）"""
    rel = "shapes.py"
    clip = "    print(w.length())"
    t = sess.texts[sess.uri(rel)]
    off = len(t)
    off = sess.type_text(rel, off, "\n\ndef demo():\n    w = Vector(1, 2)\n", delay=0.03)
    t = sess.texts[sess.uri(rel)]
    sess.edit(rel, off, 0, clip)
    off += len(clip)
    refresh(sess, rel, off, 2)
    return [("clipboard", tab(sess, rel, off, label="clipboard", clipboard=clip))]


def s_unicode(sess):
    """多字节字符：光标行里有中文和 emoji，用来核对位置单位"""
    rel = "vector.py"
    t = sess.texts[sess.uri(rel)]
    off = t.index("def main():")
    off = sess.type_text(rel, off, "def describe(v):\n" + UNICODE_LINE, delay=0.03, on_char=sel_changed(sess, rel))
    return [("unicode", tab(sess, rel, off, label="unicode"))]


SCENARIOS = [("first_open", s_first_open), ("typing", s_typing), ("accept", s_accept), ("reject", s_reject),
             ("rename", s_rename), ("jump", s_jump), ("switch_file", s_switch_file), ("diagnostics", s_diagnostics),
             ("clipboard", s_clipboard), ("unicode", s_unicode)]


_tab = tab


def tab(*a, **kw):  # noqa: F811
    kw.setdefault("timeout", 15)
    return _tab(*a, **kw)


def run(args):
    if args.list:
        for n, f in SCENARIOS:
            print("%-12s %s" % (n, (f.__doc__ or "").strip()))
        return
    names = args.scenarios or [n for n, _ in SCENARIOS]
    api_key = common.require_api_key()
    ws = prepare_workspace()
    sess_dir = os.path.join(CAPTURES, time.strftime("%Y%m%d-%H%M%S") + "-capture")
    sess = LSSession(sess_dir, ws, api_key).start()
    sess.state = {}
    summary = []
    try:
        refresh_startup(sess)
        sess.open("vector.py", "python")
        time.sleep(2)
        for n, f in SCENARIOS:
            if n not in names:
                continue
            sess.rec.mark("scenario", label=n)
            try:
                results = f(sess)
            except Exception as e:
                print("[%s] 失败：%s" % (n, repr(e)[:200]))
                summary.append({"label": n, "error": repr(e)[:200]})
                time.sleep(2.5)
                continue
            for label, (resp, dt, tid) in results:
                show(label, resp, dt)
                summary.append({"label": label, "trigger_id": tid, "ms": round(dt * 1000),
                                "kind": resp.WhichOneof("suggestion")})
            time.sleep(2.5)  # 场景之间停顿，频率低于正常打字
        time.sleep(3)  # 等 LS 的预测性请求和上报落盘
    finally:
        sess.stop()
        with open(os.path.join(sess_dir, "summary.json"), "w") as fh:
            json.dump(summary, fh, ensure_ascii=False, indent=1)
    print("记录：%s" % sess_dir)
