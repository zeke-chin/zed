"""直连云端的 Connect 客户端（不经过 LS）。

请求头对齐官方 LS（connect-go）：抓包里 LS → 云端的头见 结论.md「请求头」一节。
- 一元调用：Content-Type application/proto，Accept-Encoding gzip
- 服务端流（GetTab）：Content-Type application/connect+proto，请求帧 gzip 压缩，
  Connect-Content-Encoding / Connect-Accept-Encoding gzip，Accept-Encoding identity

所有调用串行、低频；调用方负责节流（见 throttle()）。
"""
import gzip
import json
import os
import time

import httpx

import common

UA = "connect-go/1.18.1 (go1.26.6)"
API_SERVER = common.api_server_url()
# LS 的 GetTab 实际发往这里：unleash 开关 inference-server-url 按 requestedModelId 下发，见 结论.md 问题 5。
# 环境变量 SPIKE_INFERENCE_URL 可临时覆盖（实验用）
INFERENCE_SERVER = os.environ.get("SPIKE_INFERENCE_URL") or "https://inference-us-west-lb.codeium.com"

_client = None
_last_call = 0.0
MIN_INTERVAL = 2.0  # 两次直连请求至少间隔 2 秒，低于正常打字触发频率


def client():
    global _client
    if _client is None:
        _client = httpx.Client(http2=True, timeout=httpx.Timeout(30.0, read=60.0))
    return _client


def throttle(min_interval=MIN_INTERVAL):
    global _last_call
    dt = time.time() - _last_call
    if dt < min_interval:
        time.sleep(min_interval - dt)
    _last_call = time.time()


class ConnectError(Exception):
    def __init__(self, code, message, http_status=None):
        super().__init__("%s: %s" % (code, message))
        self.code = code
        self.message = message
        self.http_status = http_status


def unary(base, path, req, out_type, timeout_ms=10000):
    throttle()
    body = req.SerializeToString()
    headers = {"User-Agent": UA, "Content-Type": "application/proto", "Accept-Encoding": "gzip",
               "Connect-Protocol-Version": "1", "Connect-Timeout-Ms": str(timeout_ms)}
    r = client().post(base + path, content=body, headers=headers)
    if r.status_code != 200:
        try:
            e = r.json()
        except Exception:
            e = {"code": "http_%d" % r.status_code, "message": r.text[:300]}
        raise ConnectError(e.get("code"), e.get("message"), r.status_code)
    m = common.msg_class(out_type)()
    m.ParseFromString(r.content)
    return m


def stream(base, path, req, out_type, timeout_ms=9500, raw_body=None):
    """服务端流调用。逐条 yield (相对开始的秒数, 消息)；结束帧带错误时抛 ConnectError。

    raw_body：直接发送给定的已序列化请求（重放用）。
    """
    throttle()
    payload = raw_body if raw_body is not None else req.SerializeToString()
    body = common.frame(gzip.compress(payload), compressed=True)
    headers = {"User-Agent": UA, "Content-Type": "application/connect+proto", "Accept-Encoding": "identity",
               "Connect-Protocol-Version": "1", "Connect-Timeout-Ms": str(timeout_ms),
               "Connect-Content-Encoding": "gzip", "Connect-Accept-Encoding": "gzip"}
    t0 = time.time()
    with client().stream("POST", base + path, content=body, headers=headers) as r:
        if r.status_code != 200:
            r.read()
            try:
                e = r.json()
            except Exception:
                e = {"code": "http_%d" % r.status_code, "message": r.text[:300]}
            raise ConnectError(e.get("code"), e.get("message"), r.status_code)
        fr = common.FrameReader()
        for chunk in r.iter_raw():
            for flags, data in fr.feed(chunk):
                if flags & 2:
                    end = json.loads(data or b"{}")
                    if end.get("error"):
                        raise ConnectError(end["error"].get("code"), end["error"].get("message"))
                    return
                m = common.msg_class(out_type)()
                m.ParseFromString(data)
                yield time.time() - t0, m
