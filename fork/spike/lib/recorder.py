"""记录用的基础设施：
- Recorder：把每条请求/响应写进 captures/<session>/events.jsonl（含时间戳，便于两段对应）
- CloudProxy：LS → 云端的记录代理。LS 以明文 HTTP 连本代理，本代理再用 HTTPS(h2) 转发到真实云端，
  响应按块原样流回，不解 TLS。
- FakeExtensionServer：冒充 IDE 扩展里的 ExtensionServerService（LS 启动后回报端口用）。
"""
import base64
import json
import os
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import httpx

import common

HOP = {"host", "content-length", "connection", "transfer-encoding", "keep-alive", "proxy-connection", "upgrade", "te"}


class Recorder:
    def __init__(self, session_dir):
        os.makedirs(session_dir, exist_ok=True)
        self.dir = session_dir
        self.path = os.path.join(session_dir, "events.jsonl")
        self.lock = threading.Lock()
        self.seq = 0
        self.fh = open(self.path, "a")
        self.t0 = time.time()

    def write(self, **ev):
        with self.lock:
            self.seq += 1
            ev.setdefault("t", time.time())
            ev["seq"] = self.seq
            for k in ("body", "chunk"):
                if isinstance(ev.get(k), (bytes, bytearray)):
                    ev[k + "_b64"] = base64.b64encode(ev.pop(k)).decode()
            self.fh.write(json.dumps(ev, ensure_ascii=False) + "\n")
            self.fh.flush()
            return self.seq

    def mark(self, kind, **kw):
        """场景标记：在事件流里插入一个边界，decode 时按它切分。"""
        return self.write(seg="mark", kind=kind, **kw)


def read_body(handler):
    te = handler.headers.get("Transfer-Encoding", "").lower()
    if "chunked" in te:
        out = b""
        while True:
            line = handler.rfile.readline().strip()
            n = int(line.split(b";")[0], 16)
            if n == 0:
                # trailer
                while handler.rfile.readline() not in (b"\r\n", b"\n", b""):
                    pass
                break
            out += handler.rfile.read(n)
            handler.rfile.readline()
        return out
    n = int(handler.headers.get("Content-Length", "0") or 0)
    return handler.rfile.read(n) if n else b""


