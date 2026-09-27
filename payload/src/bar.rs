//! 从窗口自己的像素里量出进度条的真实端点。
//!
//! 进度条的位置随页面而变(沉浸页和普通主界面就不一样),写死几何必然错。
//! 我们本来就在 QQ 音乐进程里,直接 `PrintWindow` 抓自己的窗口再扫一遍底部,
//! 就能拿到轨道左右端点,第一次点击就能落准,不用靠几次跳转去试。

use std::ffi::c_void;
use std::ptr;
use std::thread::sleep;
use std::time::Duration;

use tracing::{debug, info};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, HGDIOBJ, SelectObject,
};
use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, PostMessageW, WM_MOUSEMOVE};

/// 轨道所在的那一行一定在客户区底部这个带里。
const BAND_TOP: f64 = 0.86;
const BAND_BOTTOM: f64 = 0.995;
/// 轨道至少要有这么宽才可信(相对客户区宽度)。
const MIN_RUN_FRAC: f64 = 0.12;
/// 与背景色的最小可辨距离。
const COLOR_DISTANCE: i32 = 60;
/// 量到的断点比例与真实进度比例的最大偏差。
const RATIO_TOLERANCE: f64 = 0.06;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Bar {
    pub x0: f64,
    pub x1: f64,
    pub y: f64,
}

/// 悬停唤出控制条,然后量出进度条端点。
/// `expected_ratio` 是当前进度占时长的比例,用来验证量到的确实是进度条:
/// 播放段/未播放段的断点位置必须和它吻合,否则就是挑错了横条(比如最大化时的整宽底栏)。
pub(crate) unsafe fn measure(hwnd: HWND, expected_ratio: f64) -> Option<Bar> {
    let mut rect = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rect) }.ok()?;
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width < 480 || height < 320 {
        return None;
    }

    // 控制条默认隐藏,先把鼠标停在底部把它唤出来,并等界面画一帧。
    let hover = POINT {
        x: width / 2,
        y: (height as f64 * 0.97) as i32,
    };
    let point = LPARAM((((hover.y) << 16) | ((hover.x) & 0xFFFF)) as isize);
    unsafe { PostMessageW(Some(hwnd), WM_MOUSEMOVE, WPARAM(0), point) }.ok()?;
    sleep(Duration::from_millis(320));

    let frame = unsafe { grab(width, height, hwnd)? };
    let bar = find_track(&frame.pixels, frame.stride, width, height, expected_ratio)?;
    info!(
        x0 = format_args!("{:.0}", bar.x0),
        x1 = format_args!("{:.0}", bar.x1),
        y = format_args!("{:.0}", bar.y),
        client = format_args!("{width}x{height}"),
        "已从像素量出进度条端点"
    );
    Some(bar)
}

struct Frame {
    pixels: Vec<u8>,
    stride: usize,
}

/// 把窗口画进一张 32bpp 位图并立刻拷出来,所有 GDI 对象在这里就释放干净。
unsafe fn grab(width: i32, height: i32, hwnd: HWND) -> Option<Frame> {
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            debug!("CreateCompatibleDC 失败");
            return None;
        }
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // 负值 = 自上而下的位图,像素顺序和屏幕坐标一致。
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = ptr::null_mut();
        let Ok(bitmap) = CreateDIBSection(Some(dc), &mut info, DIB_RGB_COLORS, &mut bits, None, 0)
        else {
            let _ = DeleteDC(dc);
            debug!("CreateDIBSection 失败");
            return None;
        };
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ::from(bitmap));
            let _ = DeleteDC(dc);
            debug!("CreateDIBSection 返回了空位图");
            return None;
        }
        let previous = SelectObject(dc, HGDIOBJ::from(bitmap));
        // PW_RENDERFULLCONTENT 让自绘内容也进位图。
        let painted = PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(2));
        SelectObject(dc, previous);
        let result = if painted.as_bool() {
            let stride = (width * 4) as usize;
            let mut pixels = vec![0u8; stride * height as usize];
            unsafe {
                std::ptr::copy_nonoverlapping(bits as *const u8, pixels.as_mut_ptr(), pixels.len())
            };
            Some(Frame { pixels, stride })
        } else {
            debug!("PrintWindow 失败");
            None
        };
        let _ = DeleteObject(HGDIOBJ::from(bitmap));
        let _ = DeleteDC(dc);
        result
    }
}

