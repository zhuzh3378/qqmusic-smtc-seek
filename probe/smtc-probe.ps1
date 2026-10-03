# SMTC 会话能力探针 / 可选跳转测试
#   .\smtc-probe.ps1                        列出所有会话与能力位
#   .\smtc-probe.ps1 -SeekSeconds 75        把当前会话跳到第 75 秒并回读
#   .\smtc-probe.ps1 -Follow 10             每 500ms 采样一次进度,共 10 次
param(
    [double]$SeekSeconds = -1,
    [int]$Follow = 0,
    [string]$Match = ''
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Runtime.WindowsRuntime

$asTask = ([System.WindowsRuntimeSystemExtensions].GetMethods() |
    Where-Object {
        $_.Name -eq 'AsTask' -and $_.IsGenericMethod -and
        $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
    })[0]

function Await($operation, $resultType) {
    $task = $asTask.MakeGenericMethod($resultType).Invoke($null, @($operation))
    $task.Wait(-1) | Out-Null
    return $task.Result
}

[void][Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager, Windows.Media, ContentType = WindowsRuntime]
$managerType = [Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager]
$manager = Await ($managerType::RequestAsync()) $managerType

function Show-Capabilities($playbackInfo) {
    # Win11 把 Can* 这组位挂在 Capabilities 子对象上;Windows PowerShell 5.1 的
    # WinRT 投影里可能根本没有它,所以这里能打印多少算多少,真正的判据是 -SeekSeconds。
    $targets = @($playbackInfo)
    $capabilities = $playbackInfo.psobject.Properties['Capabilities']
    if ($capabilities) { $targets += $capabilities.Value }

    foreach ($target in $targets) {
        $target.GetType().GetProperties() | ForEach-Object {
            try { '    {0,-28} = {1}' -f $_.Name, $_.GetValue($target) }
            catch { '    {0,-28} = <读取失败>' -f $_.Name }
        }
    }
}

function Get-TargetSession($manager) {
    $sessions = $manager.GetSessions()
    if ($Match) {
        $sessions = $sessions | Where-Object { $_.SourceAppUserModelId -like "*$Match*" }
    }
    if (-not $sessions -or @($sessions).Count -eq 0) { return $null }
    return @($sessions)[0]
}

$sessions = @($manager.GetSessions())
if ($Match) {
    $sessions = @($sessions | Where-Object { $_.SourceAppUserModelId -like "*$Match*" })
}
"会话数: $($sessions.Count)"

foreach ($session in $sessions) {
    '----'
    'SourceAppUserModelId : ' + $session.SourceAppUserModelId
    try {
        $media = Await ($session.TryGetMediaPropertiesAsync()) ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionMediaProperties])
        'Title              : ' + $media.Title + ' / ' + $media.Artist
    } catch { 'Title              : <读取失败>' }

    'PlaybackInfo:'
    Show-Capabilities $session.GetPlaybackInfo()

    $timeline = $session.GetTimelineProperties()
    'Timeline             : pos={0:0.0}s start={1:0.0}s end={2:0.0}s speed={3}' -f `
        $timeline.Position.TotalSeconds, $timeline.StartTime.TotalSeconds, `
        $timeline.EndTime.TotalSeconds, $timeline.PlaySpeed
    '                     : minSeek={0:0.0}s maxSeek={1:0.0}s' -f `
        $timeline.MinSeekTime.TotalSeconds, $timeline.MaxSeekTime.TotalSeconds
}

if ($SeekSeconds -ge 0) {
    $target = Get-TargetSession $manager
    if (-not $target) { Write-Error '没有可跳转的会话'; exit 1 }
    $before = $target.GetTimelineProperties().Position.TotalSeconds
    # 这里的投影把 TimeSpan 参数收成了 Int64(ticks),直接传 [TimeSpan] 会报转换错误。
    $ticks = [TimeSpan]::FromSeconds($SeekSeconds).Ticks
    $op = $target.TryChangePlaybackPositionAsync($ticks)
    $accepted = Await $op ([bool])
    # 插件内部是"点击 + 回读"的闭环,可能要试到第三次才命中,所以轮询到落定为止。
    $deadline = (Get-Date).AddSeconds(8)
    $after = $before
    $best = [double]::PositiveInfinity
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 400
        $after = $target.GetTimelineProperties().Position.TotalSeconds
        $delta = [Math]::Abs($after - $SeekSeconds)
        if ($delta -lt $best) { $best = $delta }
        if ($delta -le 2.5) { break }
    }
    ''
    '=== TryChangePlaybackPositionAsync({0}s) ===' -f $SeekSeconds
    '会话               : ' + $target.SourceAppUserModelId
    '返回值             : ' + $accepted
    '跳转前 / 跳转后    : {0:0.0}s -> {1:0.0}s (期望 {2:0.0}s, 偏差 {3:0.0}s)' -f $before, $after, $SeekSeconds, $best
    if ($accepted -and $best -le 2.5) { '判定: 成功' } else { '判定: 失败(系统不接受、回调没触发,或点击落点偏了)' }
}

if ($Follow -gt 0) {
    $target = Get-TargetSession $manager
    if (-not $target) { Write-Error '没有可跟踪的会话'; exit 1 }
    ''
    '=== 进度采样(每 500ms) ==='
    for ($i = 0; $i -lt $Follow; $i++) {
        $t = $target.GetTimelineProperties()
        '{0,2}  pos={1:0.0}s status={2}' -f $i, $t.Position.TotalSeconds, $target.GetPlaybackInfo().PlaybackStatus
        Start-Sleep -Milliseconds 500
    }
}
