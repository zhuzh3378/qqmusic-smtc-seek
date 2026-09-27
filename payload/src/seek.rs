//! 让 QQ 音乐自己的 SMTC 会话支持进度跳转。
//!
//! 系统只在两个条件都满足时才把外部的跳转请求派发给应用:timeline 带着
//! `MinSeekTime`/`MaxSeekTime`,并且有人注册了 `PlaybackPositionChangeRequested`。
//! QQ 音乐两样都没做,所以在它的进程里补上:挂住它提交 timeline 的那一步塞进 seek 区间,
//! 再注册回调把请求翻译成一次真实的播放器跳转。

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use minhook::MinHook;
use tracing::{error, info, warn};
use windows::Foundation::TimeSpan;
use windows::Foundation::TypedEventHandler;
use windows::Media::{
    ISystemMediaTransportControls2, ISystemMediaTransportControlsTimelineProperties_Vtbl,
    PlaybackPositionChangeRequestedEventArgs, SystemMediaTransportControls,
    SystemMediaTransportControlsTimelineProperties,
};
use windows::Win32::Foundation::{E_FAIL, HWND, LPARAM, MAX_PATH, RECT, TRUE, WPARAM};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClientRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    IsWindowVisible, PostMessageW, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
};
use windows::core::{BOOL, HRESULT, Interface, Ref, Result};

use crate::{CurrentSongInfo, STATE, safe_call, try_seh};

/// 一毫秒对应的 `TimeSpan` 刻度。
const TICKS_PER_MS: i64 = 10_000;

/// 进度条在客户区里的默认几何。真值由标定覆盖,这里只当第一次点击的初值。
/// 数值取自 QQ 音乐 22.71 沉浸播放页的实测截图:轨道 x∈[446,778]、y=776(窗口 1226x800)。
/// 左端比例只决定第一次点击落在哪,标定两次就能收敛到真实映射,所以不必精确。
const TRACK_LEFT_FRAC: f64 = 0.36;
const TRACK_RIGHT_FRAC: f64 = 0.36;
/// 进度条贴底,在播放按钮那一行的下面。
const TRACK_BOTTOM_FRAC: f64 = 0.03;

/// 落点容差的下限,秒级刷新的进度字段再准也快不过这值。
const TOLERANCE_FLOOR_MS: u64 = 1_200;
/// 第一枪的容忍度:小于这个值就不再补跳,免得用户看到连续两次跳动。
const FIRST_SHOT_GRACE_MS: u64 = 12_000;
const MAX_CLICKS: u32 = 4;
/// `/seekto` 只认整秒并向下取整,所以 1 秒出头的误差是它的固有精度,不该因此再补一次点击。
const COMMAND_SEEK_TOLERANCE_MS: u64 = 1_100;

static SMTX: OnceLock<SystemMediaTransportControls> = OnceLock::new();
static MAIN_HWND: AtomicUsize = AtomicUsize::new(0);
static DURATION_MS: AtomicU32 = AtomicU32::new(0);
static SONG_ID: AtomicU32 = AtomicU32::new(0);
static SEEKING: AtomicBool = AtomicBool::new(false);
static PENDING_TARGET: AtomicUsize = AtomicUsize::new(0);
static ORIGINAL_UPDATE_TIMELINE: AtomicUsize = AtomicUsize::new(0);

/// 标定结果:进度 = 斜率 × x + 截距。按窗口几何缓存,换歌或改窗口大小就作废。
static CALIBRATION: Mutex<Option<Calibration>> = Mutex::new(None);

/// 标定按窗口尺寸持久化,这样重启 QQ 音乐后的第一次跳转也不用重新试探。
static CAL_DIR: OnceLock<std::path::PathBuf> = OnceLock::new();

fn cal_file() -> std::path::PathBuf {
    CAL_DIR
        .get_or_init(|| {
            let dir = std::env::var_os("LOCALAPPDATA")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join("QQMusicSmtcSeek");
            let _ = std::fs::create_dir_all(&dir);
            dir.join("bar-mapping.cfg")
        })
        .clone()
}

fn save_cal(win_w: i32, win_h: i32, slope: f64, intercept: f64) {
    let text = format!("{win_w} {win_h} {slope} {intercept}");
    if let Err(e) = std::fs::write(cal_file(), text) {
        warn!("标定结果写盘失败: {e}");
    }
}

