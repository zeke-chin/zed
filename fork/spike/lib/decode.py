"""把 captures/<session>/events.jsonl 解码成可读形式。

用法：
  run decode [session目录|latest] [--summary] [--full] [--out 文件] [--seg cloud,ide,...] [--path 子串]
默认输出摘要时间线；--full 输出每条消息的完整 JSON（凭据打码，长文本截断可用 --no-trunc 关闭）。
"""
import argparse
import base64
import glob
import gzip
import json
import os
import sys

import common

CAPTURES = os.path.join(common.SPIKE_DIR, "captures")


def latest():
    ds = sorted(d for d in glob.glob(os.path.join(CAPTURES, "*")) if os.path.isdir(d))
    if not ds:
        sys.exit("captures/ 下没有记录")
    return ds[-1]


def method_types(path):
    """/<pkg.Service>/<Method> → (输入类型, 输出类型, 是否服务端流)"""
    try:
        _, svc, name = path.split("?")[0].split("/")
        m = common.method(svc, name)
        return m.input_type.full_name, m.output_type.full_name, m.server_streaming if hasattr(m, "server_streaming") else None
    except Exception:
        return None, None, None


def is_stream(headers):
    for k, v in headers:
        if k.lower() == "content-type":
            return v.startswith("application/connect")
    return False


def header(headers, name):
    for k, v in headers:
        if k.lower() == name:
            return v
    return None


def decode_msg(type_name, data):
    if not type_name:
        return {"_raw_len": len(data)}
    try:
        m = common.msg_class(type_name)()
        m.ParseFromString(data)
        return common.to_json(m)
    except Exception as e:
        return {"_decode_error": str(e), "_raw_len": len(data)}


def decode_body(type_name, headers, body):
    if header(headers, "content-encoding") == "gzip":
        body = gzip.decompress(body)
    if is_stream(headers):
        out = []
        for flags, payload in common.iter_frames(body):
            if flags & 1:
                payload = gzip.decompress(payload)
            if flags & 2:
                out.append({"_end": json.loads(payload or b"{}")})
            else:
                out.append(decode_msg(type_name, payload))
        return out
    return decode_msg(type_name, body)


def trunc(o, n=400):
    if isinstance(o, dict):
        return {k: trunc(v, n) for k, v in o.items()}
    if isinstance(o, list):
        return [trunc(x, n) for x in o]
    if isinstance(o, str) and len(o) > n:
        return o[:n] + "…(%d 字符)" % len(o)
    return o


def mask_headers(hs):
    out = []
    for k, v in hs:
        if k.lower() in ("authorization", "x-api-key", "x-auth-token", "x-codeium-csrf-token"):
            v = common.mask(v)
        out.append([k, v])
    return out


def load(session):
    evs = []
    with open(os.path.join(session, "events.jsonl")) as fh:
        for line in fh:
            e = json.loads(line)
            for k in ("body", "chunk"):
                if k + "_b64" in e:
                    e[k] = base64.b64decode(e.pop(k + "_b64"))
            evs.append(e)
    return evs


def build(evs):
    """把事件聚合成调用：每个调用含请求、响应、时间。"""
    calls = {}
    order = []
    for e in evs:
        seg = e.get("seg")
        if seg == "cloud":
            if e["dir"] == "req":
                c = {"seg": "cloud", "t": e["t"], "seq": e["seq"], "path": e["path"], "upstream": e.get("upstream"),
                     "req_headers": e["headers"], "req_body": e["body"] if "body" in e else b"", "chunks": [],
                     "http_version": e.get("http_version")}
                calls[("cloud", e["seq"])] = c
                order.append(c)
            else:
                c = calls.get(("cloud", e.get("rid")))
                if not c:
                    continue
                if e["dir"] == "resp_head":
                    c["status"] = e["status"]
                    c["resp_headers"] = e["headers"]
                    c["ttfb"] = e.get("ttfb")
                    c["up_http_version"] = e.get("http_version")
                elif e["dir"] == "resp_chunk":
                    c["chunks"].append((e["dt"], e["chunk"]))
                elif e["dir"] == "resp_end":
                    c["dt"] = e["dt"]
                    c["trailers"] = e.get("trailers")
                elif e["dir"] == "err":
                    c["error"] = e["error"]
        elif seg == "ide":
            if e["dir"] == "req":
                c = {"seg": "ide", "t": e["t"], "seq": e["seq"], "path": e["path"], "req_body": e.get("body", b""),
                     "req_headers": [["Content-Type", "application/proto"]]}
                calls[("ide", e["seq"])] = c
                order.append(c)
            else:
                c = calls.get(("ide", e.get("rid")))
                if c:
                    c["status"] = e["status"]
                    c["resp_body"] = e.get("body", b"")
                    c["dt"] = e.get("dt")
                    c["resp_headers"] = [["Content-Type", "application/proto"]]
        elif seg in ("lsp", "mark", "ext", "ls"):
            order.append(e)
    return order


