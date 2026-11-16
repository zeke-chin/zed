"""打印某个消息及其引用到的消息结构（调试/阅读 proto 用）。

用法：python lib/show_msg.py exa.api_server_pb.GetTabRequest [--depth N] [--skip 前缀,...]
"""
import argparse

from google.protobuf.descriptor import FieldDescriptor

import common

TN = {v: k[5:].lower() for k, v in vars(FieldDescriptor).items() if k.startswith("TYPE_")}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("names", nargs="+")
    ap.add_argument("--depth", type=int, default=3)
    ap.add_argument("--skip", default="")
    a = ap.parse_args()
    skip = [s for s in a.skip.split(",") if s]
    seen = set()
    queue = [(n, 0) for n in a.names]
    while queue:
        name, d = queue.pop(0)
        if name in seen or any(name.startswith(s) for s in skip):
            continue
        seen.add(name)
        try:
            m = common.pool().FindMessageTypeByName(name)
        except KeyError:
            try:
                e = common.pool().FindEnumTypeByName(name)
                print("enum %s { %s }\n" % (name, ", ".join("%s=%d" % (v.name, v.number) for v in e.values)))
            except KeyError:
                print("?? %s" % name)
            continue
        print("message %s {" % name)
        for f in m.fields:
            t = TN[f.type]
            if f.message_type:
                t = f.message_type.full_name
                if d + 1 <= a.depth and not f.message_type.GetOptions().map_entry:
                    queue.append((t, d + 1))
                if f.message_type.GetOptions().map_entry:
                    kv = f.message_type.fields
                    vt = kv[1].message_type.full_name if kv[1].message_type else TN[kv[1].type]
                    t = "map<%s,%s>" % (TN[kv[0].type], vt)
            elif f.enum_type:
                t = f.enum_type.full_name
                if d + 1 <= a.depth:
                    queue.append((t, d + 1))
            lab = "repeated " if common.is_repeated(f) and not t.startswith("map<") else ""
            oo = (" (oneof %s)" % f.containing_oneof.name) if f.containing_oneof and not f.containing_oneof.name.startswith("_") else ""
            print("  %s%s %s = %d%s" % (lab, t, f.name, f.number, oo))
        print("}\n")


if __name__ == "__main__":
    main()