fn load_cal(win_w: i32, win_h: i32) -> Option<(f64, f64)> {
    let text = std::fs::read_to_string(cal_file()).ok()?;
    let mut it = text.split_whitespace();
    let w: i32 = it.next()?.parse().ok()?;
    let h: i32 = it.next()?.parse().ok()?;
    let slope: f64 = it.next()?.parse().ok()?;
    let intercept: f64 = it.next()?.parse().ok()?;
    // 窗口尺寸变了进度条就重新找。
    (w == win_w && h == win_h && slope.abs() > 1e-3).then_some((slope, intercept))
}

#[derive(Clone, Copy)]
struct Calibration {
    key: usize,
    slope_ms_per_px: f64,
    intercept_ms: f64,
    /// 来自像素测量的标定不被点击拟合覆盖:拟合样本带着写入延迟,精度反而更差。
    from_pixels: bool,
}

/// 进度条在客户区里的位置和宽度,以及标定所属的键。
#[derive(Clone, Copy)]
struct Track {
    left: f64,
    width: f64,
    click_y: f64,
    duration_ms: u64,
    key: usize,
    win_w: i32,
    win_h: i32,
}

/// 装 timeline hook 并注册跳转回调,由 `install_hook` 在拿到 SMTC 实例后调用。
pub(crate) unsafe fn install(smtc: &SystemMediaTransportControls, main_hwnd: HWND) -> Result<()> {
    MAIN_HWND.store(main_hwnd.0 as usize, Ordering::Relaxed);
    let _ = SMTX.set(smtc.clone());

    unsafe { hook_timeline(smtc)? };
    unsafe { register_position_handler(smtc)? };

    Ok(())
}

/// 挂钩 `ISystemMediaTransportControls::UpdateTimelineProperties`。
///
/// hook 的是接口 vtable 里的实现地址,进程内该实现类的所有实例都经过我们 —— 正好是需要
/// 的效果:QQ 音乐每次刷 timeline 都会被补上 seek 区间,而不是我们提交一次就被它覆盖掉。
unsafe fn hook_timeline(smtc: &SystemMediaTransportControls) -> Result<()> {
    // `UpdateTimelineProperties` 在 Win11 之后被挪到了 ISystemMediaTransportControls2 上。
    let ctrl: ISystemMediaTransportControls2 = smtc.cast()?;
    let target = Interface::vtable(&ctrl).UpdateTimelineProperties as *mut c_void;
    if target.is_null() {
        error!("UpdateTimelineProperties 地址为空");
        return Err(E_FAIL.into());
    }

    let original = unsafe { MinHook::create_hook(target, detour_update_timeline as *mut c_void) }
        .map_err(|e| {
        error!("挂钩 UpdateTimelineProperties 失败: {e:?}");
        windows::core::Error::from(E_FAIL)
    })?;
    ORIGINAL_UPDATE_TIMELINE.store(original as usize, Ordering::Relaxed);
    unsafe { MinHook::enable_hook(target) }.map_err(|e| {
        error!("启用 UpdateTimelineProperties hook 失败: {e:?}");
        windows::core::Error::from(E_FAIL)
    })?;

    info!(address = ?target, "已挂住 timeline 提交,对外开始声明 seek 能力");
    Ok(())
}

/// QQ 音乐调用 `UpdateTimelineProperties` 时经过这里。要保持轻:不分配、不阻塞。
unsafe extern "system" fn detour_update_timeline(this: *mut c_void, value: *mut c_void) -> HRESULT {
    if this.is_null() || value.is_null() {
        return E_FAIL;
    }

    safe_call((), || {
        let duration_ms = read_song(|song| song.duration) as u64;
        if duration_ms == 0 {
            return;
        }
        DURATION_MS.store(duration_ms.min(u32::MAX as u64) as u32, Ordering::Relaxed);
        SONG_ID.store(read_song(|song| song.id), Ordering::Relaxed);
        if let Err(e) = unsafe { force_seek_range(value, duration_ms) } {
            warn!("写入 seek 区间失败: {e}");
        }
    });

    let original = ORIGINAL_UPDATE_TIMELINE.load(Ordering::Relaxed);
    if original == 0 {
        return E_FAIL;
    }
    let original: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT =
        unsafe { std::mem::transmute(original) };
    unsafe { original(this, value) }
}