class CloudProxy:
    def __init__(self, rec, upstream, name, port=0, tls_ctx=None, host="127.0.0.1"):
        """upstream 为 None 时按请求的 Host 头转发到 https://<Host>（TLS 终止模式）。"""
        self.rec = rec
        self.upstream = upstream.rstrip("/") if upstream else None
        self.name = name
        self.client = httpx.Client(http2=True, timeout=httpx.Timeout(60.0, read=120.0))
        # 把云端下发的推理地址（GetUserStatus 里的 inference_server_url 等）改写成本代理，
        # 否则 LS 会绕过代理直连 https://server.codeium.com 发 GetTab。只改本地转给 LS 的副本。
        self.rewrite_urls = {}
        proxy = self

        class H(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def do_POST(self):
                proxy.handle(self)

            do_GET = do_POST

        self.server = ThreadingHTTPServer((host, port), H)
        if tls_ctx is not None:
            self.server.socket = tls_ctx.wrap_socket(self.server.socket, server_side=True, do_handshake_on_connect=False)
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    @property
    def url(self):
        return "http://127.0.0.1:%d" % self.port

    def handle(self, h):
        body = read_body(h)
        req_headers = [(k, v) for k, v in h.headers.items()]
        upstream = self.upstream or ("https://" + h.headers.get("Host", "").split(":")[0])
        rid = self.rec.write(seg="cloud", dir="req", upstream=self.name if self.upstream else upstream, method=h.command,
                             path=h.path, headers=req_headers, body=body, http_version=h.request_version)
        fwd = [(k, v) for k, v in req_headers if k.lower() not in HOP]
        t_start = time.time()
        try:
            req = self.client.build_request(h.command, upstream + h.path, headers=fwd, content=body)
            resp = self.client.send(req, stream=True)
        except Exception as e:
            self.rec.write(seg="cloud", dir="err", rid=rid, error=repr(e))
            h.send_response(502)
            h.send_header("Content-Length", "0")
            h.end_headers()
            return
        resp_headers = list(resp.headers.multi_items())
        self.rec.write(seg="cloud", dir="resp_head", rid=rid, status=resp.status_code, headers=resp_headers,
                       http_version=resp.http_version, ttfb=time.time() - t_start)
        if self.rewrite_urls and resp.status_code == 200 and resp.headers.get("content-type") == "application/proto":
            return self._rewrite_unary(h, rid, resp, resp_headers, t_start)
        h.send_response(resp.status_code)
        for k, v in resp_headers:
            if k.lower() not in HOP:
                h.send_header(k, v)
        h.send_header("Transfer-Encoding", "chunked")
        h.end_headers()
        try:
            for chunk in resp.iter_raw():
                if not chunk:
                    continue
                self.rec.write(seg="cloud", dir="resp_chunk", rid=rid, chunk=chunk, dt=time.time() - t_start)
                h.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
                h.wfile.flush()
            trailers = getattr(resp, "trailers", None)
            h.wfile.write(b"0\r\n\r\n")
            h.wfile.flush()
            self.rec.write(seg="cloud", dir="resp_end", rid=rid, dt=time.time() - t_start,
                           trailers=dict(trailers) if trailers else None)
        except Exception as e:
            self.rec.write(seg="cloud", dir="err", rid=rid, error=repr(e))
        finally:
            resp.close()


def rewrite_msg(msg, mapping):
    """递归把 string 字段里以 mapping 的键开头的 URL 换成对应值，返回替换次数。"""
    n = 0
    for fd, val in msg.ListFields():
        if fd.type == fd.TYPE_STRING:
            vals = list(val) if common.is_repeated(fd) else [val]
            new = []
            for v in vals:
                for k, r in mapping.items():
                    if v.startswith(k):
                        v = r + v[len(k):]
                        n += 1
                new.append(v)
            if common.is_repeated(fd):
                del val[:]
                val.extend(new)
            else:
                setattr(msg, fd.name, new[0])
        elif fd.type == fd.TYPE_MESSAGE:
            if fd.message_type.GetOptions().map_entry:
                for k in list(val.keys()):
                    if fd.message_type.fields_by_name["value"].type == fd.TYPE_MESSAGE:
                        n += rewrite_msg(val[k], mapping)
            elif common.is_repeated(fd):
                for x in val:
                    n += rewrite_msg(x, mapping)
            else:
                n += rewrite_msg(val, mapping)
    return n


def _rewrite_unary(self, h, rid, resp, resp_headers, t_start):
    import gzip as _gz
    raw = b"".join(resp.iter_raw())
    resp.close()
    self.rec.write(seg="cloud", dir="resp_chunk", rid=rid, chunk=raw, dt=time.time() - t_start)
    self.rec.write(seg="cloud", dir="resp_end", rid=rid, dt=time.time() - t_start, trailers=None)
    body = _gz.decompress(raw) if resp.headers.get("content-encoding") == "gzip" else raw
    try:
        from decode import method_types
        _, ot, _ = method_types(h.path)
        if ot:
            m = common.msg_class(ot)()
            m.ParseFromString(body)
            n = rewrite_msg(m, self.rewrite_urls)
            if n:
                body = m.SerializeToString()
                self.rec.write(seg="cloud", dir="rewrite", rid=rid, count=n)
    except Exception as e:
        self.rec.write(seg="cloud", dir="rewrite_err", rid=rid, error=repr(e))
    h.send_response(resp.status_code)
    for k, v in resp_headers:
        if k.lower() not in HOP and k.lower() != "content-encoding":
            h.send_header(k, v)
    h.send_header("Content-Length", str(len(body)))
    h.end_headers()
    h.wfile.write(body)


CloudProxy._rewrite_unary = _rewrite_unary


class FakeExtensionServer:
    def __init__(self, rec, csrf_token):
        self.rec = rec
        self.csrf = csrf_token
        self.started = threading.Event()
        self.started_info = None
        srv = self

        class H(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def do_POST(self):
                body = read_body(self)
                method_name = self.path.rsplit("/", 1)[-1]
                srv.rec.write(seg="ext", dir="req", path=self.path, headers=list(self.headers.items()), body=body)
                if method_name == "LanguageServerStarted":
                    m = common.new("exa.extension_server_pb.LanguageServerStartedRequest")
                    m.ParseFromString(body)
                    srv.started_info = m
                    srv.started.set()
                ctype = self.headers.get("Content-Type", "application/proto")
                if ctype.startswith("application/connect"):
                    # 服务端流：回一个空的结束帧
                    payload = common.frame(b"{}", end=True)
                else:
                    payload = b""
                self.send_response(200)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()


class ConnectTunnel:
    """HTTPS_PROXY 用的 CONNECT 隧道：只记录 LS 直连了哪些主机，不解 TLS。"""

    def __init__(self, rec):
        import socket as _s
        self.rec = rec
        self.sock = _s.socket()
        self.sock.setsockopt(_s.SOL_SOCKET, _s.SO_REUSEADDR, 1)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(64)
        self.port = self.sock.getsockname()[1]
        threading.Thread(target=self._loop, daemon=True).start()

    def _loop(self):
        while True:
            c, _ = self.sock.accept()
            threading.Thread(target=self._handle, args=(c,), daemon=True).start()

    def _handle(self, c):
        import socket as _s
        data = b""
        while b"\r\n\r\n" not in data:
            d = c.recv(4096)
            if not d:
                return
            data += d
        line = data.split(b"\r\n", 1)[0].decode()
        parts = line.split()
        if parts[0] != "CONNECT":
            self.rec.write(seg="tunnel", dir="non_connect", line=line)
            c.close()
            return
        host, port = parts[1].rsplit(":", 1)
        self.rec.write(seg="tunnel", dir="connect", host=host, port=int(port))
        try:
            u = _s.create_connection((host, int(port)))
        except Exception as e:
            c.sendall(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
            c.close()
            return
        c.sendall(b"HTTP/1.1 200 Connection Established\r\n\r\n")

        def pipe(a, b):
            try:
                while True:
                    d = a.recv(65536)
                    if not d:
                        break
                    b.sendall(d)
            except Exception:
                pass
            finally:
                try:
                    b.shutdown(_s.SHUT_WR)
                except Exception:
                    pass

        threading.Thread(target=pipe, args=(c, u), daemon=True).start()
        pipe(u, c)
