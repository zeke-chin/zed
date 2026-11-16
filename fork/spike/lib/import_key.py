"""从本机 Devin/Windsurf 登录数据导入 api_key 到 fork/spike/.env。

这是验证阶段**唯一**读取本机登录数据的地方。

来源（按顺序尝试）：
  ~/Library/Application Support/Devin/User/globalStorage/state.vscdb
  ~/Library/Application Support/Windsurf/User/globalStorage/state.vscdb
表 ItemTable，key = 'windsurfAuthStatus'，值是 JSON，字段 apiKey（明文）。
"""
import json
import os
import sqlite3
import sys

import common

CANDIDATES = [
    "~/Library/Application Support/Devin/User/globalStorage/state.vscdb",
    "~/Library/Application Support/Windsurf/User/globalStorage/state.vscdb",
    "~/.config/Devin/User/globalStorage/state.vscdb",
    "~/.config/Windsurf/User/globalStorage/state.vscdb",
]
KEY = "windsurfAuthStatus"


def find_key():
    tried = []
    for c in CANDIDATES:
        path = os.path.expanduser(c)
        if not os.path.exists(path):
            tried.append("%s（不存在）" % path)
            continue
        try:
            conn = sqlite3.connect("file:%s?mode=ro" % path, uri=True)
            row = conn.execute("select value from ItemTable where key=?", (KEY,)).fetchone()
            conn.close()
        except Exception as e:
            tried.append("%s（读取失败：%s）" % (path, e))
            continue
        if not row:
            tried.append("%s（没有 %s，可能未登录）" % (path, KEY))
            continue
        val = row[0]
        if isinstance(val, bytes):
            val = val.decode()
        api_key = (json.loads(val) or {}).get("apiKey") or ""
        if not api_key:
            tried.append("%s（%s.apiKey 为空，可能已登出）" % (path, KEY))
            continue
        return path, api_key, tried
    return None, None, tried


def main():
    yes = "-y" in sys.argv or "--yes" in sys.argv
    src, key, tried = find_key()
    if not key:
        print("没有在本机找到 Devin/Windsurf 的登录数据：")
        for t in tried:
            print("  - " + t)
        print("请在 Devin 里登录后重试，或手动编辑 fork/spike/.env 填写 WINDSURF_API_KEY。")
        return 1
    print("来源：%s  [ItemTable.%s → apiKey]" % (src, KEY))
    print("密钥：%s" % common.mask(key))
    old = common.load_env().get("WINDSURF_API_KEY", "")
    if old == key:
        print(".env 里已是同一个密钥，无需改动。")
        return 0
    if old and not yes:
        ans = input(".env 里已有 WINDSURF_API_KEY=%s，覆盖吗？[y/N] " % common.mask(old)).strip().lower()
        if ans not in ("y", "yes"):
            print("未覆盖。")
            return 1
    common.write_env_value("WINDSURF_API_KEY", key)
    print("已写入 %s" % common.ENV_PATH)
    return 0


if __name__ == "__main__":
    sys.exit(main())