/// 把传入 timeline 对象的 seek 区间改成 `[0, 时长]`。
unsafe fn force_seek_range(value: *mut c_void, duration_ms: u64) -> Result<()> {
    let vtbl =
        unsafe { &**value.cast::<*const ISystemMediaTransportControlsTimelineProperties_Vtbl>() };
    unsafe { (vtbl.SetMinSeekTime)(value, TimeSpan { Duration: 0 }).ok() }?;
    unsafe {
        (vtbl.SetMaxSeekTime)(
            value,
            TimeSpan {
                Duration: duration_ms as i64 * TICKS_PER_MS,
            },
        )
        .ok()
    }
}

unsafe fn register_position_handler(smtc: &SystemMediaTransportControls) -> Result<()> {
    let handler = TypedEventHandler::new(
        |_sender: Ref<SystemMediaTransportControls>,
         args: Ref<PlaybackPositionChangeRequestedEventArgs>| {
            let Some(args) = args.as_ref() else {
                return Ok(());
            };
            let Ok(position) = args.RequestedPlaybackPosition() else {
                warn!("读取请求的跳转位置失败");
                return Ok(());
            };
            request_seek((position.Duration / TICKS_PER_MS).max(0) as u64);
            Ok(())
        },
    );
    let token = smtc.PlaybackPositionChangeRequested(&handler)?;
    info!(token, "跳转回调已注册");
    Ok(())
}

/// 外部可能会连点进度条,同一时间只跑一个跳转;忙的时候把最新目标记下来,跑完立刻接上,
/// 而不是把请求丢掉(丢掉会让拖动收尾那一下失效)。
fn request_seek(target_ms: u64) {
    if SEEKING.swap(true, Ordering::SeqCst) {
        PENDING_TARGET.store(target_ms as usize, Ordering::SeqCst);
        return;
    }
    let spawned = thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(move || {
            struct Busy;
            impl Drop for Busy {
                fn drop(&mut self) {
                    SEEKING.store(false, Ordering::SeqCst);
                }
            }
            let _busy = Busy;
            let mut current = target_ms;
            loop {
                safe_call((), || perform_seek(current));
                match PENDING_TARGET.swap(0, Ordering::SeqCst) {
                    0 => break,
                    next => current = next as u64,
                }
            }
        });
    if let Err(e) = spawned {
        SEEKING.store(false, Ordering::SeqCst);
        error!("启动跳转线程失败: {e}");
    }
}

