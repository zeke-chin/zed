"""从 Go 编译的 LS 二进制里提取内嵌的 protobuf 文件描述符，还原 .proto。

原理：protoc-gen-go 生成的代码把每个 .proto 的 FileDescriptorProto 序列化后作为
字符串常量（rawDesc）编进二进制。这里扫描所有 ".proto" 字样，向前找到
`0x0A <len> <name>` 形式的字段 1，再按 FileDescriptorProto 的字段集合向后走 wire
格式确定消息边界，最后用 protobuf 库解析并验证。

输出：
  <out>/descriptor_set.pb   全部文件的 FileDescriptorSet（后续脚本用它动态编解码）
  <out>/proto/<path>.proto  还原出的 .proto 文本（只写 exa.* 包及其依赖）
  <out>/VERSION.txt         二进制路径、大小、sha256、版本信息
"""

import argparse
import hashlib
import os
import subprocess
import sys

from google.protobuf import descriptor_pb2

# FileDescriptorProto 字段号 -> 期望的 wire type
FDP_FIELDS = {1: 2, 2: 2, 3: 2, 4: 2, 5: 2, 6: 2, 7: 2, 8: 2, 9: 2, 10: 0, 11: 0, 12: 2, 13: 0, 14: 2, 15: 2}
SINGULAR = {1, 2, 8, 9, 12, 13}


def read_varint(buf, i):
    shift = 0
    val = 0
    while True:
        if i >= len(buf) or shift > 63:
            return None, i
        b = buf[i]
        i += 1
        val |= (b & 0x7F) << shift
        if not b & 0x80:
            return val, i
        shift += 7


def walk_fdp(buf, start):
    """从 start 开始按 FileDescriptorProto 字段集合走 wire 格式，返回结束位置。"""
    i = start
    seen = set()
    n = len(buf)
    while i < n:
        tag, j = read_varint(buf, i)
        if tag is None or tag == 0:
            break
        field, wt = tag >> 3, tag & 7
        if field not in FDP_FIELDS or FDP_FIELDS[field] != wt:
            break
        if field in SINGULAR and field in seen:
            break
        if wt == 2:
            ln, k = read_varint(buf, j)
            if ln is None or k + ln > n:
                break
            nxt = k + ln
        else:
            v, k = read_varint(buf, j)
            if v is None:
                break
            nxt = k
        seen.add(field)
        i = nxt
    return i


def scan(buf):
    found = {}
    pos = 0
    needle = b".proto"
    while True:
        p = buf.find(needle, pos)
        if p < 0:
            break
        pos = p + 1
        end_name = p + len(needle)
        # 向前找 0x0A <len>，len 覆盖到 .proto 结尾
        for back in range(1, 256):
            s = end_name - back  # name 起点
            if s - 2 < 0:
                break
            # 单字节长度
            if buf[s - 2] == 0x0A and buf[s - 1] == back:
                start = s - 2
            elif back >= 128 and s - 3 >= 0 and buf[s - 3] == 0x0A and buf[s - 2] == ((back & 0x7F) | 0x80) and buf[s - 1] == back >> 7:
                start = s - 3
            else:
                continue
            name = buf[s:end_name]
            if not all(32 < c < 127 for c in name):
                continue
            end = walk_fdp(buf, start)
            if end - start < 20:
                continue
            fdp = descriptor_pb2.FileDescriptorProto()
            try:
                fdp.ParseFromString(buf[start:end])
            except Exception:
                continue
            if fdp.name != name.decode():
                continue
            if not (fdp.message_type or fdp.enum_type or fdp.service or fdp.extension):
                continue
            old = found.get(fdp.name)
            if old is None or len(fdp.SerializeToString()) > len(old.SerializeToString()):
                found[fdp.name] = fdp
            break
    return found


# ---------------- .proto 文本输出 ----------------

