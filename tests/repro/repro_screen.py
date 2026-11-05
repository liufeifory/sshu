# -*- coding: utf-8 -*-
"""屏幕级 e2e：用 pyte 解析 sshu 输出的最终画面，校验网格渲染正确性。

覆盖：
  1. 基础命令回显 + 输出
  2. top 进入/退出后命令仍可执行（历史 bug：渲染错乱、光标跑顶）
  3. less 进出
  4. vim 编辑保存后内容正确
  5. 长输出滚动后末行可见
  6. PgUp/PgDn 回滚历史
  7. 中文宽字符对齐
  8. 清屏命令

用法：python tests/repro/repro_screen.py
依赖：pip install pyte
"""
import os
import sys
import time

sys.path.insert(0, r"D:\tool\sshu-rs\tests")
from e2e_conpty import ConPTY  # noqa: E402

import pyte  # noqa: E402

EXE = os.path.join(os.path.dirname(os.path.dirname(os.path.dirname(
    os.path.abspath(__file__)))), "dist", "sshu.exe")
HOST, PASS = "192.168.32.139", "111111"
COLS, ROWS = 100, 30

results = []


def check(name, cond, detail=""):
    print(("[ok] " if cond else "[FAIL] ") + name + (("  " + detail) if detail else ""))
    results.append(bool(cond))


def main():
    p = ConPTY([EXE, "--password", PASS, f"root@{HOST}"], cols=COLS, rows=ROWS)
    screen = pyte.Screen(COLS, ROWS)
    stream = pyte.ByteStream(screen)
    state = {"n": 0}

    def sync():
        raw = p.text().encode("utf-8", "replace")
        if len(raw) > state["n"]:
            stream.feed(raw[state["n"]:])
            state["n"] = len(raw)

    def text():
        sync()
        return "\n".join(screen.display)

    def tail(n=3):
        sync()
        return [line.rstrip() for line in screen.display[-n:]]

    try:
        check("connected", p.wait_text("[root@", 25))
        time.sleep(2.0)

        # 1 基础命令
        p.send(b"echo grid_$((6*7))\r")
        time.sleep(1.2)
        check("basic command", "grid_42" in text())

        # 2 top 进出
        p.send(b"top\r")
        time.sleep(2.0)
        check("top rendered", "load average" in text())
        p.send(b"q")
        time.sleep(1.2)
        p.send(b"echo top_$((7*6))\r")
        time.sleep(1.2)
        check("command after top", "top_42" in text())
        last = [l for l in tail(3) if l.strip()]
        check("cursor back at prompt", bool(last) and "root@gauss1" in last[-1], last[-1] if last else "")

        # 3 less 进出
        p.send(b"seq 1 60 | less\r")
        time.sleep(1.2)
        p.send(b"q")
        time.sleep(1.0)
        p.send(b"echo less_$((5*8))\r")
        time.sleep(1.2)
        check("command after less", "less_40" in text())

        # 4 vim
        p.send(b"vim /tmp/sshu_screen.txt\r")
        time.sleep(1.5)
        p.send(b"ihello screen\x1b")
        time.sleep(0.5)
        p.send(b":wq\r")
        time.sleep(1.5)
        p.send(b"cat /tmp/sshu_screen.txt\r")
        time.sleep(1.2)
        check("vim roundtrip", "hello screen" in text())

        # 5 长输出
        p.send(b"seq 1 200\r")
        time.sleep(1.8)
        check("scroll tail", "200" in "\n".join(tail(4)), "\n".join(tail(4)))

        # 6 回滚
        sync()
        before = "\n".join(screen.display)
        p.send(b"\x1b[5~")
        time.sleep(1.0)
        after = text()
        check("PgUp shows history", after != before and "200" not in "\n".join(tail(3)))
        p.send(b"\x1b[6~")
        time.sleep(1.0)
        check("PgDn returns forward", text() != after)

        # 7 中文宽字符
        p.send("echo 中文宽字符OK\r".encode("utf-8"))
        time.sleep(1.2)
        check("cjk rendered", "中文宽字符OK" in text())

        # 8 清屏
        p.send(b"clear\r")
        time.sleep(1.0)
        check("clear works", "中文宽字符OK" not in text())

        # 9 清屏后仍可执行
        p.send(b"echo after_clear_$((3*4))\r")
        time.sleep(1.2)
        check("command after clear", "after_clear_12" in text())
    finally:
        p.close()

    print("RESULT:", "PASS" if all(results) else "FAIL")
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