/// 跳到 `target_ms`。首选直接调 QQ 音乐自己的 `/seekto` 命令;它不可用或没到位时,
/// 才退回模拟点击 —— 点击是开环的,需要拿前两次点击当探针标定坐标到进度的映射。
fn perform_seek(target_ms: u64) {
    let duration_ms = match live_duration() {
        Some(d) => d,
        None => {
            warn!("还没读到歌曲时长,无法跳转");
            return;
        }
    };
    let target_ms = target_ms.min(duration_ms.saturating_sub(1));

    if crate::internal::available() {
        let before = read_song(|song| song.progress) as u64;
        if crate::internal::seek_to(target_ms) {
            let actual = wait_for_settle(before, duration_ms);
            info!(
                target_ms,
                actual_ms = actual,
                error_ms = actual.abs_diff(target_ms) as i64,
                via = "命令直调",
                "跳转完成"
            );
            push_timeline(actual, duration_ms);
            if actual.abs_diff(target_ms) <= COMMAND_SEEK_TOLERANCE_MS {
                return;
            }
            warn!(target_ms, actual_ms = actual, "命令直调没到位,改用点击兜底");
        }
    }

    let Some(track) = current_track(duration_ms) else {
        warn!("找不到承载进度条的窗口,无法跳转");
        return;
    };
    let tolerance = TOLERANCE_FLOOR_MS.max(duration_ms * 15 / 1_000);
    let ms_per_px = track.width / duration_ms.max(1) as f64;

    let mut samples: Vec<(f64, u64)> = Vec::new();
    let mut x = track.x_of(target_ms, cached_calibration(&track));

    for attempt in 1..=MAX_CLICKS {
        let before = read_song(|song| song.progress) as u64;
        if !unsafe { click_at(&track, x) } {
            return;
        }
        let actual = wait_for_settle(before, duration_ms);
        // 进度没动说明这次点在了轨道外,这个样本不能进拟合,否则斜率会被污染。
        let landed = actual.abs_diff(before) > 400;
        let error_ms = actual.abs_diff(target_ms) as i64;
        info!(
            attempt,
            x = format_args!("{x:.0}"),
            target_ms,
            actual_ms = actual,
            error_ms,
            landed,
            "跳转点击已生效"
        );

        // 像素测量给的是精确映射,单次点击的量化误差只有半个像素。回读值反而滞后几秒,
        // 所以第一枪只要没明显落偏就直接接受,别为了追平滞后的读数再跳一次。
        let first_shot_ok = attempt == 1 && actual.abs_diff(target_ms) <= FIRST_SHOT_GRACE_MS;
        if first_shot_ok || actual.abs_diff(target_ms) <= tolerance {
            if landed {
                samples.push((x, actual));
                remember_calibration(&track, &samples);
            }
            push_timeline(actual, duration_ms);
            return;
        }

        if landed {
            samples.push((x, actual));
        }
        x = if !landed {
            warn!(x = format_args!("{x:.0}"), "点击没落在进度条上,向中心试探");
            track.nudge_toward_center(x)
        } else if samples.len() >= 2 {
            let (x1, m1) = samples[0];
            let (x2, m2) = samples[1];
            let slope = (m2 as f64 - m1 as f64) / (x2 - x1);
            if slope.abs() < 1e-3 {
                break;
            }
            track.clamp_x(x1 + (target_ms as f64 - m1 as f64) / slope)
        } else {
            track.clamp_x(x + (target_ms as f64 - actual as f64) * ms_per_px)
        };
    }
    warn!(target_ms, "点击多次仍未落进容差,跳转可能没生效");
}

fn live_duration() -> Option<u64> {
    let from_memory = read_song(|song| song.duration) as u64;
    if from_memory > 0 {
        return Some(from_memory);
    }
    // 只在内存里读不到时长时才用缓存(切歌瞬间、部分本地曲),而且必须还是同一首歌 ——
    // 否则会把上一首的时长当成当前时长,提交出去的 EndTime 就是错的。
    let cached = DURATION_MS.load(Ordering::Relaxed) as u64;
    if cached == 0 || SONG_ID.load(Ordering::Relaxed) != read_song(|song| song.id) {
        return None;
    }
    Some(cached)
}

/// 点完之后等进度落定。播放中进度本身在以 1ms/ms 推进,所以"稳定"的判据是
/// 相邻两次读数的差值贴合真实经过时间,而不是读数不变。
fn wait_for_settle(before: u64, duration_ms: u64) -> u64 {
    let jump_min = (duration_ms / 40).max(400);
    let started = Instant::now();
    let deadline = started + Duration::from_millis(2_500);
    let mut prev_read = before;
    let mut prev_at = started;
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
        let now = Instant::now();
        let value = read_song(|song| song.progress) as u64;
        let elapsed = (now - prev_at).as_millis() as u64;
        let moved = value.abs_diff(prev_read) as i64;
        let drifted = (moved - elapsed as i64).abs();
        // 先等到点击真的改变了进度,再等到它不再大幅变化。
        if value.abs_diff(before) > jump_min && drifted < 120 {
            return value;
        }
        prev_read = value;
        prev_at = now;
    }
    read_song(|song| song.progress) as u64
}

pub(crate) fn read_song(field: impl Fn(&CurrentSongInfo) -> u32) -> u32 {
    let addr = STATE.song_struct_addr.load(Ordering::Relaxed);
    if addr == 0 {
        return 0;
    }
    try_seh(|| unsafe { field(&*(addr as *const CurrentSongInfo)) }).unwrap_or(0)
}

