# 卸载:删掉注册表所指安装目录里的 msimg32.dll(装了插件的那个目录)。
# 如果安装时备份过原始文件则还原备份。
# 历史版本还会把 SMTCFeature.dll 换成代理,这里一并还原。
#   .\uninstall.ps1
[CmdletBinding()]
param(
    [string]$QqDir = ''
)

$ErrorActionPreference = 'Stop'

if (-not $QqDir) {
    foreach ($key in 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\QQMusic',
                     'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\QQMusic') {
        if (Test-Path $key) {
            $loc = (Get-ItemProperty $key).InstallLocation
            if ($loc -and (Test-Path (Join-Path $loc 'QQMusic.exe'))) { $QqDir = $loc; break }
        }
    }
}
if (-not $QqDir) { Write-Error '找不到 QQ 音乐目录,请用 -QqDir 指定。'; exit 1 }
"目标目录: $QqDir"

$running = Get-Process -Name 'QQMusic', 'QQMusicExternal', 'QQMusicAgent', 'DesktopDynamicLyric' -ErrorAction SilentlyContinue
if ($running) {
    '正在结束 QQ 音乐进程: ' + ($running.ProcessName -join ', ')
    $running | Stop-Process -Force
    Start-Sleep -Seconds 2
}

$target = Join-Path $QqDir 'msimg32.dll'
$backup = "$target.orig.bak"

if (Test-Path $backup) {
    Move-Item $backup $target -Force
    '已还原原始 msimg32.dll'
} elseif (Test-Path $target) {
    Remove-Item $target -Force
    '已删除 ' + $target
} else {
    '没有需要清理的文件。'
}


# 代理层还原:删掉我们的 SMTCFeature.dll,把原版改回来。
$smtc = Join-Path $QqDir 'SMTCFeature.dll'
$smtcReal = Join-Path $QqDir 'SMTCFeatureReal.dll'
if (Test-Path $smtcReal) {
    if (Test-Path $smtc) { Remove-Item $smtc -Force }
    Move-Item $smtcReal $smtc
    '已还原原版 SMTCFeature.dll'
}

$logDir = Join-Path $env:TEMP 'QQMusicInjectorLogs'
if (Test-Path $logDir) {
    Remove-Item $logDir -Recurse -Force
    '已清理日志目录'
}
