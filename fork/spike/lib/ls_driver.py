"""拉起官方 LS 并按 IDE→LS 的协议驱动它，同时记录两段流量。

  ① IDE → LS：Connect（HTTP/1.1，application/proto，头 x-codeium-csrf-token）+ LSP（TCP，JSON-RPC）
  ② LS → 云端：LS 的 --api_server_url / --inference_api_server_url 指向本地 CloudProxy，代理转发到真实云端

用法（经 fork/spike/run）：
  run ls-tab                 备选路线最小验证：拉起 LS，打开一个文件，调一次 HandleStreamingTabV2
  run capture [场景名...]     跑抓包场景（默认全部），结果在 captures/<时间戳>/
  run capture --list         列出场景
"""
import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import uuid

import httpx

import common
import identity
from recorder import CloudProxy, ConnectTunnel, FakeExtensionServer, Recorder

LS_SVC = "exa.language_server_pb.LanguageServerService"
RUN_DIR = os.path.join(common.SPIKE_DIR, ".lsrun")
CAPTURES = os.path.join(common.SPIKE_DIR, "captures")


class LspClient:
    def __init__(self, rec, port):
        self.rec = rec
        self.sock = socket.create_connection(("127.0.0.1", port))
        self.next_id = 0
        self.pending = {}
        self.lock = threading.Lock()
        threading.Thread(target=self._reader, daemon=True).start()

    def _send(self, obj):
        data = json.dumps(obj).encode()
        self.rec.write(seg="lsp", dir="c2s", msg=obj)
        self.sock.sendall(b"Content-Length: %d\r\n\r\n" % len(data) + data)

    def _reader(self):
        f = self.sock.makefile("rb")
        while True:
            headers = {}
            while True:
                line = f.readline()
                if not line:
                    return
                line = line.strip()
                if not line:
                    break
                k, v = line.decode().split(":", 1)
                headers[k.lower()] = v.strip()
            body = f.read(int(headers["content-length"]))
            msg = json.loads(body)
            self.rec.write(seg="lsp", dir="s2c", msg=msg)
            if "id" in msg and "method" in msg:
                # 服务端请求：一律回空结果
                result = None
                if msg["method"] == "workspace/configuration":
                    result = [None for _ in msg.get("params", {}).get("items", [])]
                self._send({"jsonrpc": "2.0", "id": msg["id"], "result": result})
            elif "id" in msg:
                ev = self.pending.pop(msg["id"], None)
                if ev:
                    ev[1] = msg
                    ev[0].set()

    def request(self, method, params, timeout=30):
        with self.lock:
            self.next_id += 1
            i = self.next_id
        ev = [threading.Event(), None]
        self.pending[i] = ev
        self._send({"jsonrpc": "2.0", "id": i, "method": method, "params": params})
        ev[0].wait(timeout)
        return ev[1]

    def notify(self, method, params):
        self._send({"jsonrpc": "2.0", "method": method, "params": params})


