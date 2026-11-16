"""公共工具：.env 读取、LS 定位、打码、proto 动态加载、Connect 分帧。

注意：本模块**不读取**本机 Devin/Windsurf 登录数据，那只在 import_key.py 里做。
"""

import glob
import gzip
import json
import os
import struct
import sys

SPIKE_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ENV_PATH = os.path.join(SPIKE_DIR, ".env")
PROTO_OUT = os.path.join(SPIKE_DIR, "proto-out")
DEFAULT_API_SERVER_URL = "https://server.codeium.com"

LS_CANDIDATES = [
    "/Applications/Devin.app/Contents/Resources/app/extensions/windsurf/bin/language_server_macos_arm",
    "/Applications/Devin.app/Contents/Resources/app/extensions/windsurf/bin/language_server_macos_x64",
    "/Applications/Windsurf.app/Contents/Resources/app/extensions/windsurf/bin/language_server_macos_arm",
    "/Applications/Windsurf.app/Contents/Resources/app/extensions/windsurf/bin/language_server_macos_x64",
    os.path.expanduser("~/.codeium/windsurf/bin/language_server_*"),
    # 容器里用的 Linux 版（与 Mac 版同一构建，见 proto-out/VERSION.txt）
    os.path.join(SPIKE_DIR, ".cache/linux/reh/extensions/windsurf/bin/language_server_linux_*"),
]


# ---------------- .env ----------------

def load_env():
    env = {}
    if os.path.exists(ENV_PATH):
        with open(ENV_PATH) as fh:
            for line in fh:
                line = line.strip()
                if not line or line.startswith("#") or "=" not in line:
                    continue
                k, v = line.split("=", 1)
                env[k.strip()] = v.strip().strip('"').strip("'")
    return env


def write_env_value(key, value):
    lines = []
    if os.path.exists(ENV_PATH):
        with open(ENV_PATH) as fh:
            lines = fh.read().splitlines()
    else:
        tpl = os.path.join(SPIKE_DIR, ".env.example")
        if os.path.exists(tpl):
            with open(tpl) as fh:
                lines = fh.read().splitlines()
    done = False
    for i, line in enumerate(lines):
        if line.split("=", 1)[0].strip() == key:
            lines[i] = "%s=%s" % (key, value)
            done = True
    if not done:
        lines.append("%s=%s" % (key, value))
    fd = os.open(ENV_PATH, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as fh:
        fh.write("\n".join(lines) + "\n")
    os.chmod(ENV_PATH, 0o600)


def require_api_key():
    key = load_env().get("WINDSURF_API_KEY", "")
    if not key:
        sys.stderr.write(
            "WINDSURF_API_KEY 为空。两种写入方式：\n"
            "  1) 从本机导入：fork/spike/run import-key\n"
            "  2) 手动填写：编辑 fork/spike/.env（模板见 .env.example）\n"
        )
        sys.exit(2)
    return key


def api_server_url():
    return load_env().get("WINDSURF_API_SERVER_URL") or DEFAULT_API_SERVER_URL


def mask(s):
    if not s:
        return "<空>"
    s = str(s)
    if len(s) <= 12:
        return "***"
    return "%s…%s(len=%d)" % (s[:6], s[-4:], len(s))


def find_ls_path(verbose=True):
    p = load_env().get("WINDSURF_LS_PATH")
    if p:
        return p
    for c in LS_CANDIDATES:
        for hit in sorted(glob.glob(c)):
            if os.path.isfile(hit):
                if verbose:
                    sys.stderr.write("[ls] 找到 LS：%s\n" % hit)
                return hit
    sys.stderr.write("没找到 LS 二进制，请在 .env 里设置 WINDSURF_LS_PATH\n")
    sys.exit(2)


# ---------------- proto ----------------

_pool = None


def pool():
    global _pool
    if _pool is None:
        from google.protobuf import descriptor_pb2, descriptor_pool
        path = os.path.join(PROTO_OUT, "descriptor_set.pb")
        if not os.path.exists(path):
            sys.stderr.write("缺少 %s，先运行 fork/spike/extract-proto\n" % path)
            sys.exit(2)
        fds = descriptor_pb2.FileDescriptorSet()
        with open(path, "rb") as fh:
            fds.ParseFromString(fh.read())
        _pool = descriptor_pool.DescriptorPool()
        for f in fds.file:
            _pool.Add(f)
    return _pool


def msg_class(full_name):
    from google.protobuf import message_factory
    return message_factory.GetMessageClass(pool().FindMessageTypeByName(full_name))


def new(full_name, **kw):
    m = msg_class(full_name)()
    for k, v in kw.items():
        setattr(m, k, v)
    return m


def method(full_service, name):
    return pool().FindServiceByName(full_service).FindMethodByName(name)


def to_json(msg, **kw):
    from google.protobuf import json_format
    return json_format.MessageToDict(msg, preserving_proto_field_name=True, **kw)


def from_json(full_name, d):
    from google.protobuf import json_format
    m = msg_class(full_name)()
    json_format.ParseDict(d, m, ignore_unknown_fields=False)
    return m


# ---------------- Connect 分帧 ----------------
# Connect 流式：每帧 = 1 字节 flags + 4 字节大端长度 + 负载。
# flags bit0=压缩，bit1=结束帧（负载是 JSON：{"error":..., "metadata":...}）

def frame(payload, compressed=False, end=False):
    flags = (1 if compressed else 0) | (2 if end else 0)
    return struct.pack(">BI", flags, len(payload)) + payload


def iter_frames(data):
    i = 0
    while i + 5 <= len(data):
        flags, ln = struct.unpack(">BI", data[i:i + 5])
        payload = data[i + 5:i + 5 + ln]
        if len(payload) < ln:
            break
        yield flags, payload
        i += 5 + ln


class FrameReader:
    """增量解析 Connect 帧。"""

    def __init__(self):
        self.buf = b""

    def feed(self, chunk):
        self.buf += chunk
        out = []
        while len(self.buf) >= 5:
            flags, ln = struct.unpack(">BI", self.buf[:5])
            if len(self.buf) < 5 + ln:
                break
            payload = self.buf[5:5 + ln]
            self.buf = self.buf[5 + ln:]
            if flags & 1:
                payload = gzip.decompress(payload)
            out.append((flags, payload))
        return out


def scrub(obj, keys=("api_key", "apiKey", "auth_token", "authToken", "token", "access_token", "session_token", "user_jwt", "userJwt", "sessionToken", "session_token")):
    """递归把 dict 里的凭据字段打码（用于打印和保存示例）。"""
    if isinstance(obj, dict):
        return {k: (mask(v) if k in keys and isinstance(v, str) else scrub(v, keys)) for k, v in obj.items()}
    if isinstance(obj, list):
        return [scrub(x, keys) for x in obj]
    return obj


if __name__ == "__main__":
    if sys.argv[1:] == ["ls-path"]:
        print(find_ls_path())


def is_repeated(fd):
    """兼容新旧 protobuf：新版去掉了 FieldDescriptor.label。"""
    r = getattr(fd, "is_repeated", None)
    if r is not None:
        return r() if callable(r) else r
    return fd.label == 3
