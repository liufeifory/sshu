# -*- coding: utf-8 -*-
"""Drive the real sshu TUI inside a Windows ConPTY pseudo-console.

Validates the WinTerm path (ctypes console modes, msvcrt input) end to end:
connect -> normal typing -> magic upload -> prefix mode download -> exit.
"""
import ctypes
import os
import subprocess
import sys
import tempfile
import threading
import time

from ctypes import wintypes

try:
    sys.stdout.reconfigure(errors="replace")
except Exception:
    pass

k32 = ctypes.WinDLL("kernel32", use_last_error=True)
HPCON = wintypes.HANDLE
PTA_PC = 0x00020016  # ProcThreadAttributeValue(22, FALSE, TRUE, FALSE)
EXTENDED_STARTUPINFO_PRESENT = 0x00080000


class COORD(ctypes.Structure):
    _fields_ = [("X", ctypes.c_short), ("Y", ctypes.c_short)]


class SECURITY_ATTRIBUTES(ctypes.Structure):
    _fields_ = [("nLength", wintypes.DWORD),
                ("lpSecurityDescriptor", wintypes.LPVOID),
                ("bInheritHandle", wintypes.BOOL)]


class STARTUPINFOW(ctypes.Structure):
    _fields_ = [
        ("cb", wintypes.DWORD), ("lpReserved", wintypes.LPWSTR),
        ("lpDesktop", wintypes.LPWSTR), ("lpTitle", wintypes.LPWSTR),
        ("dwX", wintypes.DWORD), ("dwY", wintypes.DWORD),
        ("dwXSize", wintypes.DWORD), ("dwYSize", wintypes.DWORD),
        ("dwXCountChars", wintypes.DWORD), ("dwYCountChars", wintypes.DWORD),
        ("dwFillAttribute", wintypes.DWORD), ("dwFlags", wintypes.DWORD),
        ("wShowWindow", wintypes.WORD), ("cbReserved2", wintypes.WORD),
        ("lpReserved2", ctypes.POINTER(ctypes.c_byte)),
        ("hStdInput", wintypes.HANDLE), ("hStdOutput", wintypes.HANDLE),
        ("hStdError", wintypes.HANDLE),
    ]


class STARTUPINFOEXW(ctypes.Structure):
    _fields_ = [("StartupInfo", STARTUPINFOW), ("lpAttributeList", wintypes.LPVOID)]


class PROCESS_INFORMATION(ctypes.Structure):
    _fields_ = [("hProcess", wintypes.HANDLE), ("hThread", wintypes.HANDLE),
                ("dwProcessId", wintypes.DWORD), ("dwThreadId", wintypes.DWORD)]


k32.CreatePipe.argtypes = [ctypes.POINTER(wintypes.HANDLE), ctypes.POINTER(wintypes.HANDLE),
                           ctypes.POINTER(SECURITY_ATTRIBUTES), wintypes.DWORD]
k32.CreatePipe.restype = wintypes.BOOL
k32.CreatePseudoConsole.argtypes = [COORD, wintypes.HANDLE, wintypes.HANDLE,
                                    wintypes.DWORD, ctypes.POINTER(HPCON)]
k32.CreatePseudoConsole.restype = ctypes.c_long
k32.InitializeProcThreadAttributeList.argtypes = [wintypes.LPVOID, wintypes.DWORD,
                                                  wintypes.DWORD, ctypes.POINTER(ctypes.c_size_t)]
k32.UpdateProcThreadAttribute.argtypes = [wintypes.LPVOID, wintypes.DWORD, ctypes.c_size_t,
                                          wintypes.LPVOID, ctypes.c_size_t,
                                          wintypes.LPVOID, wintypes.LPVOID]
k32.CreateProcessW.argtypes = [wintypes.LPCWSTR, wintypes.LPWSTR,
                               wintypes.LPVOID, wintypes.LPVOID, wintypes.BOOL,
                               wintypes.DWORD, wintypes.LPVOID, wintypes.LPCWSTR,
                               wintypes.LPVOID, ctypes.POINTER(PROCESS_INFORMATION)]
k32.CreateProcessW.restype = wintypes.BOOL
k32.ReadFile.argtypes = [wintypes.HANDLE, wintypes.LPVOID, wintypes.DWORD,
                         ctypes.POINTER(wintypes.DWORD), wintypes.LPVOID]
k32.WriteFile.argtypes = [wintypes.HANDLE, wintypes.LPVOID, wintypes.DWORD,
                          ctypes.POINTER(wintypes.DWORD), wintypes.LPVOID]
k32.CloseHandle.argtypes = [wintypes.HANDLE]
k32.ClosePseudoConsole.argtypes = [HPCON]
k32.WaitForSingleObject.argtypes = [wintypes.HANDLE, wintypes.DWORD]
k32.GetExitCodeProcess.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
k32.SetHandleInformation.argtypes = [wintypes.HANDLE, wintypes.DWORD, wintypes.DWORD]
k32.SetHandleInformation.restype = wintypes.BOOL
k32.GetStdHandle.argtypes = [wintypes.DWORD]
k32.GetStdHandle.restype = wintypes.HANDLE