class LSSession:
    def __init__(self, session_dir, workspace, api_key, record_cloud=True, mitm=None):
        self.rec = Recorder(session_dir)
        # mitm=None：Linux 上自动用 TLS 终止录制（mitm.py），macOS 上用明文记录代理
        self.mitm = (sys.platform.startswith("linux") and record_cloud) if mitm is None else mitm
        self._mitm = None
        self.workspace = workspace
        self.api_key = api_key
        self.csrf = str(uuid.uuid4())
        self.proc = None
        self.record_cloud = record_cloud
        self.versions = {}  # uri -> version
        self.texts = {}
        self.langs = {}

    def start(self):
        ls_path = common.find_ls_path()
        api_up = common.api_server_url()
        if self.mitm:
            import mitm as _m
            self._mitm = _m.Mitm(self.rec)
            api_url, inf_url = "https://server.codeium.com", "https://inference.codeium.com"
        elif self.record_cloud:
            self.api_proxy = CloudProxy(self.rec, api_up, "api")
            self.inf_proxy = CloudProxy(self.rec, "https://inference.codeium.com", "inference")
            api_url, inf_url = self.api_proxy.url, self.inf_proxy.url
            mapping = {"https://server.codeium.com": api_url, "https://inference.codeium.com": inf_url}
            self.api_proxy.rewrite_urls = mapping
            self.inf_proxy.rewrite_urls = mapping
        else:
            api_url, inf_url = api_up, "https://inference.codeium.com"
        self.ext = FakeExtensionServer(self.rec, self.csrf)
        # parent pipe：LS 用它监测父进程是否存活
        self.pipe_path = os.path.join(tempfile.gettempdir(), "spike_parent_%s" % uuid.uuid4().hex[:12])
        self.pipe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.pipe.bind(self.pipe_path)
        self.pipe.listen(4)
        self._pipe_conns = []
        threading.Thread(target=self._accept_pipe, daemon=True).start()

        p = identity.product()
        db = os.path.join(RUN_DIR, "database")
        codeium_dir = os.path.join(RUN_DIR, "codeium")
        os.makedirs(db, exist_ok=True)
        os.makedirs(codeium_dir, exist_ok=True)
        args = [ls_path,
                "--api_server_url", api_url,
                "--run_child", "--enable_lsp",
                "--extension_server_port", str(self.ext.port),
                "--ide_name", "windsurf",
                "--random_port",
                "--inference_api_server_url", inf_url,
                "--database_dir", db,
                "--enable_index_service", "--enable_local_search",
                "--indexed_files_retention_period_days", "30",
                "--codeium_dir", codeium_dir,
                "--parent_pipe_path", self.pipe_path,
                "--windsurf_version", p["windsurfVersion"],
                "--stdin_initial_metadata",
                "--verbosity_level", os.environ.get("SPIKE_LS_VERBOSITY", "-1"),
                "--detect_proxy=%s" % ("true" if os.environ.get("SPIKE_DETECT_PROXY") else "false")]
        env = dict(os.environ)
        env["WINDSURF_CSRF_TOKEN"] = self.csrf
        env["CODEIUM_EDITOR_APP_ROOT"] = identity.APP_ROOT
        # 让 Go 运行时把每次 DNS 解析的主机名打到 ls.log，便于发现绕过代理的直连
        env["GODEBUG"] = "netdns=2"
        if self.mitm:
            env["SSL_CERT_FILE"] = _m.bundle_path()
        elif self.record_cloud:
            # 兜底：LS 绕过 --api_server_url 直连的 HTTPS 主机经此隧道，至少记录主机名
            self.tunnel = ConnectTunnel(self.rec)
            env["HTTPS_PROXY"] = env["https_proxy"] = "http://127.0.0.1:%d" % self.tunnel.port
            env["NO_PROXY"] = env["no_proxy"] = "127.0.0.1,localhost"
        self.rec.write(seg="ls", dir="spawn", args=args)
        self.log = open(os.path.join(self.rec.dir, "ls.log"), "wb")
        self.proc = subprocess.Popen(args, stdin=subprocess.PIPE, stdout=self.log, stderr=subprocess.STDOUT,
                                     cwd=self.workspace, env=env)
        md = identity.metadata(self.api_key)
        self.proc.stdin.write(md.SerializeToString())
        self.proc.stdin.close()
        if not self.ext.started.wait(60):
            raise RuntimeError("LS 60 秒内没有回报端口，见 %s/ls.log" % self.rec.dir)
        info = self.ext.started_info
        self.port = info.language_server_port
        self.lsp_port = info.lsp_port
        if info.csrf_token:
            self.csrf = info.csrf_token
        self.rec.write(seg="ls", dir="started", port=self.port, lsp_port=self.lsp_port)
        self.http = httpx.Client(http1=True, http2=False, timeout=60)
        # 心跳：官方扩展每秒一次
        self._hb_stop = threading.Event()
        self.heartbeat()
        threading.Thread(target=self._hb_loop, daemon=True).start()
        # LSP
        self.lsp = LspClient(self.rec, self.lsp_port)
        ws_uri = "file://" + self.workspace
        self.lsp.request("initialize", {
            "processId": os.getpid(), "rootUri": ws_uri, "rootPath": self.workspace,
            "workspaceFolders": [{"uri": ws_uri, "name": os.path.basename(self.workspace)}],
            "capabilities": {"textDocument": {"synchronization": {"didSave": True, "dynamicRegistration": True}},
                             "workspace": {"workspaceFolders": True, "configuration": True}},
            "clientInfo": {"name": "Devin", "version": p["windsurfVersion"]},
        })
        self.lsp.notify("initialized", {})
        return self

    def _accept_pipe(self):
        while True:
            try:
                c, _ = self.pipe.accept()
                self._pipe_conns.append(c)
            except Exception:
                return

    def _hb_loop(self):
        while not self._hb_stop.wait(1.0):
            try:
                self.heartbeat()
            except Exception:
                pass

    def heartbeat(self):
        return self.call("Heartbeat", "HeartbeatRequest", record=False, metadata=identity.metadata(self.api_key))

    def call(self, method, req_type, record=True, timeout=60, **fields):
        req = common.new("exa.language_server_pb." + req_type) if "." not in req_type else common.new(req_type)
        for k, v in fields.items():
            f = getattr(req, k)
            if hasattr(f, "CopyFrom") and not isinstance(v, (str, bytes, int, float, bool)):
                f.CopyFrom(v)
            elif isinstance(v, list):
                getattr(req, k).extend(v)
            else:
                setattr(req, k, v)
        if "metadata" in req.DESCRIPTOR.fields_by_name and not req.HasField("metadata"):
            req.metadata.CopyFrom(identity.metadata(self.api_key))
        body = req.SerializeToString()
        path = "/%s/%s" % (LS_SVC, method)
        headers = {"Content-Type": "application/proto", "Connect-Protocol-Version": "1",
                   "x-codeium-csrf-token": self.csrf}
        rid = None
        t0 = time.time()
        if record:
            rid = self.rec.write(seg="ide", dir="req", path=path, body=body)
        r = self.http.post("http://127.0.0.1:%d%s" % (self.port, path), content=body, headers=headers, timeout=timeout)
        if record:
            self.rec.write(seg="ide", dir="resp", rid=rid, path=path, status=r.status_code, body=r.content,
                           dt=time.time() - t0)
        if r.status_code != 200:
            raise RuntimeError("%s -> HTTP %d %s" % (method, r.status_code, r.content[:300]))
        out_type = common.method(LS_SVC, method).output_type.full_name
        resp = common.msg_class(out_type)()
        resp.ParseFromString(r.content)
        return resp

    # ---- 文档同步（LSP）----
    def uri(self, rel):
        return "file://" + os.path.join(self.workspace, rel)

    def open(self, rel, language_id):
        path = os.path.join(self.workspace, rel)
        with open(path) as fh:
            text = fh.read()
        u = self.uri(rel)
        self.versions[u] = 1
        self.texts[u] = text
        self.langs[u] = language_id
        self.lsp.notify("textDocument/didOpen", {"textDocument": {"uri": u, "languageId": language_id, "version": 1, "text": text}})
        return text

    def close(self, rel):
        u = self.uri(rel)
        self.lsp.notify("textDocument/didClose", {"textDocument": {"uri": u}})

    def edit(self, rel, offset, delete_len, insert):
        """在 offset 处删 delete_len 个字符再插入 insert，用增量 didChange 同步（VS Code 默认行为）。"""
        u = self.uri(rel)
        text = self.texts[u]
        start = pos_of(text, offset)
        end = pos_of(text, offset + delete_len)
        new = text[:offset] + insert + text[offset + delete_len:]
        self.texts[u] = new
        self.versions[u] += 1
        self.lsp.notify("textDocument/didChange", {
            "textDocument": {"uri": u, "version": self.versions[u]},
            "contentChanges": [{"range": {"start": start, "end": end}, "rangeLength": delete_len, "text": insert}]})
        return new

    def type_text(self, rel, offset, s, delay=0.12, on_char=None):
        """逐字符输入，模拟打字节奏。"""
        for i, ch in enumerate(s):
            self.edit(rel, offset + i, 0, ch)
            if on_char:
                on_char(offset + i + 1)
            time.sleep(delay)
        return offset + len(s)

    def stop(self):
        try:
            self._hb_stop.set()
            self.call("Exit", "ExitRequest", record=False, timeout=3)
        except Exception:
            pass
        if self.proc:
            try:
                self.proc.wait(5)
            except Exception:
                self.proc.kill()
        try:
            self.pipe.close()
            os.unlink(self.pipe_path)
        except Exception:
            pass
        if self._mitm:
            self._mitm.stop()


