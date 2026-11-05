# -*- coding: utf-8 -*-
"""Fake SSH server mimicking bash (echo + OSC7 prompt) for input-path repro."""
import socket
import threading

import paramiko

HOST_KEY = paramiko.RSAKey.generate(2048)

PROMPT = "\x1b]7;file://gauss1/template\x07[root@gauss1 template]# "


class ShellSession(threading.Thread):
    """Emulates a bash readline: echo keys, OSC7 + prompt per line."""

    def __init__(self, chan, log):
        super().__init__(daemon=True)
        self.chan = chan
        self.log = log
        self.line = ""

    def emit(self, text):
        self.chan.sendall(text.encode("utf-8"))

    def prompt(self):
        self.emit(PROMPT)

    def run(self):
        try:
            self.prompt()
            while True:
                data = self.chan.recv(4096)
                if not data:
                    break
                self.log.append(data)
                for b in data:
                    ch = chr(b)
                    if ch in ("\r", "\n"):
                        text = self.line
                        self.line = ""
                        self.emit("\r\n")
                        if text.startswith(("upload", "download")):
                            cmd = text.split(" ")[0]
                            self.emit("bash: %s: 未找到命令\r\n" % cmd)
                        elif text:
                            self.emit("ran: %s\r\n" % text)
                        self.prompt()
                    elif ch == "\x7f":
                        if self.line:
                            self.line = self.line[:-1]
                            self.emit("\b \b")
                    elif ch == "\x03":
                        self.line = ""
                        self.emit("^C\r\n")
                        self.prompt()
                    elif b >= 0x20:
                        self.line += ch
                        self.emit(ch)
        except Exception:
            pass


class SshuServer(paramiko.ServerInterface):
    def __init__(self, log):
        self.log = log

    def check_auth_password(self, username, password):
        return paramiko.AUTH_SUCCESSFUL

    def get_allowed_auths(self, username):
        return "password"

    def check_channel_request(self, kind, chanid):
        return paramiko.OPEN_SUCCEEDED

    def check_channel_pty_request(self, channel, term, cols, rows, pixelw, pixelh, modes):
        return True

    def check_channel_shell_request(self, channel):
        return True

    def check_channel_exec_request(self, channel, command):
        if isinstance(command, bytes):
            command = command.decode("utf-8", "replace")
        # probe_shell asks for $SHELL; hook install execs a bash wrapper.
        if "SHELL" in command:
            channel.sendall(b"/bin/bash")
            channel.send_exit_status(0)
            channel.close()
        else:
            ShellSession(channel, self.log).start()
        return True

    def check_channel_subsystem_request(self, channel, name):
        return False  # no SFTP: sshu should warn and continue


def serve(port, log):
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    sock.bind(("127.0.0.1", port))
    sock.listen(4)
    while True:
        conn, _ = sock.accept()
        t = paramiko.Transport(conn)
        t.add_server_key(HOST_KEY)
        t.start_server(server=SshuServer(log))
        threading.Thread(target=_pump, args=(t, log), daemon=True).start()


def _pump(transport, log):
    try:
        chan = transport.accept(20)
        if chan is None:
            return
        ShellSession(chan, log).start()
    except Exception:
        pass


if __name__ == "__main__":
    import sys
    log = []
    serve(int(sys.argv[1]) if len(sys.argv) > 1 else 2222, log)