class ConPTY:
    def __init__(self, argv, cols=100, rows=30):
        self.out = bytearray()
        self.lock = threading.Lock()
        sa = SECURITY_ATTRIBUTES(ctypes.sizeof(SECURITY_ATTRIBUTES), None, True)
        self.h_in_r = wintypes.HANDLE()
        self.h_in_w = wintypes.HANDLE()
        self.h_out_r = wintypes.HANDLE()
        self.h_out_w = wintypes.HANDLE()
        for a, b in ((ctypes.byref(self.h_in_r), ctypes.byref(self.h_in_w)),
                     (ctypes.byref(self.h_out_r), ctypes.byref(self.h_out_w))):
            if not k32.CreatePipe(a, b, ctypes.byref(sa), 0):
                raise ctypes.WinError(ctypes.get_last_error())

        self.hpc = HPCON()
        err = k32.CreatePseudoConsole(COORD(cols, rows), self.h_in_r, self.h_out_w, 0,
                                      ctypes.byref(self.hpc))
        if err != 0:
            raise ctypes.WinError(err)
        k32.CloseHandle(self.h_in_r)
        k32.CloseHandle(self.h_out_w)

        size = ctypes.c_size_t(0)
        k32.InitializeProcThreadAttributeList(None, 1, 0, ctypes.byref(size))
        self.attr_buf = ctypes.create_string_buffer(size.value)
        if not k32.InitializeProcThreadAttributeList(self.attr_buf, 1, 0, ctypes.byref(size)):
            raise ctypes.WinError(ctypes.get_last_error())
        if not k32.UpdateProcThreadAttribute(
                self.attr_buf, 0, PTA_PC, self.hpc,
                ctypes.sizeof(HPCON), None, None):
            raise ctypes.WinError(ctypes.get_last_error())

        si = STARTUPINFOEXW()
        si.StartupInfo.cb = ctypes.sizeof(STARTUPINFOEXW)
        si.lpAttributeList = ctypes.cast(self.attr_buf, wintypes.LPVOID)
        pi = PROCESS_INFORMATION()
        cmdline = ctypes.create_unicode_buffer(subprocess.list2cmdline(argv))
        # Our own std handles are the test runner's pipes; if they are
        # inheritable the child gets those instead of the pseudoconsole's
        # console handles. Clear the inherit bit just for the spawn.
        saved = []
        for std in (-10, -11, -12):
            h = k32.GetStdHandle(std & 0xFFFFFFFF)
            if h:
                saved.append((h, bool(k32.SetHandleInformation(h, 1, 0))))
        try:
            ok = k32.CreateProcessW(argv[0], cmdline, None, None, False,
                                    EXTENDED_STARTUPINFO_PRESENT, None, None,
                                    ctypes.byref(si), ctypes.byref(pi))
            err = ctypes.get_last_error()
        finally:
            for h, cleared in saved:
                if cleared:
                    k32.SetHandleInformation(h, 1, 1)
        if not ok:
            raise ctypes.WinError(err)
        self.pi = pi
        k32.CloseHandle(pi.hThread)
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def _read(self):
        buf = ctypes.create_string_buffer(8192)
        n = wintypes.DWORD(0)
        while True:
            ok = k32.ReadFile(self.h_out_r, buf, 8192, ctypes.byref(n), None)
            if not ok or n.value == 0:
                return
            with self.lock:
                self.out += buf.raw[:n.value]

    def text(self):
        with self.lock:
            return self.out.decode("utf-8", "replace")

    def wait_text(self, needle, timeout=20.0):
        end = time.time() + timeout
        while time.time() < end:
            if needle in self.text():
                return True
            time.sleep(0.1)
        return False

    def send(self, data, delay=0.0):
        if isinstance(data, str):
            data = data.encode("utf-8")
        if delay:
            time.sleep(delay)
        n = wintypes.DWORD(0)
        buf = ctypes.create_string_buffer(data, len(data))
        if not k32.WriteFile(self.h_in_w, buf, len(data), ctypes.byref(n), None):
            raise ctypes.WinError(ctypes.get_last_error())

    def wait_exit(self, timeout=20.0):
        rc = k32.WaitForSingleObject(self.pi.hProcess, int(timeout * 1000))
        if rc != 0:
            return None
        code = wintypes.DWORD()
        k32.GetExitCodeProcess(self.pi.hProcess, ctypes.byref(code))
        return code.value

    def close(self):
        try:
            k32.ClosePseudoConsole(self.hpc)
        except Exception:
            pass
        for h in (self.h_in_w, self.h_out_r, self.pi.hProcess):
            try:
                k32.CloseHandle(h)
            except Exception:
                pass


def check(name, cond, detail=""):
    print(("[ok] " if cond else "[FAIL] ") + name + (("  " + detail) if detail else ""))
    return bool(cond)


