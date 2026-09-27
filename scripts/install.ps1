# 部署 seek 插件到 QQ 音乐当前的版本目录。
#   .\install.ps1 [-Restart] [-DllPath <payload.dll>]
# QQ 音乐自升级后真实目录是带版本号的一层(外层只剩旧文件残留),所以路径从注册表取。
[CmdletBinding()]
param(
    [string]$DllPath = '',
    [string]$QqDir = '',
    [switch]$Restart
)

$ErrorActionPreference = 'Stop'

function Resolve-QqVersionDir {
    foreach ($key in 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\QQMusic',
                     'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\QQMusic',
                     'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\QQMusic') {
        if (Test-Path $key) {
            $loc = (Get-ItemProperty $key).InstallLocation
            if ($loc -and (Test-Path (Join-Path $loc 'QQMusic.exe'))) { return $loc }
        }
    }

    # 注册表没指到版本子目录时,退而求其次找带版本号的那层。
    $root = 'C:\Program Files (x86)\Tencent\QQMusic'
    if (Test-Path $root) {
        $versioned = Get-ChildItem $root -Directory -Filter 'QQMusic*' -ErrorAction SilentlyContinue |
            Where-Object { Test-Path (Join-Path $_.FullName 'QQMusic.exe') } |
            Sort-Object LastWriteTime -Descending | Select-Object -First 1
        if ($versioned) { return $versioned.FullName }
        if (Test-Path (Join-Path $root 'QQMusic.exe')) { return $root }
    }
    return $null
}

if (-not $QqDir) {
    $QqDir = Resolve-QqVersionDir
    if (-not $QqDir) { Write-Error '找不到 QQ 音乐安装目录,请用 -QqDir 指定。'; exit 1 }
}
"目标目录: $QqDir"

if (-not (Test-Path (Join-Path $QqDir 'QQMusic.exe'))) {
    Write-Error "目录里没有 QQMusic.exe: $QqDir"; exit 1
}

if (-not $DllPath) {
    $candidate = Join-Path $PSScriptRoot '../target/i686-pc-windows-msvc/release/payload.dll'
    if (-not (Test-Path $candidate)) { Write-Error "找不到构建产物,请先 cargo build 或用 -DllPath 指定: $candidate"; exit 1 }
    $DllPath = (Resolve-Path $candidate).Path
}

# 32 位是硬要求:放进去一个 64 位 DLL 会让 QQ 音乐直接起不来。
$bytes = [IO.File]::ReadAllBytes($DllPath)
$peOffset = [BitConverter]::ToInt32($bytes, 0x3C)
$machine = [BitConverter]::ToUInt16($bytes, $peOffset + 4)
if ($machine -ne 0x014C) {
    Write-Error ('构建产物不是 32 位(PE machine = 0x{0:X4},应为 0x014C),中止。' -f $machine)
    exit 1
}
'构建产物: {0} ({1:N0} 字节, i386 32 位)' -f $DllPath, $bytes.Length

$target = Join-Path $QqDir 'msimg32.dll'
$backup = "$target.orig.bak"

# 只有确认不是我们自己的产物时才备份,避免把备份覆盖成插件。
if ((Test-Path $target) -and -not (Test-Path $backup)) {
    $existing = [IO.File]::ReadAllBytes($target)
    $existingPe = [BitConverter]::ToUInt16($existing, [BitConverter]::ToInt32($existing, 0x3C) + 4)
    $looksLikeSystem = $existing.Length -lt 20000 -and $existingPe -eq 0x014C
    if ($looksLikeSystem) {
        Copy-Item $target $backup
        '已备份原文件 -> ' + $backup
    }
}

$running = Get-Process -Name 'QQMusic', 'QQMusicExternal', 'QQMusicAgent', 'DesktopDynamicLyric' -ErrorAction SilentlyContinue
if ($running) {
    '正在结束 QQ 音乐进程: ' + ($running.ProcessName -join ', ')
    $running | Stop-Process -Force
    Start-Sleep -Seconds 2
}

try {
    Copy-Item $DllPath $target -Force
} catch [System.UnauthorizedAccessException] {
    Write-Error '目标目录不可写,请以管理员身份运行本脚本。'
    exit 1
}
'已写入 ' + $target

if ($Restart) {
    Start-Process -FilePath (Join-Path $QqDir 'QQMusic.exe') -WorkingDirectory $QqDir
    'QQ 音乐已启动。日志在 %TEMP%\QQMusicInjectorLogs。'
} else {
    '请手动启动 QQ 音乐,然后用 ..\probe\smtc-probe.ps1 验证。'
}
