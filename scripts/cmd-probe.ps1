# 命令行命令探针的驱动脚本(配合 payload 里的 cmd-probe 通道)。
#   .\cmd-probe.ps1 -Setup              重启 QQ 音乐 -> 装探针 -> 抓命令行对象 -> 开始播放
#   .\cmd-probe.ps1 -Seek 12345         让插件在进程内调 HandleSeekTo,并打印跳转前后的进度
#   .\cmd-probe.ps1 -Seek "'12345'"     带引号的形态,和真实命令行一致
#   .\cmd-probe.ps1 -Seek 30 -Direct    走生产代码那条路(internal::seek_to)
#   .\cmd-probe.ps1 -Peek               只打印命令行对象的关键字段
# 日志在 %TEMP%\QQMusicInjectorLogs\payload.<日期>.log。

[CmdletBinding()]
param(
    [switch]$Setup,
    [string]$Seek = '',
    [switch]$Direct,
    [switch]$Peek,
    [switch]$Pause
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$logDir = Join-Path $env:TEMP 'QQMusicInjectorLogs'
$flag = Join-Path $logDir 'cmd-probe.flag'
$directive = Join-Path $logDir 'cmd-probe.txt'
$log = Join-Path $logDir ("payload.{0}.log" -f (Get-Date -Format 'yyyy-MM-dd'))

function Resolve-QqDir {
    $key = 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\QQMusic'
    if (Test-Path $key) {
        $loc = (Get-ItemProperty $key).InstallLocation
        if ($loc -and (Test-Path (Join-Path $loc 'QQMusic.exe'))) { return $loc }
    }
    throw '找不到 QQ 音乐版本目录'
}

function Invoke-Smtc([string]$verb) {
    Add-Type -AssemblyName System.Runtime.WindowsRuntime
    $asTask = ([System.WindowsRuntimeSystemExtensions].GetMethods() |
        Where-Object {
            $_.Name -eq 'AsTask' -and $_.IsGenericMethod -and
            $_.GetParameters().Count -eq 1 -and
            $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
        })[0]
    $await = {
        param($op, $t)
        $task = $asTask.MakeGenericMethod($t).Invoke($null, @($op))
        $task.Wait(-1) | Out-Null
        $task.Result
    }
    [void][Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager, Windows.Media, ContentType = WindowsRuntime]
    $mt = [Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager]
    $manager = & $await ($mt::RequestAsync()) $mt
    $session = $manager.GetCurrentSession()
    if (-not $session) { return '<没有会话>' }
    switch ($verb) {
        'play'  { [void](& $await ($session.TryPlayAsync()) ([bool])) }
        'pause' { [void](& $await ($session.TryPauseAsync()) ([bool])) }
    }
    $t = $session.GetTimelineProperties()
    'pos={0:N1}s end={1:N1}s status={2}' -f $t.Position.TotalSeconds, $t.EndTime.TotalSeconds,
        $session.GetPlaybackInfo().PlaybackStatus
}

if ($Setup) {
    New-Item -ItemType Directory -Force -Path $logDir | Out-Null
    Set-Content -Path $flag -Value '' -NoNewline
    Remove-Item -Path $directive -ErrorAction SilentlyContinue

    & (Join-Path $root 'scripts\install.ps1') -Restart | Out-Null
    '已重启 QQ 音乐,等探针就绪...'
    Start-Sleep -Seconds 25

    $qqDir = Resolve-QqDir
    '触发一次命令分发器(第二个实例转发命令行)...'
    Start-Process -FilePath (Join-Path $qqDir 'QQMusic.exe') -ArgumentList "/seekto '1'"
    Start-Sleep -Seconds 5
    Invoke-Smtc 'play'
    '现在可以用 -Seek <值> 做实验了。'
    return
}

if ($Pause) { Invoke-Smtc 'pause'; return }

if (-not (Test-Path $flag)) {
    Write-Error "先运行 .\cmd-probe.ps1 -Setup(需要 $flag)"
    exit 1
}

$before = (Get-Item $log).Length
if ($Peek) {
    Set-Content -Path $directive -Value 'peek' -NoNewline
} elseif ($Seek) {
    $prefix = if ($Direct) { 'direct' } else { 'seek' }
    Set-Content -Path $directive -Value "${prefix}:$Seek" -NoNewline
} else {
    Write-Error '给一个 -Seek <值> 或 -Peek'
    exit 1
}

Start-Sleep -Milliseconds 600
Invoke-Smtc 'none'
Start-Sleep -Seconds 4
Get-Content $log -Encoding UTF8 | Select-Object -Skip ([int]([Math]::Max(0, $before / 120))) |
    Select-String 'HandleSeekTo|命令行 seek|分发器|命令行对象头部' |
    ForEach-Object { $_.Line }
