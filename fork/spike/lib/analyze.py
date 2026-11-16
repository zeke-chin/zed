"""分析一次抓包：把 IDE→LS 的 HandleStreamingTabV2 和 LS→云端的 GetTab / GetChatMessage 按 trigger_id 对应起来，
列出每次触发里各类上下文的数量和大小。用于 结论.md 的上下文对照表。

用法：run analyze [session目录|latest] [--json]
"""
import argparse
import collections
import gzip
import json

import capture_util
import common
from decode import build, header, load, method_types


def resp_frames(c, out_type):
    body = b"".join(ch for _, ch in c["chunks"])
    hs = c.get("resp_headers") or []
    if header(hs, "content-encoding") == "gzip":
        body = gzip.decompress(body)
    out = []
    if (header(hs, "content-type") or "").startswith("application/connect"):
        for flags, p in common.iter_frames(body):
            if flags & 1:
                p = gzip.decompress(p)
            if flags & 2:
                out.append({"_end": json.loads(p or b"{}")})
            else:
                m = common.msg_class(out_type)()
                m.ParseFromString(p)
                out.append(common.to_json(m))
    elif body:
        m = common.msg_class(out_type)()
        m.ParseFromString(body)
        out.append(common.to_json(m))
    return out


def step_summary(steps):
    cnt = collections.Counter(s.get("type", "?").replace("CORTEX_STEP_TYPE_", "") for s in steps)
    return dict(cnt)


def size(o):
    return len(json.dumps(o, ensure_ascii=False).encode())


def analyze_gettab(req, resp):
    upc = req.get("unified_prompt_components", {})
    ct = upc.get("cortex_trajectory", {})
    tsv2 = req.get("trajectory_steps_v2", {})
    ci = upc.get("command_intent_info", {})
    doc = ci.get("input_document", {})
    kinds = collections.Counter()
    for r in resp:
        for k in r:
            if k == "streaming_completion_v2":
                d = r[k]
                kinds[next((x for x in ("delta_supercomplete", "delta_tab_jump", "delta_noop") if x in d), "complete")] += 1
            else:
                kinds[k] += 1
    pre = next((r["pre_response"] for r in resp if "pre_response" in r), {})
    return {
        "trigger_id": req.get("metadata", {}).get("trigger_id"),
        "provider_source": req.get("provider_source"),
        "model": req.get("model"),
        "cortex_steps": step_summary(ct.get("steps", [])),
        "parents": [p.get("trajectory_type", "").replace("CORTEX_TRAJECTORY_TYPE_", "") for p in ct.get("parent_references", [])],
        "hashes": len(tsv2.get("hashes", [])),
        "steps_after_hashes": step_summary(tsv2.get("steps_after_hashes", [])),
        "doc_bytes": len(doc.get("text", "").encode()),
        "selection_lines": [ci.get("selection_start_line"), ci.get("selection_end_line")],
        "cursor_offset": doc.get("cursor_offset"),
        "cursor_position": doc.get("cursor_position"),
        "other_documents": len(upc.get("other_documents", [])),
        "other_documents_bytes": sum(len(d.get("text", "").encode()) for d in upc.get("other_documents", [])),
        "code_diagnostics": len(upc.get("code_diagnostics", [])),
        "context_module_result": "context_module_result" in upc,
        "feedback": len(upc.get("supercomplete_feedback_context", {}).get("feedback", [])),
        "chat_messages": len(upc.get("chat_messages", [])),
        "experiments": [e.get("key_string") for e in req.get("experiment_config", {}).get("experiments", [])],
        "resp": dict(kinds),
        "cache_hashes": len(pre.get("cache_info_v2", {}).get("hashes", [])),
        "missing": next((r["missing_information"] for r in resp if "missing_information" in r), None),
        "error": next((r["_end"].get("error") for r in resp if "_end" in r and r["_end"].get("error")), None),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("session", nargs="?", default="latest")
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args()
    s = capture_util.sessions()[-1] if a.session == "latest" else a.session
    order = build(load(s))
    label = None
    rows = []
    counts = collections.Counter()
    for c in order:
        if c.get("seg") == "mark":
            label = c.get("label") or label
            continue
        if c.get("seg") == "ide" and c["path"].endswith("HandleStreamingTabV2"):
            it, ot, _ = method_types(c["path"])
            req = common.msg_class(it)()
            req.ParseFromString(c["req_body"])
            r = common.to_json(req)
            rows.append({"seg": "ide", "label": label, "trigger_id": r.get("metadata", {}).get("trigger_id"),
                         "status": c.get("status"), "ms": round((c.get("dt") or 0) * 1000),
                         "fields": sorted(k for k in r if k != "metadata")})
        if c.get("seg") != "cloud":
            continue
        counts[c["path"].rsplit("/", 1)[-1]] += 1
        if c["path"].endswith("/GetTab") or c["path"].endswith("/GetChatMessage"):
            it, ot, _ = method_types(c["path"])
            reqs = capture_util.request_payloads(c)
            if not reqs:
                continue
            m = common.msg_class(it)()
            m.ParseFromString(reqs[0])
            req = common.to_json(m)
            resp = resp_frames(c, ot) if c.get("status") == 200 else []
            if c["path"].endswith("/GetTab"):
                row = analyze_gettab(req, resp)
            else:
                row = {"trigger_id": req.get("metadata", {}).get("trigger_id"),
                       "keys": sorted(req.keys()), "bytes": len(reqs[0]), "resp_frames": len(resp)}
            row.update({"seg": "cloud", "method": c["path"].rsplit("/", 1)[-1], "label": label,
                        "ttfb_ms": round((c.get("ttfb") or 0) * 1000), "total_ms": round((c.get("dt") or 0) * 1000)})
            rows.append(row)
    if a.json:
        print(json.dumps({"rows": rows, "cloud_counts": counts}, ensure_ascii=False, indent=1))
        return
    for r in rows:
        print(json.dumps(r, ensure_ascii=False))
    print("\n云端调用次数：")
    for k, v in counts.most_common():
        print("  %-40s %d" % (k, v))


if __name__ == "__main__":
    main()
