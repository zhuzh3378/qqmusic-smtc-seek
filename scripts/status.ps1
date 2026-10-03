# 只读地报告 QQ 音乐目录里现在装了什么。不改任何东西。
#   .\status.ps1
[CmdletBinding()]
param()

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
    # 注册表没指到有效目录时,退而求其次找带版本号的子目录,再退到外层。
    $root = 'C:\Program Files (x86)\Tencent\QQMusic'
    $versioned = Get-ChildItem $root -Directory -Filter 'QQMusic*' -ErrorAction SilentlyContinue |
        Where-Object { Test-Path (Join-Path $_.FullName 'QQMusic.exe') } |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if ($versioned) { return $versioned.FullName }
    if (Test-Path (Join-Path $root 'QQMusic.exe')) { return $root }
    return $null
}

# 用文件里残留的字符串判断这份 msimg32.dll 到底是谁 —— 只看大小容易误判。
function Get-PayloadIdentity {
    param([byte[]]$Bytes)
    $text = [System.Text.Encoding]::UTF8.GetString($Bytes)
    if ($text -match "/seekto '%s'") { return '本项目(seek 插件)' }
    if ($text -match 'QQMusicInjectorLogs') { return 'QQMusic-ID-Injector(上游,只有 ID/封面,没有 seek)' }
    return '未知来源'
}

$dir = Resolve-QqVersionDir
if (-not $dir) {
    Write-Host '找不到 QQ 音乐安装目录(注册表里没有,公共路径下也没有 QQMusic.exe)。' -ForegroundColor Yellow
    exit 1
}
"安装目录 : $dir"

$msimg = Join-Path $dir 'msimg32.dll'
if (-not (Test-Path $msimg)) {
    'msimg32  : 不存在 —— 未安装任何注入插件'
} else {
    $item  = Get-Item $msimg
    $bytes = [IO.File]::ReadAllBytes($msimg)
    $pe     = [BitConverter]::ToInt32($bytes, 0x3C)
    $machine = [BitConverter]::ToUInt16($bytes, $pe + 4)
    $bits = if ($machine -eq 0x014C) { '32 位' } elseif ($machine -eq 0x8664) { '64 位' } else { '未知 0x{0:X4}' -f $machine }
    'msimg32  : {0:N0} 字节, 改动于 {1}, {2} <- {3}' -f $item.Length, $item.LastWriteTime, $bits, (Get-PayloadIdentity $bytes)
    if ($machine -ne 0x014C) {
        Write-Host '           !! QQ 音乐是 32 位进程,这份会让它起不来,应立刻删除 !!' -ForegroundColor Red
    }
}

# 早期版本还会把 SMTCFeature.dll 换成代理,现在不需要了;残留的话提示一下。
$smtcReal = Join-Path $dir 'SMTCFeatureReal.dll'
if (Test-Path $smtcReal) {
    Write-Host 'SMTCFeature: 检测到历史代理层残留(SMTCFeatureReal.dll),建议跑一次 uninstall 还原。' -ForegroundColor Yellow
} else {
    $smtc = Join-Path $dir 'SMTCFeature.dll'
    if (Test-Path $smtc) {
        $s = Get-Item $smtc
        'SMTCFeature: 原版在位({0:N0} 字节, {1})' -f $s.Length, $s.LastWriteTime
    }
}

$running = Get-Process -Name 'QQMusic' -ErrorAction SilentlyContinue
if ($running) {
    '运行状态 : 正在运行(PID ' + (($running.Id) -join ', ') + ')'
} else {
    '运行状态 : 未运行'
}