fn find_track(
    frame: &[u8],
    stride: usize,
    width: i32,
    height: i32,
    expected_ratio: f64,
) -> Option<Bar> {
    let background = pixel(frame, stride, 4, 4);
    let top = (height as f64 * BAND_TOP) as i32;
    let bottom = (height as f64 * BAND_BOTTOM) as i32;
    let min_run = (width as f64 * MIN_RUN_FRAC) as i32;

    // 逐行找线段,再用"已播放段占比是否等于进度占比"来认定哪一条才是进度条。
    let mut best: Option<(f64, Bar)> = None;
    for y in top..bottom {
        let Some((_run, x0, x1)) = longest_run(frame, stride, y, width, background) else {
            continue;
        };
        if x1 - x0 < min_run {
            continue;
        }
        // 进度条是几像素高的细线:上下相邻行应该都是背景。整宽色块(比如底栏)不会满足。
        if !is_thin_line(frame, stride, y, width, x0, x1, background) {
            continue;
        }
        let Some(break_x) = find_break(frame, stride, y, x0, x1, background) else {
            continue;
        };
        let span = (x1 - x0 + 1) as f64;
        let ratio = (break_x - x0 as f64) / span;
        let error = (ratio - expected_ratio).abs();
        if error > RATIO_TOLERANCE {
            continue;
        }
        let bar = Bar {
            x0: x0 as f64,
            x1: (x1 + 1) as f64,
            y: y as f64,
        };
        if best.is_none_or(|(best_error, _)| error < best_error) {
            best = Some((error, bar));
        }
    }

    match best {
        Some((error, bar)) => {
            debug!(error = format_args!("{error:.3}"), "行校验通过");
            Some(bar)
        }
        None => {
            debug!(expected_ratio, "底部没有一行和进度比例对得上,不用像素结果");
            None
        }
    }
}

/// 这一行是不是细线:上下各一行在同样的横向跨度内几乎全是背景。
fn is_thin_line(
    frame: &[u8],
    stride: usize,
    y: i32,
    width: i32,
    x0: i32,
    x1: i32,
    background: [i32; 3],
) -> bool {
    for probe in [y - 3, y + 3] {
        if probe <= 0 || probe >= height_of(stride, frame) {
            return false;
        }
        let mut non_background = 0;
        let mut x = x0;
        while x <= x1 {
            if differs(pixel(frame, stride, x, probe), background) {
                non_background += 1;
            }
            x += 1;
        }
        // 相邻行里非背景像素超过跨度的一成就说明这不是细线。
        if non_background * 4 > (x1 - x0 + 1) {
            return false;
        }
    }
    let _ = width;
    true
}

/// 位图高度(按 stride 反推)。
fn height_of(stride: usize, frame: &[u8]) -> i32 {
    (frame.len() / stride.max(1)) as i32
}

/// 已播放段与未播放段的分界:从左端起第一次出现明显色差的位置。
fn find_break(
    frame: &[u8],
    stride: usize,
    y: i32,
    x0: i32,
    x1: i32,
    background: [i32; 3],
) -> Option<f64> {
    let left_color = pixel(frame, stride, x0 + 2, y);
    let mut x = x0 + 4;
    while x <= x1 - 2 {
        let here = pixel(frame, stride, x, y);
        if differs(here, background) && differs(here, left_color) {
            return Some(x as f64);
        }
        x += 1;
    }
    None
}

/// 一行里最长的"非背景"连续段。
fn longest_run(
    frame: &[u8],
    stride: usize,
    y: i32,
    width: i32,
    background: [i32; 3],
) -> Option<(i32, i32, i32)> {
    let mut best = (0, 0, 0);
    let mut start = -1;
    for x in 0..width {
        if differs(pixel(frame, stride, x, y), background) {
            if start < 0 {
                start = x;
            }
        } else if start >= 0 {
            if x - start > best.0 {
                best = (x - start, start, x - 1);
            }
            start = -1;
        }
    }
    if start >= 0 && width - start > best.0 {
        best = (width - start, start, width - 1);
    }
    (best.0 > 0).then_some(best)
}

fn differs(a: [i32; 3], b: [i32; 3]) -> bool {
    (a[0] - b[0]).abs() + (a[1] - b[1]).abs() + (a[2] - b[2]).abs() > COLOR_DISTANCE
}

/// 读一个像素(BGRA 字节序,这里只当三元组用)。
fn pixel(frame: &[u8], stride: usize, x: i32, y: i32) -> [i32; 3] {
    let offset = (y as usize) * stride + (x as usize) * 4;
    [
        frame[offset] as i32,
        frame[offset + 1] as i32,
        frame[offset + 2] as i32,
    ]
}
