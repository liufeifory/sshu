use std::process::ExitCode;

use sshu::app::{run, AppError};
use sshu::cli::{parse, split_target};
use sshu::sftp::Sftp;
use sshu::ssh::{connect, ConnectOptions};
use sshu::term::Console;

#[tokio::main]
async fn main() -> ExitCode {
    let args = parse();
    if args.debug {
        sshu::debuglog::init();
        sshu::debuglog::log("boot", &format!("sshu v{}", env!("CARGO_PKG_VERSION")));
    }
    let (user_from_target, host, port) = split_target(&args.target, args.port);
    let user = args
        .user
        .clone()
        .or(user_from_target)
        .unwrap_or_else(|| {
            std::env::var("USERNAME")
                .or_else(|_| std::env::var("USER"))
                .unwrap_or_else(|_| "root".to_string())
        });

    let password = match args.password.clone() {
        Some(p) => p,
        None => match sshu::term::prompt_password(&format!("{user}@{host} 的密码: ")) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("读取密码失败: {e}");
                return ExitCode::from(6);
            }
        },
    };

    let opts = ConnectOptions {
        host: host.clone(),
        port,
        user,
        password,
        timeout_secs: args.timeout,
        strict_host_key: args.strict_host_key,
    };

    let handle = match connect(opts).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(if e.0.contains("认证失败") { 3 } else { 4 });
        }
    };

    let sftp = match Sftp::open(&handle).await {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("[警告] {e}；shell 可用，put/get 不可用");
            None
        }
    };

    let console = match Console::open() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(6);
        }
    };

    match run(handle, sftp, console, &args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(AppError::OpenShell(e)) => {
            eprintln!("{e}");
            ExitCode::from(5)
        }
        Err(AppError::Runtime(e)) => {
            eprintln!("{e}");
            ExitCode::from(6)
        }
    }
}
