# -*- coding: utf-8 -*-
"""Repro: manual typing vs paste for the magic upload/download interception.

Starts fake_sshd, runs dist/sshu.exe inside ConPTY, then:
  A. types "upload C:\\tmp\\a.txt" one byte at a time (80ms gaps) -> Enter
  B. pastes "upload C:\\tmp\\a.txt\\r" as one burst
Reports what the server received and whether magic (sshu) mode triggered.
"""
import sys
import time
import threading

sys.path.insert(0, r"D:\tool\sshu-rs\tests")
sys.path.insert(0, r"D:\tool\sshu-rs\tests\repro")

from e2e_conpty import ConPTY  # noqa: E402
import fake_sshd  # noqa: E402

EXE = r"D:\tool\sshu-rs\dist\sshu.exe"
CMD = "upload C:\\tmp\\a.txt"
PORT = 2299

server_log = []
threading.Thread(target=fake_sshd.serve, args=(PORT, server_log), daemon=True).start()
time.sleep(0.5)

p = ConPTY([EXE, "--password", "111111", "-p", str(PORT), "root@127.0.0.1"],
           cols=100, rows=30)


def mark():
    return len(p.text())


def slice_from(m):
    return p.text()[m:]


def report(tag, m):
    seg = slice_from(m)
    magic = "(sshu)" in seg
    got = b"".join(server_log).decode("utf-8", "replace")
    print(f"[{tag}] magic_triggered={magic}")
    print(f"[{tag}] output_tail={seg[-300:]!r}")
    server_log.clear()
    return magic


def wait_banner():
    ok = p.wait_text("已连接", 20)
    print("banner:", ok)
    time.sleep(1.5)  # let first prompt+OSC7 arrive


try:
    wait_banner()

    # A. manual typing: one byte at a time
    m = mark()
    for ch in CMD:
        p.send(ch)
        time.sleep(0.08)
    time.sleep(0.3)
    p.send("\r")
    time.sleep(1.5)
    report("TYPE", m)

    # B. paste: whole line at once
    m = mark()
    p.send(CMD + "\r")
    time.sleep(1.5)
    report("PASTE", m)

    # C. type after arrow-key history recall (common real-world flow)
    m = mark()
    p.send(b"\x1b[A")
    time.sleep(0.5)
    for ch in CMD:
        p.send(ch)
        time.sleep(0.08)
    time.sleep(0.3)
    p.send("\r")
    time.sleep(1.5)
    report("TYPE_AFTER_ARROW", m)

    # D. type, erase all with backspace, retype (the reported bug)
    m = mark()
    for ch in "uplodX":
        p.send(ch)
        time.sleep(0.05)
    time.sleep(0.3)
    for _ in range(6):
        p.send(b"\x7f")
        time.sleep(0.05)
    time.sleep(0.3)
    for ch in CMD:
        p.send(ch)
        time.sleep(0.08)
    time.sleep(0.3)
    p.send("\r")
    time.sleep(1.5)
    report("TYPE_AFTER_ERASE", m)

    # E. arrow recall then Ctrl-U (clear line) then type
    m = mark()
    p.send(b"\x1b[A")
    time.sleep(0.5)
    p.send(b"\x15")
    time.sleep(0.3)
    for ch in CMD:
        p.send(ch)
        time.sleep(0.08)
    time.sleep(0.3)
    p.send("\r")
    time.sleep(1.5)
    report("TYPE_AFTER_ARROW_CTRL_U", m)
finally:
    p.close()
