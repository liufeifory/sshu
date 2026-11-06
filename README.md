# sshu

> 一个带文件传输的 SSH 客户端：正常输入照常透传给远端 shell，另外多四条会话内指令 —— `put` / `get` / `lcd` / `lll`。

平时用 `ssh` 连服务器，传文件要另开一个终端敲 `scp`，还得重新拼一遍远端路径。
`sshu` 把两件事合成一件：连上去之后，在同一个提示符下就能传文件，远端路径走 SFTP、
本地路径走本机文件系统，两个方向都支持 **Tab 补全**。

单文件可执行程序。

```
[root@gauss1 ~]# put C:\Users\me\app.tar.gz
  ✓ /root/app.tar.gz  (12.4 MB, 1 个文件, 3.21s)

[root@gauss1 ~]# get /var/log/nginx/error.log
  ✓ error.log  (88.1 KB, 1 个文件, 0.12s)

[root@gauss1 ~]# lcd D:\build            # 切本地工作目录（等价于 ftp 的 lcd）
本地目录: D:\build

[root@gauss1 ~]# lll                    # 看本地目录，长格式（等价于 ls -l）
drwxr-xr-x        - 2026-11-05 12:40 dist/
-rw-r--r--   4.2 MB 2026-11-05 12:39 sshu.exe
```

## 特性

| 指令 | 作用 |
|---|---|
| `put <本地路径> [远端路径]` | 上传；省略远端路径 = 远端 shell 的实时 cwd |
| `get <远端路径> [本地路径]` | 下载；省略本地路径 = 本地当前目录 |
| `lcd [本地目录]` | 切换 / 查看本地工作目录（影响 `put` 源和 `get` 目标） |
| `lll` | 本地目录长格式列表（权限、大小、时间，目录优先） |

- **Tab 补全**：`put`/`lcd` 补本地路径，`get` 补远端路径（SFTP 实时列目录）；唯一匹配直接补全，
  有歧义列候选，目录补全后自动追加 `/`。普通模式下 Tab 仍交给远端 bash。
- **路径按 scp 习惯解析**：第二参数已存在为目录或以 `/`、`\` 结尾 → 放进该目录（必要时自动创建）；
  否则作为目标文件名，可用来改名。
- **远端 cwd 自动跟踪**：通过 OSC7 钩子实时拿到远端 shell 的当前目录，`put` 不用每次写全路径。
- **传输进度**：底部状态栏显示百分比、吞吐、剩余时间，完成后自动清除。
- **Ctrl-] 命令模式**：前缀键进入 `(sshu)` 本地命令模式，空行回车回到远端 shell。
- **终端网格渲染**：内置基于 [alacritty_terminal](https://github.com/alacritty/alacritty) 的终端模拟器，
  远端输出先进入 VT100/xterm 网格再按行重绘，top / vim / less 等全屏应用进出后画面不再错乱。
- **回滚历史**：鼠标滚轮 / `PgUp` / `PgDn`（含 `Shift`/`Ctrl` 组合）在客户端内翻看历史
  （默认 20000 行，`--history` 可调，每行约占 3 KB 内存）；任意键入自动回到最新输出。
  查看历史时底部状态栏会提示当前回滚行数。

`upload` / `download` 保留为 `put` / `get` 的兼容别名。

## 运行要求

- **客户端**：Windows 10 1703+（Windows Terminal / cmd / PowerShell）、Linux 或 macOS（任意 TTY 终端）。
- **服务端**：任意开启 SSH 与 SFTP 子系统的主机（OpenSSH 均可）。
- cwd 自动跟踪仅对远端 **bash / zsh** 生效；其他 shell 请显式指定远端目录。

## 安装

从 [Releases](../../releases) 下载对应平台的单文件可执行程序：

| 平台 | 文件 |
|---|---|
| Windows x64 | `sshu.exe` |
| Linux x86_64 | `sshu-linux-x86_64` |

也可以从源码构建（见下文）。Linux / macOS 上加执行权限即可运行：

```sh
chmod +x sshu-linux-x86_64
./sshu-linux-x86_64 --password 密码 root@192.168.32.129
```

## 用法

```sh
sshu --password 密码 root@192.168.32.129
sshu -l root -p 22 some-host
sshu --prefix c-a root@host          # 前缀键换成 Ctrl-A（默认 Ctrl-]）
sshu --no-magic root@host            # 只保留前缀键，不拦截直接输入的 put/get
sshu --no-hook root@host             # 不安装 cwd 上报钩子
sshu --strict-host-key root@host     # 未知主机键也拒绝
sshu --debug root@host               # 记录击键级调试日志
```

认证只支持账号 + 密码：`--password`，或省略后在连接前交互输入。

会话内两种用法任选：

- 直接在提示符处输入 `put ...` / `get ...` / `lcd [目录]` / `lll`；
- 或按 `Ctrl-]` 进入 `(sshu)` 本地命令模式，空行回车回到远端 shell。

复制粘贴与普通终端一致：

- **Windows**：鼠标拖选后回车 / 右键复制（传统控制台 QuickEdit 已保留），
  Windows Terminal 里用 `Ctrl+Shift+C` / `Ctrl+V`（或右键）；
- **Linux / macOS**：终端自身的 `Ctrl+Shift+C` / `Ctrl+Shift+V`。
- 粘贴整段 `put ...` 会被正常拦截；粘贴路径等普通文本原样转发远端。

## 排障

遇到"某条命令没被拦截 / 被吞掉 / 屏幕花掉"这类怪现象，用 `--debug` 跑一次复现步骤：

```sh
sshu --debug root@host
```

日志在系统临时目录（`%TEMP%\sshu-debug.log`、`/tmp/sshu-debug.log`），按击键级别记录：
控制台输入字节、发给远端的字节、远端回显、magic 词累积/命中/冲刷、行状态（`line_len`）转移、
命令解析结果、传输状态。提 issue 时附上这段日志，基本能还原每一步决策。

## 已知限制

- 为了收滚轮事件，客户端会向外层终端开启鼠标上报（`?1000h` + `?1006h`）：
  选中文字需按住 **Shift 再拖选**（Windows Terminal / VS Code 终端通用做法）；
  不想要这个行为可以加 `--no-mouse`，此时滚轮交回外层终端，历史只能用 `PgUp` / `PgDn` 翻。
- 鼠标上报会按远端需求切换级别（vim / htop 开了鼠标时，事件原样转给远端，滚轮不再翻本地历史）。
- 远端设置的窗口标题（OSC 0/2）不会转发给本地终端。

- 只有密码登录；不支持私钥、Pageant、ssh-agent。
- `put` / `get` 依赖远端 SFTP 子系统；SFTP 被禁用时 shell 仍可用，但传输会明确报错。
- cwd 自动跟踪只对 bash / zsh 生效。
- 把文件 `put` 到与本地源完全相同的路径（同一台机器、同一绝对路径）会先把目标截断再读，
  结果是 0 字节——跨机传文件不会遇到，单机自测时注意错开目标路径。
- Windows 控制台必须支持 VT；过老的 Windows 10 会拒绝运行而不是降级。
- 未签名 exe；部分杀毒软件可能拦截，需要按现场策略加白名单。
- 中文输入法依赖控制台提交的字符；极少数 IME 组合下可能表现异常。
- 行内编辑过的提示符（`↑` 翻历史、`Tab` 补全之后）不拦截直接输入的会话指令，见上文说明。

## 许可证

MIT，见 [LICENSE](LICENSE)。
