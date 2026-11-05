# Windows 构建脚本：编译 release、检查导入表、产物放到 dist\sshu.exe
# 需要：Rust stable + x86_64-pc-windows-gnu target + MinGW gcc/nasm（PATH 里能找到 gcc、objdump）
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File build.ps1
#
# 说明：如果本机把工具链装在非默认位置，可用环境变量覆盖：
#   $env:RUSTUP_HOME / $env:CARGO_HOME / $env:MSYS2_MINGW_BIN

$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

# 可选：本机便携工具链位置（存在才用，方便无管理员环境）
if (-not $env:RUSTUP_HOME -and (Test-Path 'D:\rust\rustup')) { $env:RUSTUP_HOME = 'D:\rust\rustup' }
if (-not $env:CARGO_HOME  -and (Test-Path 'D:\rust\cargo'))  { $env:CARGO_HOME  = 'D:\rust\cargo' }
if (-not $env:MSYS2_MINGW_BIN -and (Test-Path 'D:\msys64\mingw64\bin')) { $env:MSYS2_MINGW_BIN = 'D:\msys64\mingw64\bin' }

if ($env:CARGO_HOME)      { $env:PATH = (Join-Path $env:CARGO_HOME 'bin') + ';' + $env:PATH }
if ($env:MSYS2_MINGW_BIN) { $env:PATH = $env:MSYS2_MINGW_BIN + ';' + $env:PATH }

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error '找不到 cargo，请先安装 Rust（https://rustup.rs）'
    exit 1
}
if (-not (Get-Command gcc -ErrorAction SilentlyContinue)) {
    Write-Error '找不到 gcc，请安装 MinGW-w64（MSYS2: pacman -S mingw-w64-x86_64-gcc）'
    exit 1
}

# rustup 会把进度写到 stderr，PowerShell 在 Stop 模式下会当成异常；
# 这里临时放宽错误策略，并且只在未安装该 target 时才装。
$prevEap = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
$installedTargets = (& rustup target list --installed 2>&1 | Out-String)
if ($installedTargets -notmatch 'x86_64-pc-windows-gnu') {
    & rustup target add x86_64-pc-windows-gnu 2>&1 | Out-Null
}
$ErrorActionPreference = $prevEap

cargo build --release --target x86_64-pc-windows-gnu
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

New-Item -ItemType Directory -Force dist | Out-Null
Copy-Item 'target\x86_64-pc-windows-gnu\release\sshu.exe' 'dist\sshu.exe' -Force

# 导入表检查：只允许系统 DLL，避免产物在别人机器上缺 DLL 起不来
$objdump = (Get-Command objdump -ErrorAction SilentlyContinue).Source
$bad = @()
if ($objdump) {
    $imports = & $objdump -p 'dist\sshu.exe' |
        Select-String 'DLL Name' |
        ForEach-Object { ($_ -split 'DLL Name:')[1].Trim() } |
        Sort-Object -Unique

    $allowed = '^(KERNEL32|USER32|ADVAPI32|WS2_32|bcrypt|bcryptprimitives|USERENV|oleaut32|ntdll|msvcrt)\.dll$|^api-ms-win-'
    $bad = $imports | Where-Object { $_ -notmatch $allowed }
    Write-Host 'OK: dist\sshu.exe'
    $imports | ForEach-Object { Write-Host ('  ' + $_) }
} else {
    Write-Host 'OK: dist\sshu.exe（未找到 objdump，跳过导入表检查）'
}

if ($bad) {
    Write-Error ('非系统 DLL 依赖: ' + ($bad -join ', '))
    exit 1
}
