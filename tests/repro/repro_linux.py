#!/usr/bin/env python3
"""Linux -> Linux 端到端回归：在 Linux 上用 pty 驱动 sshu 连接本机 sshd。

用法（在 Linux 上执行）：
    python3 tests/repro/repro_linux.py [sshu 可执行文件路径]

环境变量：
    SSHU_TEST_HOST / SSHU_TEST_PORT / SSHU_TEST_USER / SSHU_TEST_PASS

只用标准库，兼容 python3.6+。
"""
import os
import pty
import select
import sys
import time
import fcntl
import struct
import termios

EXE = sys.argv[1] if len(sys.argv) > 1 else "./target/release/sshu"
HOST = os.environ.get("SSHU_TEST_HOST", "127.0.0.1")
PORT = os.environ.get("SSHU_TEST_PORT", "22")
USER = os.environ.get("SSHU_TEST_USER", "root")
PASS = os.environ.get("SSHU_TEST_PASS", "111111")

results = []


class Pty(object):
    def __init__(self, argv, cols=120, rows=40):
        pid, fd = pty.fork()
        if pid == 0:
            os.execv(argv[0], argv)
        self.pid = pid
        self.fd = fd
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.buf = b""

    def pump(self, timeout=0.25):
        end = time.time() + timeout
        while time.time() < end:
            r, _, _ = select.select([self.fd], [], [], 0.05)
            if not r:
                continue
            try:
                d = os.read(self.fd, 8192)
            except OSError:
                return
            if not d:
                return
            self.buf += d

    def send(self, s):
        if isinstance(s, str):
            s = s.encode("utf-8")
        os.write(self.fd, s)

    def type_slow(self, s, delay=0.15):
        for ch in s:
            self.send(ch)
            time.sleep(delay)
            self.pump(0.02)

    def text(self):
        return self.buf.decode("utf-8", "replace")

    def wait_text(self, needle, timeout=20):
        end = time.time() + timeout
        while time.time() < end:
            self.pump(0.2)
            if needle in self.text():
                return True
        return False

    def close(self):
        try:
            os.close(self.fd)
        except OSError:
            pass
        try:
            os.waitpid(self.pid, os.WNOHANG)
        except OSError:
            pass


def check(name, ok, detail=""):
    results.append(ok)
    mark = "PASS" if ok else "FAIL"
    print("[%s] %s%s" % (mark, name, ("  | " + detail.replace("\n", " ")[-160:]) if detail else ""))


def sh(cmd):
    os.system(cmd)


def main():
    src = "/root/lx_src.txt"
    got = "/root/lx_got.txt"
    payload = b"linux to linux e2e\n"
    with open(src, "wb") as fh:
        fh.write(payload)
    size = len(payload)
    for p in (got, "/tmp/lx_got.txt"):
        if os.path.exists(p):
            os.remove(p)

    p = Pty([EXE, "--password", PASS, "-p", PORT, "%s@%s" % (USER, HOST)])
    check("linux connect", p.wait_text("#", 30) or p.wait_text("$", 5), p.text()[-120:])
    time.sleep(0.8)

    # 1. 干净提示符下逐键手打 put
    m = len(p.text())
    p.type_slow("put " + src + " /tmp/lx_dst.txt", 0.06)
    time.sleep(0.3)
    p.send("\r")
    ok = p.wait_text("\u2713", 20) or p.wait_text("\u4e0a\u4f20\u5b8c\u6210", 5)
    p.pump(1.0)
    seg = p.text()[m:]
    check("PUT keeps space echo", "put /root/lx" in seg, seg[-160:])
    check("PUT typed key-by-key", ok and "not found" not in seg and "\u672a\u627e\u5230\u547d\u4ee4" not in seg, seg[-150:])
    check("PUT remote file", os.path.exists("/tmp/lx_dst.txt") and os.path.getsize("/tmp/lx_dst.txt") == size, "")

    # 2. 打错 -> 退格删光 -> 重打（原 bug 场景）
    m = len(p.text())
    p.send("xxx")
    time.sleep(0.2)
    p.send("\x7f\x7f\x7f")
    time.sleep(0.2)
    p.type_slow("put " + src + " /tmp/lx_dst.txt", 0.05)
    time.sleep(0.2)
    p.send("\r")
    ok = p.wait_text("\u2713", 20)
    time.sleep(0.8)
    seg = p.text()[m:]
    check("ERASE-then-type", ok and "\u672a\u627e\u5230\u547d\u4ee4" not in seg, seg[-150:])

    # 3. 慢速打字（键间隔 0.8s）
    m = len(p.text())
    p.type_slow("put " + src + " /tmp/lx_dst2.txt", 0.8)
    time.sleep(0.3)
    p.send("\r")
    ok = p.wait_text("\u2713", 25)
    time.sleep(0.8)
    check("SLOW-TYPE put", ok, p.text()[m:][-150:])

    # 4. 整段粘贴
    m = len(p.text())
    p.send("put " + src + " /tmp/lx_dst3.txt\r")
    ok = p.wait_text("\u2713", 20)
    time.sleep(0.8)
    check("PASTE put", ok, p.text()[m:][-150:])

    # 5. lcd 切本地目录 + lll 列表
    m = len(p.text())
    p.send("lcd /tmp\r")
    p.pump(1.2)
    seg = p.text()[m:]
    check("LCD switch", "\u672c\u5730\u76ee\u5f55" in seg and "/tmp" in seg, seg[-150:])

    m = len(p.text())
    p.send("lll\r")
    p.pump(1.2)
    seg = p.text()[m:]
    check("LLL listing", "lx_" in seg or "drwx" in seg, seg[-150:])

    # 6. get：落到 lcd 后的 /tmp
    m = len(p.text())
    p.send("get " + src + "\r")
    ok = p.wait_text("\u2713", 20)
    p.pump(1.0)
    check("GET at prompt", ok and os.path.exists("/tmp/lx_src.txt"), p.text()[m:][-150:])

    # 7. Tab 补全（get 远端路径）
    m = len(p.text())
    p.type_slow("get /root/lx_s", 0.05)
    p.send("\t")
    p.pump(1.2)
    seg = p.text()[m:]
    check("TAB complete remote", "lx_src.txt" in seg, seg[-150:])
    p.send("\x15")  # Ctrl-U 清行，不执行
    time.sleep(0.5)
    p.send("\r")
    time.sleep(0.8)

    # 8. put 不带参数 + 回车 -> 本地提示用法，不漏给 bash
    m = len(p.text())
    p.send("put\r")
    p.pump(1.2)
    seg = p.text()[m:]
    check("PUT without args -> usage", "\u7528\u6cd5" in seg and "\u672a\u627e\u5230\u547d\u4ee4" not in seg, seg[-150:])

    # 9. 前缀键进入命令模式
    m = len(p.text())
    p.send("\x1d")
    time.sleep(0.5)
    p.send("lll\r")
    p.pump(1.2)
    seg = p.text()[m:]
    check("PREFIX key cmd mode", "(sshu)" in seg, seg[-150:])
    p.send("\r")
    time.sleep(0.8)

    p.close()

    print("RESULT:", "PASS" if all(results) else "FAIL")
    sys.exit(0 if all(results) else 1)


if __name__ == "__main__":
    main()
