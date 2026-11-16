"""从 captures/ 里取出某类云端调用的请求/响应（解压后的 protobuf），供重放和对照用。"""
import glob
import gzip
import os

import common
from decode import build, load, header

CAPTURES = os.path.join(common.SPIKE_DIR, "captures")


def sessions():
    return sorted((d for d in glob.glob(os.path.join(CAPTURES, "*")) if os.path.isdir(d)), key=os.path.getmtime)


def cloud_calls(session, suffix):
    out = []
    for c in build(load(session)):
        if c.get("seg") == "cloud" and c["path"].endswith(suffix):
            out.append(c)
    return out


def request_payloads(c):
    """返回请求里的 protobuf 负载列表（流式请求可能多帧，一元请求一条）。"""
    body = c["req_body"]
    if header(c["req_headers"], "content-type", ) and header(c["req_headers"], "content-type").startswith("application/connect"):
        out = []
        for flags, p in common.iter_frames(body):
            if flags & 1:
                p = gzip.decompress(p)
            if not flags & 2:
                out.append(p)
        return out
    if header(c["req_headers"], "content-encoding") == "gzip":
        body = gzip.decompress(body)
    return [body]
