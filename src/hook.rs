use std::path::Path;

pub const HOOK_SH: &str = r#"# sshu: 让 shell 提示符上报当前目录(OSC 7)，供客户端实时跟踪 cwd
__sshu_osc7() {
  printf '\033]7;file://%s%s\007' "${HOSTNAME:-localhost}" "$PWD"
}
if [ -n "${ZSH_VERSION:-}" ]; then
  autoload -Uz add-zsh-hook 2>/dev/null && add-zsh-hook precmd __sshu_osc7
elif [ -n "${BASH_VERSION:-}" ]; then
  case ";${PROMPT_COMMAND:-};" in
    *__sshu_osc7*) ;;
    *) PROMPT_COMMAND="__sshu_osc7${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
  esac
fi
"#;

pub const BASHRC: &str = r#"[ -f "$HOME/.bash_profile" ] && . "$HOME/.bash_profile"
[ -f "$HOME/.bashrc" ] && . "$HOME/.bashrc"
. "$HOME/.sshu/hook.sh"
"#;

pub const ZSHRC: &str = r#"[ -f "$HOME/.zshrc" ] && . "$HOME/.zshrc"
. "$HOME/.sshu/hook.sh"
"#;

pub fn shell_name(shell: Option<&str>) -> String {
    shell
        .and_then(|s| Path::new(s).file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// 返回启动带 OSC7 钩子的 shell 的命令；不支持的 shell 返回 None。
pub fn hook_command(shell: Option<&str>) -> Option<String> {
    match shell_name(shell).as_str() {
        "bash" => Some(r#"exec "${SHELL:-bash}" --rcfile "$HOME/.sshu/bashrc" -i"#.to_string()),
        "zsh" => Some(r#"export ZDOTDIR="$HOME/.sshu/zdotdir"; exec "${SHELL:-zsh}" -l"#.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_by_shell() {
        assert!(hook_command(Some("/bin/bash")).unwrap().contains("--rcfile"));
        assert!(hook_command(Some("/usr/bin/zsh")).unwrap().contains("ZDOTDIR"));
        assert!(hook_command(Some("/usr/bin/fish")).is_none());
        assert!(hook_command(None).is_none());
    }

    #[test]
    fn hook_is_local_only() {
        // 教训：系统级（/etc/profile.d）钩子 + 导出会让 OSC7 混进 exec 通道，
        // 污染 SFTP 等二进制子系统（openGauss 机器实测）。钩子必须只装在
        // 登录用户的 ~/.sshu 里，不导出、不进系统目录。
        assert!(!HOOK_SH.contains("export"));
        assert!(!hook_command(Some("/bin/bash")).unwrap().contains("profile.d"));
    }
}