def render_call(c, full, no_trunc):
    it, ot, _ = method_types(c["path"])
    d = {"seg": c["seg"], "seq": c["seq"], "t": c["t"], "path": c["path"]}
    if c["seg"] == "cloud":
        d["upstream"] = c.get("upstream")
        d["status"] = c.get("status")
        d["ttfb_ms"] = round((c.get("ttfb") or 0) * 1000)
        d["total_ms"] = round((c.get("dt") or 0) * 1000)
        if full:
            d["req_headers"] = mask_headers(c["req_headers"])
            d["resp_headers"] = mask_headers(c.get("resp_headers") or [])
            d["trailers"] = c.get("trailers")
            d["chunk_times_ms"] = [round(t * 1000) for t, _ in c["chunks"]]
        body = b"".join(ch for _, ch in c["chunks"])
        d["request"] = decode_body(it, c["req_headers"], c["req_body"])
        d["response"] = decode_body(ot, c.get("resp_headers") or [], body) if c.get("status") == 200 else body[:500].decode("utf-8", "replace")
        if "error" in c:
            d["error"] = c["error"]
    else:
        d["status"] = c.get("status")
        d["total_ms"] = round((c.get("dt") or 0) * 1000)
        d["request"] = decode_msg(it, c["req_body"])
        rb = c.get("resp_body", b"")
        d["response"] = decode_msg(ot, rb) if c.get("status") == 200 else rb[:500].decode("utf-8", "replace")
    d = common.scrub(d)
    return d if no_trunc else trunc(d)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("session", nargs="?", default="latest")
    ap.add_argument("--full", action="store_true")
    ap.add_argument("--no-trunc", action="store_true")
    ap.add_argument("--seg", default="")
    ap.add_argument("--path", default="")
    ap.add_argument("--out")
    a = ap.parse_args()
    session = latest() if a.session == "latest" else a.session
    evs = load(session)
    order = build(evs)
    segs = set(s for s in a.seg.split(",") if s)
    out = open(a.out, "w") if a.out else sys.stdout
    t0 = evs[0]["t"] if evs else 0
    for c in order:
        if segs and c.get("seg") not in segs:
            continue
        if "path" in c and c.get("seg") in ("cloud", "ide"):
            if a.path and a.path not in c["path"]:
                continue
            if a.full or a.out:
                out.write(json.dumps(render_call(c, a.full, a.no_trunc), ensure_ascii=False) + "\n")
            else:
                out.write("%7.2fs %-5s %-62s %s %s\n" % (c["t"] - t0, c["seg"], c["path"][-62:], c.get("status"),
                                                       ("%dms" % round((c.get("dt") or 0) * 1000))))
        else:
            if a.path:
                continue
            if a.full or a.out:
                out.write(json.dumps(trunc(common.scrub({k: v for k, v in c.items() if k not in ("body", "chunk")})), ensure_ascii=False) + "\n")
            else:
                desc = c.get("label") or c.get("kind") or (c.get("msg", {}).get("method") if isinstance(c.get("msg"), dict) else "") or c.get("dir")
                out.write("%7.2fs %-5s %s %s\n" % (c["t"] - t0, c["seg"], c.get("dir", ""), desc))
    if a.out:
        print("已写入 %s" % a.out)


if __name__ == "__main__":
    main()
