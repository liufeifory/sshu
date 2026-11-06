# -*- coding: utf-8 -*-
"""探针：测量回滚深度 + 观察滚轮行为。"""
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

    def top_lines(n=2):
        sync()
        return [l.rstrip() for l in screen.display[:n]]

    try:
        print("connected:", p.wait_text("[root@", 25))
        time.sleep(2.0)
        p.send(b"seq 1 30000\r")
        time.sleep(6.0)
        print("bottom:", [l.rstrip() for l in screen.display[-3:]][:3])

        for i in range(1, 401):
            p.send(b"\x1b[5~")
            time.sleep(0.02)
            if i in (1, 1, 2, 10, 50, 100, 200, 400):
                time.sleep(0.35)
                print(f"PgUp x{i}: top={top_lines(1)}")

        # 一路滚到顶，确认历史深度
        for _ in range(800):
            p.send(b"\x1b[5~")
            time.sleep(0.01)
        time.sleep(1.0)
        print("top after 1200 pages:", top_lines(2))
        print("hint bar:", [l.rstrip() for l in screen.display[-1:]])

        # 滚轮（SGR 鼠标）
        p.send(b"\x1b[<65;10;10M")
        time.sleep(0.4)
        print("wheel-down:", top_lines(2))
        p.send(b"\x1b[<64;10;10M")
        time.sleep(0.4)
        print("wheel-up:", top_lines(2))
        print("screen has escape garbage:", "[" in "\n".join(top_lines(5)))
    finally:
        p.close()


if __name__ == "__main__":
    main()