fn current_track(duration_ms: u64) -> Option<Track> {
    let hwnd = track_window()?;
    let mut rect = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut rect) }.is_err() {
        return None;
    }
    let width = f64::from(rect.right - rect.left);
    let height = f64::from(rect.bottom - rect.top);
    if width < 480.0 || height < 320.0 {
        return None;
    }
    let song_id = read_song(|song| song.id);
    let mut left = width * TRACK_LEFT_FRAC;
    let mut span = (width * (1.0 - TRACK_LEFT_FRAC - TRACK_RIGHT_FRAC)).max(1.0);
    let mut click_y = height - height * TRACK_BOTTOM_FRAC;

    // 能直接量到轨道端点就用量到的,默认比例只是量不到时的兜底。
    let progress = read_song(|song| song.progress) as f64;
    let expected_ratio = (progress / (duration_ms.max(1) as f64)).clamp(0.0, 1.0);
    let measured = unsafe { crate::bar::measure(hwnd, expected_ratio) };
    if let Some(bar) = measured {
        left = bar.x0;
        span = (bar.x1 - bar.x0).max(1.0);
        click_y = bar.y;
    }

    let track = Track {
        left,
        width: span,
        click_y,
        duration_ms,
        win_w: rect.right - rect.left,
        win_h: rect.bottom - rect.top,
        key: (song_id as usize)
            .wrapping_mul(1_000_003)
            .wrapping_add(width.to_bits() as usize)
            .wrapping_add(duration_ms as usize),
    };
    if let Some(bar) = measured {
        // 量到的端点等价于一条精确的线性映射,直接当标定存下来,第一枪就落准。
        let slope = duration_ms as f64 / (bar.x1 - bar.x0).max(1.0);
        apply_calibration(&track, slope, -bar.x0 * slope, true);
    }
    Some(track)
}

impl Track {
    fn x_of(&self, target_ms: u64, cal: Option<(f64, f64)>) -> f64 {
        let t = target_ms as f64;
        let x = match cal {
            Some((slope, intercept)) if slope.abs() > 1e-6 => (t - intercept) / slope,
            // 还没标定过,退回默认几何比例。
            _ => self.left + self.width * t / self.duration_ms.max(1) as f64,
        };
        self.clamp_x(x)
    }

    fn clamp_x(&self, x: f64) -> f64 {
        x.clamp(self.left, self.left + self.width)
    }

    /// 点空了的时候用:朝轨道中点挪一档,步长按轨道跨度算。
    fn nudge_toward_center(&self, x: f64) -> f64 {
        let center = self.left + self.width / 2.0;
        let step = self.width * 0.08;
        if x < center {
            (x + step).min(center)
        } else {
            (x - step).max(center)
        }
    }
}

fn cached_calibration(track: &Track) -> Option<(f64, f64)> {
    let in_memory = CALIBRATION.lock().ok().and_then(|guard| {
        guard
            .as_ref()
            .filter(|c| c.key == track.key)
            .map(|c| (c.slope_ms_per_px, c.intercept_ms))
    });
    in_memory.or_else(|| load_cal(track.win_w, track.win_h))
}

/// 只有一个样本时信息不够,留着等下一次跳转凑够两个再存。
fn remember_calibration(track: &Track, samples: &[(f64, u64)]) {
    let Some(&(x1, m1)) = samples.first() else {
        return;
    };
    let Some(&(x2, m2)) = samples.last().filter(|_| samples.len() >= 2) else {
        return;
    };
    if (x2 - x1).abs() < 1.0 {
        return;
    }
    let slope = (m2 as f64 - m1 as f64) / (x2 - x1);
    apply_calibration(track, slope, m1 as f64 - slope * x1, false);
}

/// 记下"进度 = 斜率 × x + 截距",并按窗口尺寸存盘供下次启动复用。
fn apply_calibration(track: &Track, slope_ms_per_px: f64, intercept_ms: f64, from_pixels: bool) {
    if slope_ms_per_px.abs() < 1e-3 {
        return;
    }
    if let Ok(mut guard) = CALIBRATION.lock() {
        if !from_pixels
            && guard
                .as_ref()
                .is_some_and(|c| c.from_pixels && c.key == track.key)
        {
            return; // 已有像素标定,不让含延迟的拟合覆盖它
        }
        *guard = Some(Calibration {
            key: track.key,
            slope_ms_per_px,
            intercept_ms,
            from_pixels,
        });
        save_cal(track.win_w, track.win_h, slope_ms_per_px, intercept_ms);
        info!(
            slope_ms_per_px = format_args!("{slope_ms_per_px:.2}"),
            intercept_ms = format_args!("{:.0}", intercept_ms),
            "进度条映射已标定,后续跳转一次命中"
        );
    }
}

