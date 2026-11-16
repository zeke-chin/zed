"""不经过 LS，直连云端发一次**自己构造**的 GetTab，打印预测的修改。

用法（经 fork/spike/run）：
  run get-tab                       内置示例：在一段 Python 里把变量 total 改名为 subtotal（只改了第一处），看模型是否预测后续修改
  run get-tab --demo complete       内置示例：在函数末尾输入半行，看补全
  run get-tab --file a.py --line 10 --col 4 [--language python]   对任意文件、任意光标发一次（无编辑轨迹）
  选项：--json 打印原始增量；--full 每次发全量（不用轨迹缓存）

只读 .env 里的 WINDSURF_API_KEY；不读本机 Devin 登录数据。
"""
import argparse
import difflib
import os
import sys

import common
import tab_request

RENAME_BEFORE = '''def order_total(items, tax_rate):
    total = 0
    for item in items:
        total += item.price * item.quantity
    tax = total * tax_rate
    return total + tax


def describe(order):
    return f"{order.id}: {len(order.items)} items"
'''

COMPLETE_BEFORE = '''class Stack:
    def __init__(self):
        self.items = []

    def push(self, item):
        self.items.append(item)

    def pop(self):
        return self.items.pop()

    def peek(self):
'''


def demo_rename(sess, uri):
    """先「打开」文件，再模拟逐字把第 2 行的 total 改成 subtotal（每敲几个字记一步，像 LS 合并连续输入）。"""
    text = RENAME_BEFORE
    pos = text.index("total = 0")
    steps = ["sub"[:i] for i in range(1, 4)]
    prev = text
    for s in steps:
        cur = text[:pos] + s + text[pos:]
        sess.record_edit(uri, prev, cur)
        prev = cur
    cursor = pos + 3
    return prev, cursor


def demo_complete(sess, uri):
    text = COMPLETE_BEFORE
    typed = "        return self."
    new = text + typed
    sess.record_edit(uri, text, new)
    return new, len(new)


def show(text, res, uri):
    print("—— 预测 ——")
    if res.missing is not None:
        print("服务端仍然报 MissingInformation：%s" % res.missing)
        return
    if res.jump:
        print("跳转：行 %d 列 %d（path=%s）" % (res.jump[0], res.jump[1], res.path or uri))
    if res.noop:
        print("无预测（DeltaNoop）")
    if res.old_str or res.new_str:
        if res.old_str and res.old_str in text:
            new_text = text.replace(res.old_str, res.new_str, 1)
        else:
            print("（旧文本在文档里找不到，按原样打印）")
            print("--- old_str ---\n%s\n--- new_str ---\n%s" % (res.old_str, res.new_str))
            return
        a_lines = (text if text.endswith("\n") else text + "\n").splitlines(True)
        b_lines = (new_text if new_text.endswith("\n") else new_text + "\n").splitlines(True)
        diff = difflib.unified_diff(a_lines, b_lines, "当前", "预测后", n=1)
        sys.stdout.writelines(diff)
        print()
    if res.cursor:
        print("预测后光标：行 %d 列 %d" % res.cursor)
    print("首条增量 %.0f ms，完成 %.0f ms，%d 条增量，stop_reason=%s" % (
        (res.t_first or 0) * 1000, (res.t_done or 0) * 1000, res.n_deltas, res.stop_reason))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--demo", choices=["rename", "complete"], default="rename")
    ap.add_argument("--file")
    ap.add_argument("--line", type=int, default=0, help="从 0 开始")
    ap.add_argument("--col", type=int, default=0, help="字符列，从 0 开始")
    ap.add_argument("--language", default="python")
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--full", action="store_true")
    a = ap.parse_args()
    api_key = common.require_api_key()
    ws = "file:///tmp/spike-demo"
    sess = tab_request.TabSession(api_key, ws)
    sess.full_every_time = a.full
    if a.file:
        path = os.path.abspath(a.file)
        text = open(path).read()
        uri = "file://" + path
        lines = text.split("\n")
        cursor = sum(len(l) + 1 for l in lines[:a.line]) + a.col
        sess.workspace_uri = "file://" + os.path.dirname(path)
        # 服务端要求轨迹至少一步（空轨迹报 internal error），把光标前一个字符当作刚输入的
        if cursor > 0:
            sess.record_edit(uri, text[:cursor - 1] + text[cursor:], text)
    else:
        uri = ws + "/order.py"
        text, cursor = (demo_rename if a.demo == "rename" else demo_complete)(sess, uri)
    req = sess.build(uri, text, cursor, a.language)
    print("请求：%d 个轨迹步骤，文档 %d 字节，光标偏移 %d，api_key=%s，user_jwt=%s" % (
        len(sess.steps), len(text.encode()), cursor, common.mask(api_key), common.mask(sess.jwt)))
    res = sess.send(req)
    if a.json and res.pre is not None:
        import json
        print(json.dumps(common.to_json(res.pre), ensure_ascii=False, indent=1))
    show(text, res, uri)


if __name__ == "__main__":
    main()