TYPE_NAMES = {
    1: "double", 2: "float", 3: "int64", 4: "uint64", 5: "int32", 6: "fixed64", 7: "fixed32",
    8: "bool", 9: "string", 12: "bytes", 13: "uint32", 15: "sfixed32", 16: "sfixed64", 17: "sint32", 18: "sint64",
}


def field_type(f):
    if f.type in (11, 14, 10):  # message / enum / group
        return f.type_name
    return TYPE_NAMES[f.type]


def rel_name(type_name, pkg):
    # 去掉前导点；同包时去掉包前缀，便于阅读
    t = type_name.lstrip(".")
    if pkg and t.startswith(pkg + "."):
        return t[len(pkg) + 1:]
    return t


class Printer:
    def __init__(self, fdp):
        self.fdp = fdp
        self.pkg = fdp.package
        self.out = []
        self.proto3 = fdp.syntax == "proto3"

    def w(self, depth, s):
        self.out.append("  " * depth + s)

    def opts_str(self, f):
        o = []
        if f.options.HasField("deprecated") and f.options.deprecated:
            o.append("deprecated = true")
        if f.options.HasField("packed"):
            o.append("packed = %s" % ("true" if f.options.packed else "false"))
        if f.HasField("json_name") and False:
            pass
        return (" [" + ", ".join(o) + "]") if o else ""

    def map_entry(self, msg, type_name):
        short = type_name.split(".")[-1]
        for nt in msg.nested_type:
            if nt.name == short and nt.options.map_entry:
                return nt
        return None

    def field_line(self, msg, f, depth, in_oneof):
        label = ""
        if f.label == 3:
            entry = self.map_entry(msg, f.type_name) if f.type == 11 else None
            if entry:
                k, v = entry.field[0], entry.field[1]
                vt = rel_name(field_type(v), self.pkg) if v.type in (11, 14) else field_type(v)
                self.w(depth, "map<%s, %s> %s = %d%s;" % (field_type(k), vt, f.name, f.number, self.opts_str(f)))
                return
            label = "repeated "
        elif not in_oneof:
            if self.proto3:
                if f.proto3_optional:
                    label = "optional "
            elif self.fdp.syntax == "editions":
                label = ""
            else:
                label = "required " if f.label == 2 else "optional "
        t = field_type(f)
        if f.type in (11, 14, 10):
            t = rel_name(t, self.pkg)
        self.w(depth, "%s%s %s = %d%s;" % (label, t, f.name, f.number, self.opts_str(f)))

    def enum(self, e, depth):
        self.w(depth, "enum %s {" % e.name)
        for v in e.value:
            dep = " [deprecated = true]" if v.options.deprecated else ""
            self.w(depth + 1, "%s = %d%s;" % (v.name, v.number, dep))
        self.w(depth, "}")

    def message(self, m, depth):
        self.w(depth, "message %s {" % m.name)
        for nt in m.nested_type:
            if nt.options.map_entry:
                continue
            self.message(nt, depth + 1)
        for e in m.enum_type:
            self.enum(e, depth + 1)
        real_oneofs = {}
        for f in m.field:
            if f.HasField("oneof_index") and not f.proto3_optional:
                real_oneofs.setdefault(f.oneof_index, []).append(f)
        emitted = set()
        for f in m.field:
            if f.HasField("oneof_index") and not f.proto3_optional:
                idx = f.oneof_index
                if idx in emitted:
                    continue
                emitted.add(idx)
                self.w(depth + 1, "oneof %s {" % m.oneof_decl[idx].name)
                for of in real_oneofs[idx]:
                    self.field_line(m, of, depth + 2, True)
                self.w(depth + 1, "}")
            else:
                self.field_line(m, f, depth + 1, False)
        for r in m.reserved_range:
            self.w(depth + 1, "reserved %d to %d;" % (r.start, r.end - 1) if r.end - 1 != r.start else "reserved %d;" % r.start)
        if m.reserved_name:
            self.w(depth + 1, "reserved %s;" % ", ".join('"%s"' % n for n in m.reserved_name))
        self.w(depth, "}")

    def service(self, s):
        self.w(0, "service %s {" % s.name)
        for m in s.method:
            cs = "stream " if m.client_streaming else ""
            ss = "stream " if m.server_streaming else ""
            self.w(1, "rpc %s(%s%s) returns (%s%s);" % (m.name, cs, rel_name(m.input_type, self.pkg), ss, rel_name(m.output_type, self.pkg)))
        self.w(0, "}")

    def render(self):
        f = self.fdp
        self.w(0, "// 由 fork/spike/extract-proto 从官方 LS 二进制还原，请勿手改")
        if f.syntax == "editions":
            self.w(0, 'edition = "%s";' % f.edition)
        else:
            self.w(0, 'syntax = "%s";' % (f.syntax or "proto2"))
        self.w(0, "")
        if f.package:
            self.w(0, "package %s;" % f.package)
            self.w(0, "")
        for d in f.dependency:
            self.w(0, 'import "%s";' % d)
        if f.options.go_package:
            self.w(0, 'option go_package = "%s";' % f.options.go_package)
        self.w(0, "")
        for e in f.enum_type:
            self.enum(e, 0)
            self.w(0, "")
        for m in f.message_type:
            self.message(m, 0)
            self.w(0, "")
        for s in f.service:
            self.service(s)
            self.w(0, "")
        return "\n".join(self.out) + "\n"