def main():
    if "--probe" in sys.argv:
        exe = sys.argv[sys.argv.index("--probe") + 1]
        p = ConPTY([exe], cols=100, rows=30)
        ok1 = p.wait_text("SIZE 100x30", 10)
        print("[ok] size" if ok1 else "[FAIL] size", repr(p.text()[-200:]))
        p.send("abc" + chr(13))
        time.sleep(0.5)
        ok2 = "abc" in p.text()
        print("[ok] echo" if ok2 else "[FAIL] echo", repr(p.text()[-200:]))
        p.send("q")
        rc = p.wait_exit(5)
        p.close()
        ok3 = rc is not None
        print("[ok] exit" if ok3 else "[FAIL] exit", rc)
        print("RESULT:", "PASS" if (ok1 and ok2 and ok3) else "FAIL")
        return 0 if (ok1 and ok2 and ok3) else 1

    python = sys.executable
    launcher = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                            "_conpty_launch.py")
    tmp = tempfile.mkdtemp(prefix="sshu_pty_")
    src_dir = os.path.join(tmp, "local dir")
    os.makedirs(src_dir)
    src = os.path.join(src_dir, "hello.txt")
    with open(src, "wb") as fh:
        fh.write(b"from conpty\n")
    dl_dir = os.path.join(tmp, "downloads")
    os.makedirs(dl_dir)

    if "--exe" in sys.argv:
        exe = sys.argv[sys.argv.index("--exe") + 1]
        argv = [exe, "--password", "111111", "--prefix", "c-a",
                "root@192.168.32.129"]
    else:
        argv = [python, "-u", launcher, "--password", "111111", "--prefix", "c-a",
                "root@192.168.32.129"]
    p = ConPTY(argv, cols=100, rows=30)
    results = []
    try:
        results.append(check("connected banner", p.wait_text("已连接", 25),
                             p.text()[-200:]))
        results.append(check("mouse reporting disabled",
                             "[?1003l" in p.text()))
        time.sleep(1.0)

        # normal shell input passes through
        p.send("cd /tmp\r", 0.3)
        time.sleep(1.0)
        results.append(check("normal command echoed", "/tmp" in p.text()))

        # a terminal focus event must not break magic-word detection
        p.send(b"\x1b[I")
        time.sleep(0.2)

        # magic upload (local path contains a space)
        p.send('upload "%s" /tmp/sshu_pty/\r' % src, 0.3)
        ok_up = p.wait_text("上传中", 5) or p.wait_text("(sshu)", 3)
        time.sleep(3.0)
        import paramiko
        c = paramiko.SSHClient()
        c.set_missing_host_key_policy(paramiko.AutoAddPolicy())
        c.connect("192.168.32.129", username="root", password="111111", timeout=15)
        s = c.open_sftp()
        try:
            data = s.open("/tmp/sshu_pty/hello.txt", "rb").read()
        except FileNotFoundError:
            data = b""
        results.append(check("magic upload created file", data == b"from conpty\n", repr(data)))

        # prefix key -> command mode -> download
        mark = len(p.text())
        p.send(b"\x01", 0.5)          # ctrl-a prefix (ConPTY passes this through)
        end = time.time() + 3
        while time.time() < end and "(sshu)" not in p.text()[mark:]:
            time.sleep(0.1)
        results.append(check("prefix enters cmd mode", "(sshu)" in p.text()[mark:]))
        p.send('download /tmp/sshu_pty/hello.txt "%s"\r' % dl_dir, 0.3)
        got = os.path.join(dl_dir, "hello.txt")
        end = time.time() + 6
        while time.time() < end and not os.path.isfile(got):
            time.sleep(0.1)
        results.append(check("prefix download wrote local file",
                             os.path.isfile(got) and open(got, "rb").read() == b"from conpty\n",
                             got))
        mark = len(p.text())
        p.send('download /tmp/sshu_pty_missing "%s"\r' % dl_dir, 0.3)
        end = time.time() + 5
        while time.time() < end and not any(
                x in p.text()[mark:] for x in ("失败", "不存在", "SshuError")):
            time.sleep(0.1)
        results.append(check("download error reported locally",
                             any(x in p.text()[mark:]
                                 for x in ("失败", "不存在", "SshuError"))))
        p.send("\r", 0.3)          # leave command mode
        time.sleep(0.5)

        try:
            s.remove("/tmp/sshu_pty/hello.txt")
            s.rmdir("/tmp/sshu_pty")
        except FileNotFoundError:
            pass
        c.close()

        p.send("exit\r", 0.3)
        rc = p.wait_exit(20)
        time.sleep(0.4)
        results.append(check("process exited", rc is not None, str(rc)))
        results.append(check("no traceback", "Traceback" not in p.text(),
                             p.text()[-400:]))
    finally:
        p.close()

    print("RESULT:", "PASS" if all(results) else "FAIL")
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