/// 自己再提交一次 timeline,让锁屏和任务栏的进度条立刻落到新位置,不用等 QQ 音乐刷新。
fn push_timeline(position_ms: u64, duration_ms: u64) {
    let Some(smtc) = SMTX.get() else { return };
    let result: Result<()> = safe_call(Err(E_FAIL.into()), || {
        let span = |ms: u64| TimeSpan {
            Duration: ms as i64 * TICKS_PER_MS,
        };
        let timeline = SystemMediaTransportControlsTimelineProperties::new()?;
        timeline.SetStartTime(span(0))?;
        timeline.SetMinSeekTime(span(0))?;
        timeline.SetPosition(span(position_ms))?;
        timeline.SetMaxSeekTime(span(duration_ms))?;
        timeline.SetEndTime(span(duration_ms))?;
        smtc.UpdateTimelineProperties(&timeline)
    });
    if let Err(e) = result {
        warn!("回写进度位置失败: {e}");
    }
}

/// 给进度条位置发一对鼠标消息。QQ 音乐的界面是它自绘的,收到消息就跳,
/// 不需要窗口在前台,也不移动真实光标。
unsafe fn click_at(track: &Track, x: f64) -> bool {
    let Some(hwnd) = track_window() else {
        warn!("找不到播放器窗口");
        return false;
    };
    if unsafe { IsIconic(hwnd) }.as_bool() {
        warn!("播放器窗口被最小化,进度条坐标不可用");
        return false;
    }
    let point = LPARAM((((track.click_y as i32) << 16) | ((x as i32) & 0xFFFF)) as isize);
    // 进度条默认是隐藏的,要先有鼠标移动把它唤出来,否则按下会落在空白处。
    if unsafe { PostMessageW(Some(hwnd), WM_MOUSEMOVE, WPARAM(0), point) }.is_err() {
        warn!("发送鼠标移动消息失败");
        return false;
    }
    thread::sleep(Duration::from_millis(160));
    let down = unsafe { PostMessageW(Some(hwnd), WM_LBUTTONDOWN, WPARAM(1), point) };
    let up = unsafe { PostMessageW(Some(hwnd), WM_LBUTTONUP, WPARAM(0), point) };
    match (down, up) {
        (Ok(()), Ok(())) => true,
        _ => {
            warn!("发送点击消息失败");
            false
        }
    }
}

/// 找出真正承载进度条的窗口:属于本进程、可见、标题像"歌名 - 歌手"、客户区最大的那个。
/// 标题过滤是为了甩掉托盘、动态歌词、输入法这些辅助窗口。
fn track_window() -> Option<HWND> {
    let mut state = WindowSearch {
        pid: unsafe { GetCurrentProcessId() },
        best: HWND(std::ptr::null_mut()),
        best_area: 0,
    };

    let lparam = LPARAM(&raw mut state as isize);
    if unsafe { EnumWindows(Some(probe_window), lparam) }.is_err() {
        warn!("EnumWindows 失败");
    }

    if !state.best.0.is_null() {
        return Some(state.best);
    }
    // 退回到注入时记住的主窗口。
    match MAIN_HWND.load(Ordering::Relaxed) {
        0 => None,
        raw => Some(HWND(raw as *mut c_void)),
    }
}

struct WindowSearch {
    pid: u32,
    best: HWND,
    best_area: i32,
}

unsafe extern "system" fn probe_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // EnumWindows 是同步的,state 在回调期间一直有效。
    let state = unsafe { &mut *(lparam.0 as *mut WindowSearch) };

    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid != state.pid || !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return TRUE;
    }

    let mut buffer = [0u16; MAX_PATH as usize];
    let len = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    if len == 0 {
        return TRUE;
    }
    let title = String::from_utf16_lossy(&buffer[..len as usize]);
    if !title.contains(" - ") && !title.contains('(') {
        return TRUE;
    }

    let mut rect = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut rect) }.is_err() {
        return TRUE;
    }
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    if w < 480 || h < 320 {
        return TRUE;
    }
    if w * h > state.best_area {
        state.best_area = w * h;
        state.best = hwnd;
    }
    TRUE
}