def closure(found, roots):
    keep = set()
    stack = list(roots)
    while stack:
        n = stack.pop()
        if n in keep or n not in found:
            continue
        keep.add(n)
        stack.extend(found[n].dependency)
    return keep


def topo(found, names):
    order, seen = [], set()

    def visit(n):
        if n in seen or n not in found:
            return
        seen.add(n)
        for d in found[n].dependency:
            visit(d)
        order.append(n)

    for n in sorted(names):
        visit(n)
    return order


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("binary")
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    with open(args.binary, "rb") as fh:
        buf = fh.read()
    found = scan(buf)
    exa_roots = [n for n, f in found.items() if f.package.startswith("exa.")]
    keep = closure(found, exa_roots)
    order = topo(found, keep)

    os.makedirs(os.path.join(args.out, "proto"), exist_ok=True)
    fds = descriptor_pb2.FileDescriptorSet()
    for n in order:
        fds.file.append(found[n])
    with open(os.path.join(args.out, "descriptor_set.pb"), "wb") as fh:
        fh.write(fds.SerializeToString())
    for n in order:
        path = os.path.join(args.out, "proto", n)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as fh:
            fh.write(Printer(found[n]).render())

    version = ""
    try:
        r = subprocess.run([args.binary, "--stamp"], capture_output=True, text=True, timeout=20)
        version = (r.stdout + r.stderr).strip()
    except Exception as e:  # 非本机架构的二进制无法执行
        version = "无法执行 --stamp: %s" % e
    sha = hashlib.sha256(buf).hexdigest()
    with open(os.path.join(args.out, "VERSION.txt"), "w") as fh:
        fh.write("binary: %s\nsize: %d\nsha256: %s\n\n--stamp 输出:\n%s\n" % (args.binary, len(buf), sha, version))

    # 校验 GetTab 相关消息
    from google.protobuf import descriptor_pool
    pool = descriptor_pool.DescriptorPool()
    for f in fds.file:
        pool.Add(f)
    missing = []
    for full in ["exa.api_server_pb.GetTabRequest", "exa.api_server_pb.GetTabResponse"]:
        try:
            pool.FindMessageTypeByName(full)
        except KeyError:
            missing.append(full)
    print("扫描到 %d 个 proto 文件，exa.* 及依赖 %d 个，已写入 %s" % (len(found), len(order), args.out))
    print("sha256=%s" % sha)
    if missing:
        print("警告：没找到 %s（可能包名变了，用 grep GetTab 在 proto/ 里找）" % missing)
        return 1
    print("GetTabRequest / GetTabResponse 已找到，依赖可完整解析")
    return 0


if __name__ == "__main__":
    sys.exit(main())
