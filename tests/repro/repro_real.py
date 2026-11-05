# -*- coding: utf-8 -*-
"""Real-server e2e: put/get/lcd/lll magic interception on 192.168.32.139.

Scenarios (all at a real bash prompt with the OSC7 hook installed):
  1. TYPE      : type `put <local>` one key at a time -> expect (sshu) + file uploaded
  1.5 SLOW-TYPE: 0.8s key gap -> must still intercept (no idle flush)
  2. ERASE     : type garbage, backspace all, type put -> expect magic
  3. PASTE     : paste put line as one burst -> expect magic
  3.5 PUT+enter (no space) -> local usage hint, never leaked to bash
  4. ARROW     : press Up (history), then type put -> expect passthrough (by design)
  5. LCD       : type `lcd /tmp` -> local dir message, no bash round-trip
  6. LLL       : type `lll` -> local listing appears, no bash round-trip
  7. GET       : `get /root/e2e_src.txt` via prefix mode -> local file appears
"""
import os
import sys
import time

sys.path.insert(0, r"D:\tool\sshu-rs\tests")
from e2e_conpty import ConPTY  # noqa: E402

import paramiko  # noqa: E402

EXE = r"D:\tool\sshu-rs\dist\sshu.exe"
HOST, PORT, USER, PASS = "192.168.32.139", 22, "root", "111111"
CMD = "put {src}"

src = os.path.join(os.path.dirname(os.path.abspath(__file__)), "e2e_src.txt")
with open(src, "wb") as fh:
    fh.write(b"real server e2e\n")
tmpdir = os.environ.get("TEMP", ".")
local_get = os.path.join(tmpdir, "e2e_src.txt")
for f in (local_get, os.path.join(tmpdir, "sshu_e2e_get.txt")):
    if os.path.exists(f):
        os.remove(f)

results = []


def check(name, cond, detail=""):
    print(("[ok] " if cond else "[FAIL] ") + name + (("  " + detail) if detail else ""))
    results.append(bool(cond))


p = ConPTY([EXE, "--debug", "--password", PASS, "-p", str(PORT), f"{USER}@{HOST}"],
           cols=100, rows=30)
