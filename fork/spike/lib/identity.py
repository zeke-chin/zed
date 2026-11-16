"""客户端标识：集中定义官方 IDE 扩展会填进 Metadata 的值（对齐 extension.js 的 MetadataProvider.getMetadata）。

官方取值方式（Devin 3.10.48 / 扩展 extension.js）：
  ide_name / extension_name = "windsurf"
  ide_version       = product.json 的 windsurfVersion（例如 3.10.48）
  extension_version = product.json 的 codeiumVersion（例如 1.48.2，即 LS 版本）
  extension_path    = 扩展目录
  session_id        = 每次 IDE 进程启动生成的 uuid4
  request_id        = 进程内自增
  locale            = VS Code 的 env.language
  os                = "mac" / "linux" / "windows"
  device_fingerprint= sha512(macs + serial + username)，键按字母序拼接；serial 长度>=4 时 macs 为空
  plan_name         = 用户状态里的套餐名，未知时 "Unset"
  trigger_id        = 本次触发的 uuid（Tab 请求会带）
"""
import hashlib
import json
import os
import platform
import subprocess
import uuid

import common

APP_ROOT = "/Applications/Devin.app/Contents/Resources/app"
FALLBACK = {"windsurfVersion": "3.10.48", "codeiumVersion": "1.48.2"}

_session_id = str(uuid.uuid4())
_request_id = 0


def product():
    try:
        with open(os.path.join(APP_ROOT, "product.json")) as fh:
            p = json.load(fh)
        return {"windsurfVersion": p.get("windsurfVersion", FALLBACK["windsurfVersion"]),
                "codeiumVersion": p.get("codeiumVersion", FALLBACK["codeiumVersion"])}
    except Exception:
        return dict(FALLBACK)


def os_name():
    s = platform.system()
    return {"Darwin": "mac", "Linux": "linux", "Windows": "windows"}.get(s, "unknown")


def device_fingerprint():
    serial = ""
    try:
        out = subprocess.run(["ioreg", "-rd1", "-c", "IOPlatformExpertDevice"], capture_output=True, text=True).stdout
        for line in out.splitlines():
            if "IOPlatformSerialNumber" in line:
                serial = line.split("=", 1)[1].strip().strip('"')
    except Exception:
        pass
    macs = ""
    if len(serial) < 4:
        macs = ""  # 官方用 systeminformation.uuid().macs，这里不再模拟
    try:
        user = os.getlogin()
    except Exception:
        user = os.environ.get("USER", "unknown")
    parts = {"macs": macs, "serial": serial, "username": user}
    s = "".join(parts[k] for k in sorted(parts))
    return hashlib.sha512(s.encode()).hexdigest()


_fp = None


def metadata(api_key, trigger_id="", plan_name="Unset", locale="en", team_id=""):
    """构造一份与官方扩展一致的 Metadata。"""
    global _request_id, _fp
    _request_id += 1
    if _fp is None:
        _fp = device_fingerprint()
    p = product()
    m = common.new("exa.codeium_common_pb.Metadata")
    m.ide_name = "windsurf"
    m.ide_version = p["windsurfVersion"]
    m.extension_name = "windsurf"
    m.extension_version = p["codeiumVersion"]
    m.extension_path = os.path.join(APP_ROOT, "extensions/windsurf")
    m.api_key = api_key
    m.session_id = _session_id
    m.request_id = _request_id
    m.locale = locale
    m.device_fingerprint = _fp
    m.plan_name = plan_name
    if trigger_id:
        m.trigger_id = trigger_id
    m.os = os_name()
    if team_id:
        m.team_id = team_id
    return m
