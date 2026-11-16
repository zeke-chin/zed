"""延迟测量（低频、串行，每次触发间隔 >= 2.5 秒）。

  run latency direct [-n 20]   直连云端：模拟逐字输入，每输入一个字符触发一次自构造 GetTab（带轨迹缓存）
  run latency ls [-n 20]       经过 LS：同样的输入节奏驱动官方 LS（LS 直连云端，不经录制代理），计 HandleStreamingTabV2 耗时
                               （输入沿着预测走时几乎全部命中 LS 本地缓存，1–5 ms）
  run latency ls-diverge       经过 LS：每次插入预测不到的内容，测 LS 实际调云端的耗时

结果写到 .scratch/latency-<模式>.json，并打印 P50/P90。
"""
import argparse
import json
import os
import statistics
import time

import common

TEXT = '''import math


def mean(xs):
    return sum(xs) / len(xs)


def variance(xs):
    m = mean(xs)
    return sum((x - m) ** 2 for x in xs) / len(xs)


'''
TYPED = "def stddev(xs):\n    return math.sqrt(variance(xs))\n\n\ndef median(xs):\n    s = sorted(xs)\n"
INTERVAL = 2.5


def pct(xs, p):
    xs = sorted(xs)
    if not xs:
        return None
    k = (len(xs) - 1) * p
    f = int(k)
    c = min(f + 1, len(xs) - 1)
    return xs[f] + (xs[c] - xs[f]) * (k - f)


def report(name, rows, keys):
    print("== %s：%d 次" % (name, len(rows)))
    for k in keys:
        xs = [r[k] for r in rows if r.get(k) is not None]
        if xs:
            print("  %-14s P50 %6.0f ms  P90 %6.0f ms  min %5.0f  max %5.0f  (n=%d)" % (
                k, pct(xs, .5), pct(xs, .9), min(xs), max(xs), len(xs)))


def direct(n):
    import tab_request
    sess = tab_request.TabSession(common.require_api_key(), "file:///tmp/spike-latency")
    uri = "file:///tmp/spike-latency/stats.py"
    text = TEXT
    rows = []
    i = 0
    while len(rows) < n and i < len(TYPED):
        ch = TYPED[i]
        new = text + ch
        sess.record_edit(uri, text, new)
        text = new
        i += 1
        if ch in " \n":  # 空白处不触发，和打字时触发频率接近
            continue
        t0 = time.time()
        req = sess.build(uri, text, len(text), "python")
        res = sess.send(req)
        rows.append({"first_ms": (res.t_first or 0) * 1000, "done_ms": (res.t_done or 0) * 1000,
                     "wall_ms": (time.time() - t0) * 1000, "deltas": res.n_deltas,
                     "hashes_sent": len(req.trajectory_steps_v2.hashes)})
        print("  #%d first %.0f ms done %.0f ms hashes %d" % (len(rows), rows[-1]["first_ms"], rows[-1]["done_ms"],
                                                           rows[-1]["hashes_sent"]))
        time.sleep(INTERVAL)
    report("直连", rows, ["first_ms", "done_ms"])
    return rows


def via_ls_diverge(n):
    """每次触发前一次性插入一行预测不到的内容（随机名字），避开 LS 的本地预测缓存，测 LS 实际调云端的耗时。"""
    import random
    from ls_driver import CAPTURES, LSSession, prepare_workspace, refresh, refresh_startup, tab
    ws = prepare_workspace("ws-latency")
    with open(os.path.join(ws, "stats.py"), "w") as fh:
        fh.write(TEXT)
    sess_dir = os.path.join(CAPTURES, time.strftime("%Y%m%d-%H%M%S") + "-latency-ls")
    sess = LSSession(sess_dir, ws, common.require_api_key(), record_cloud=False).start()
    rows = []
    rnd = random.Random(7)
    try:
        refresh_startup(sess)
        sess.open("stats.py", "python")
        refresh(sess, "stats.py", 0, 4)
        time.sleep(2)
        off = len(TEXT)
        for _ in range(n):
            name = "".join(rnd.choice("abcdefghjkmnpqrstuvwxyz") for _ in range(6))
            line = "%s_%d = %s(" % (name, rnd.randint(10, 99), rnd.choice(["mean", "variance", "len", "sorted"]))
            sess.edit("stats.py", off, 0, line)
            off += len(line)
            refresh(sess, "stats.py", off, 2)
            try:
                resp, dt, _ = tab(sess, "stats.py", off, label="latency", timeout=15)
                kind = resp.WhichOneof("suggestion")
            except Exception as e:
                dt, kind = None, "error:" + type(e).__name__
            rows.append({"done_ms": dt * 1000 if dt is not None else None, "kind": kind})
            print("  #%d %s %s" % (len(rows), "%.0f ms" % rows[-1]["done_ms"] if dt is not None else "-", kind))
            sess.edit("stats.py", off, 0, "xs)\n")
            off += 4
            time.sleep(INTERVAL)
    finally:
        sess.stop()
    report("经过 LS（每次输入预测不到的内容）", rows, ["done_ms"])
    return rows


def via_ls(n):
    from ls_driver import CAPTURES, LSSession, prepare_workspace, refresh, refresh_startup, tab
    ws = prepare_workspace("ws-latency")
    with open(os.path.join(ws, "stats.py"), "w") as fh:
        fh.write(TEXT)
    sess_dir = os.path.join(CAPTURES, time.strftime("%Y%m%d-%H%M%S") + "-latency-ls")
    sess = LSSession(sess_dir, ws, common.require_api_key(), record_cloud=False).start()
    rows = []
    try:
        refresh_startup(sess)
        sess.open("stats.py", "python")
        refresh(sess, "stats.py", 0, 4)
        time.sleep(2)
        off = len(TEXT)
        i = 0
        while len(rows) < n and i < len(TYPED):
            ch = TYPED[i]
            sess.edit("stats.py", off, 0, ch)
            off += 1
            i += 1
            if ch in " \n":
                continue
            refresh(sess, "stats.py", off, 2)
            try:
                resp, dt, _ = tab(sess, "stats.py", off, label="latency", timeout=15)
                kind = resp.WhichOneof("suggestion")
            except Exception as e:
                dt, kind = None, "error:" + type(e).__name__
            rows.append({"done_ms": dt * 1000 if dt is not None else None, "kind": kind})
            print("  #%d %s %s" % (len(rows), "%.0f ms" % rows[-1]["done_ms"] if dt is not None else "-", kind))
            time.sleep(INTERVAL)
    finally:
        sess.stop()
    report("经过 LS（IDE 侧看到的完成时间）", rows, ["done_ms"])
    cold = [r for r in rows if r.get("done_ms") and r["done_ms"] > 50]
    report("经过 LS（去掉 <50ms 的本地缓存命中）", cold, ["done_ms"])
    return rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("mode", choices=["direct", "ls", "ls-diverge"])
    ap.add_argument("-n", type=int, default=20)
    a = ap.parse_args()
    rows = {"direct": direct, "ls": via_ls, "ls-diverge": via_ls_diverge}[a.mode](a.n)
    out = os.path.join(common.SPIKE_DIR, ".scratch", "latency-%s.json" % a.mode)
    with open(out, "w") as fh:
        json.dump(rows, fh, indent=1)
    print("已写入 %s" % out)


if __name__ == "__main__":
    main()