try:
    check("connected to prompt", p.wait_text("[root@", 25))
    time.sleep(2.0)  # first prompt + OSC7

    # 1. plain typing
    m = len(p.text())
    for ch in CMD.format(src=src):
        p.send(ch)
        time.sleep(0.08)
    time.sleep(0.3)
    p.send("\r")
    ok = p.wait_text("✓", 15)
    time.sleep(2.5)
    check("TYPE put magic + upload", ok and "✓" in p.text()[m:], p.text()[-150:])

    # 1.5 slow typing (key gap 0.8s >> old 500ms idle flush): must still intercept
    m = len(p.text())
    for ch in CMD.format(src=src):
        p.send(ch)
        time.sleep(0.8)
    p.send("\r")
    ok = False
    end = time.time() + 15
    while time.time() < end:
        seg = p.text()[m:]
        if "✓" in seg:
            ok = True
            break
        if "未找到命令" in seg:
            break
        time.sleep(0.2)
    time.sleep(2.5)
    seg = p.text()[m:]
    check("SLOW-TYPE put magic (0.8s gap)", ok and "未找到命令" not in seg, seg[-150:])

    # 2. type garbage -> backspace all -> retype
    m = len(p.text())
    for ch in "putX":
        p.send(ch)
        time.sleep(0.05)
    time.sleep(0.3)
    for _ in range(4):
        p.send(b"\x7f")
        time.sleep(0.05)
    time.sleep(0.3)
    for ch in CMD.format(src=src):
        p.send(ch)
        time.sleep(0.08)
    time.sleep(0.3)
    p.send("\r")
    ok = False
    end = time.time() + 15
    while time.time() < end:
        seg = p.text()[m:]
        if "✓" in seg:
            ok = True
            break
        if "未找到命令" in seg:
            break
        time.sleep(0.2)
    time.sleep(2.5)
    seg = p.text()[m:]
    check("ERASE-then-type put magic", ok and "未找到命令" not in seg and "✓" in seg, seg[-150:])

    # 3. paste whole line
    m = len(p.text())
    p.send(CMD.format(src=src) + "\r")
    ok = p.wait_text("✓", 15)
    time.sleep(2.5)
    check("PASTE put magic", ok, p.text()[m:][-150:])

    # 3.5 "put" + Enter WITHOUT space: must be handled locally (usage hint),
    # never leaked to bash.
    m = len(p.text())
    p.send("put")
    time.sleep(0.3)
    p.send("\r")
    time.sleep(2.0)
    seg = p.text()[m:]
    check("PUT+enter (no space) local usage",
          "用法" in seg and "未找到命令" not in seg,
          seg[-150:])

    # 4. Up-arrow then type -> must passthrough (design: line content unknown)
    m = len(p.text())
    p.send(b"\x1b[A")
    time.sleep(0.5)
    for ch in CMD.format(src=src):
        p.send(ch)
        time.sleep(0.08)
    p.send("\r")
    time.sleep(2.0)
    seg = p.text()[m:]
    check("ARROW passthrough (by design)", "未找到命令" in seg or "bash" in seg, seg[-150:])

    # 5. lcd <TEMP>: local dir switch, visible message, no bash "lcd" execution
    m = len(p.text())
    p.send("lcd " + tmpdir)
    time.sleep(0.3)
    p.send("\r")
    time.sleep(1.5)
    seg = p.text()[m:]
    check("LCD magic + local switch",
          "本地目录" in seg and "未找到命令" not in seg,
          seg[-150:])

    # 5.5 lcd with no args shows current dir
    m = len(p.text())
    p.send("lcd")
    time.sleep(0.3)
    p.send("\r")
    time.sleep(1.5)
    seg = p.text()[m:]
    check("LCD (no args) shows dir", "本地目录" in seg, seg[-150:])

    # 6. lll: local long listing, no bash round-trip
    m = len(p.text())
    p.send("lll")
    time.sleep(0.3)
    p.send("\r")
    time.sleep(1.5)
    seg = p.text()[m:]
    check("LLL magic + local listing",
          "总计" in seg and "未找到命令" not in seg,
          seg[-150:])

    # 7. get typed directly at prompt: magic-intercepted, downloads into the
    # directory chosen by lcd above.
    m = len(p.text())
    p.send("get /root/e2e_src.txt")
    time.sleep(0.3)
    p.send("\r")
    ok = p.wait_text("✓", 15) or p.wait_text("下载完成", 5)
    time.sleep(2.0)
    seg = p.text()[m:]
    check("GET typed at prompt", ok and "✓" in seg, seg[-150:])

    # 8. GET + Tab: remote path completion via SFTP (e2e_s -> e2e_src.txt),
    # then Enter executes the completed line.
    if os.path.exists(local_get):
        os.remove(local_get)
    m = len(p.text())
    p.send("get e2e_s")
    time.sleep(0.5)
    p.send("\t")
    ok = p.wait_text("e2e_src.txt", 8)
    time.sleep(0.5)
    p.send("\r")
    ok2 = p.wait_text("✓", 15)
    time.sleep(2.0)
    seg = p.text()[m:]
    check("GET Tab-completion (SFTP remote list)",
          ok and ok2 and "未找到命令" not in seg, seg[-150:])
    check("tab-completed get downloaded file", os.path.isfile(local_get))

    # 9. LCD + Tab: local path completion (unique dir -> full name + /)
    comp_dir = os.path.join(tmpdir, "sshu_comp_e2e")
    os.makedirs(comp_dir, exist_ok=True)
    m = len(p.text())
    p.send("lcd " + os.path.join(tmpdir, "sshu_c"))
    time.sleep(0.5)
    p.send("\t")
    ok = p.wait_text("sshu_comp_e2e", 8)
    time.sleep(0.5)
    p.send(b"\x15")  # Ctrl-U clear line
    time.sleep(0.3)
    p.send("\r")     # empty line -> back to shell
    time.sleep(1.0)
    seg = p.text()[m:]
    check("LCD Tab-completion (local dir)", ok, seg[-150:])
finally:
    p.close()

# verify put landed on server and get landed locally
try:
    c = paramiko.SSHClient()
    c.set_missing_host_key_policy(paramiko.AutoAddPolicy())
    c.connect(HOST, username=USER, password=PASS, timeout=15)
    _, out, _ = c.exec_command("ls -la /root/e2e_src.txt 2>/dev/null && cat /root/e2e_src.txt")
    data = out.read().decode("utf-8", "replace")
    check("file uploaded to remote cwd", "real server e2e" in data, data.strip()[-120:])
    c.exec_command("rm -f /root/e2e_src.txt")
    c.close()
except Exception as e:
    check("file uploaded to remote cwd", False, repr(e))

check("get produced local file", os.path.isfile(local_get)
      and open(local_get, "rb").read() == b"real server e2e\n")

print("RESULT:", "PASS" if all(results) else "FAIL")

# dump debug log tail for inspection
log_path = os.path.join(os.environ.get("TEMP", ""), "sshu-debug.log")
if os.path.isfile(log_path):
    with open(log_path, "r", encoding="utf-8", errors="replace") as fh:
        lines = fh.readlines()
    print(f"--- debug log tail ({log_path}, {len(lines)} lines) ---")
    print("".join(lines[-25:]))
sys.exit(0 if all(results) else 1)