def pos_of(text, offset):
    """字符偏移 → LSP 位置（行从 0 开始，列按 UTF-16）。"""
    before = text[:offset]
    line = before.count("\n")
    col_text = before[before.rfind("\n") + 1:]
    return {"line": line, "character": len(col_text.encode("utf-16-le")) // 2}


# ---------------- IDE 动作（对齐 extension.js 的 refreshContextForIdeAction）----------------
# STARTUP=5 启动时一次；ACTIVE_EDITOR_CHANGED=4 切换编辑器；SELECTION_CHANGED=2 光标/选区变化（扩展里 500ms 防抖，leading）；
# VISIBLE_RANGES_CHANGED=3 滚动；SAVE=1 保存

def refresh(sess, rel, offset, action, visible=None):
    u = sess.uri(rel)
    req_doc = document(sess, rel, offset, sess.langs.get(u, "plaintext"), visible=visible)
    others = [document(sess, os.path.relpath(x[7:], sess.workspace), 0, sess.langs.get(x, "plaintext"))
              for x in sess.texts if x != u]
    sess.call("RefreshContextForIdeAction", "RefreshContextForIdeActionRequest", active_document=req_doc,
              ide_action=action, other_documents=others, recently_opened_uris=[u],
              workspace_uris=["file://" + sess.workspace])


# ---------------- Tab 请求 ----------------

LANG = {"python": 30, "rust": 39, "typescript": 45}


def document(sess, rel, offset, language_id, visible=None, selections=None):
    """与 extension.js 的 getDocumentInfo 一致：cursor_offset 是 UTF-8 字节偏移；不填 language、cursor_position；
    visible_range 用行列（列是 UTF-16）；selections 用 UTF-8 字节偏移。"""
    u = sess.uri(rel)
    text = sess.texts[u]
    d = common.new("exa.codeium_common_pb.Document")
    d.absolute_uri = u
    d.workspace_uri = "file://" + sess.workspace
    d.text = text
    d.editor_language = language_id
    d.cursor_offset = len(text[:offset].encode("utf-8"))
    d.line_ending = "\n"
    d.is_dirty = True
    d.timestamp.GetCurrentTime()
    if visible:
        d.visible_range.start_position.row, d.visible_range.end_position.row = visible
    for a, b in selections or []:
        r = d.selections.add()
        r.start_offset = len(text[:a].encode("utf-8"))
        r.end_offset = len(text[:b].encode("utf-8"))
    return d


def tab(sess, rel, offset, language_id="python", trigger=3, clipboard="", diagnostics=None, other_docs=None,
        disable_tab_jump=False, label="", timeout=60):
    trigger_id = str(uuid.uuid4())
    md = identity.metadata(sess.api_key, trigger_id=trigger_id)
    sess.rec.mark("tab", trigger_id=trigger_id, label=label)
    eo = common.new("exa.codeium_common_pb.EditorOptions")
    eo.tab_size = 4
    eo.insert_spaces = True
    kw = dict(metadata=md, document=document(sess, rel, offset, language_id), editor_options=eo,
              supercomplete_trigger_condition=trigger, disable_tab_jump=disable_tab_jump)
    if clipboard:
        kw["clipboard_entry"] = clipboard
    if diagnostics:
        kw["diagnostics"] = diagnostics
    if other_docs:
        kw["other_documents"] = other_docs
    t0 = time.time()
    resp = sess.call("HandleStreamingTabV2", "HandleStreamingTabV2Request", timeout=timeout, **kw)
    return resp, time.time() - t0, trigger_id


def refresh_startup(sess):
    sess.call("RefreshContextForIdeAction", "RefreshContextForIdeActionRequest", ide_action=5,
              workspace_uris=["file://" + sess.workspace])


def sel_changed(sess, rel, interval=0.5):
    """返回 on_char 回调：按 500ms leading 防抖发 SELECTION_CHANGED（与扩展一致）。"""
    last = [0.0]

    def cb(off):
        if time.time() - last[0] >= interval:
            last[0] = time.time()
            refresh(sess, rel, off, 2)
    return cb


def summarize_tab(resp):
    d = common.to_json(resp)
    d.pop("request_info", None)
    return d


# ---------------- 命令 ----------------

SAMPLE_PY = '''import math


class Vector:
    def __init__(self, x, y):
        self.x = x
        self.y = y

    def length(self):
        return math.sqrt(self.x * self.x + self.y * self.y)

    def scale(self, factor):
        return Vector(self.x * factor, self.y * factor)


def add(a, b):
    return Vector(a.x + b.x, a.y + b.y)


def main():
    v = Vector(3, 4)
    print(v.length())
    w = add(v, Vector(1, 1))
    print(w.length())
'''


def prepare_workspace(name="ws"):
    ws = os.path.join(RUN_DIR, name)
    if os.path.exists(ws):
        shutil.rmtree(ws)
    os.makedirs(ws)
    with open(os.path.join(ws, "vector.py"), "w") as fh:
        fh.write(SAMPLE_PY)
    # 独立 git 仓库：否则 LS 会沿用 fork 仓库的 .gitignore（.lsrun/ 被忽略）而拒绝补全
    subprocess.run(["git", "init", "-q", ws], check=True)
    return ws


def cmd_tab(args):
    api_key = common.require_api_key()
    ws = prepare_workspace()
    sess_dir = os.path.join(CAPTURES, time.strftime("%Y%m%d-%H%M%S") + "-ls-tab")
    sess = LSSession(sess_dir, ws, api_key, record_cloud=not args.no_proxy).start()
    try:
        print("LS 已启动：connect 端口 %d，LSP 端口 %d" % (sess.port, sess.lsp_port))
        refresh_startup(sess)
        sess.open("vector.py", "python")
        refresh(sess, "vector.py", 0, 4)
        time.sleep(2)
        # 光标放在 main() 末尾，再输入一行开头，引导模型补全
        text = sess.texts[sess.uri("vector.py")]
        off = len(text)
        off = sess.type_text("vector.py", off, "    u = v.sc", delay=0.1, on_char=sel_changed(sess, "vector.py"))
        resp, dt, tid = tab(sess, "vector.py", off, label="ls-tab")
        print("HandleStreamingTabV2 用时 %.0f ms" % (dt * 1000))
        print(json.dumps(summarize_tab(resp), ensure_ascii=False, indent=2))
        print("记录：%s" % sess_dir)
    finally:
        sess.stop()


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd")
    t = sub.add_parser("tab")
    t.add_argument("--no-proxy", action="store_true", help="LS 直连云端，不经过记录代理")
    c = sub.add_parser("capture")
    c.add_argument("scenarios", nargs="*")
    c.add_argument("--list", action="store_true")
    args = ap.parse_args()
    if args.cmd == "tab":
        cmd_tab(args)
    elif args.cmd == "capture":
        import scenarios
        scenarios.run(args)
    else:
        ap.print_help()


if __name__ == "__main__":
    main()
