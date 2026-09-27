//! 探 QQ 音乐自己的命令行命令通道,看能不能用它做 seek(替代模拟点击)。
//!
//! 静态分析(QQMusic 22.71,镜像基址 0x10000000)得到的链路:
//!   `+0x5DDD00` `__thiscall(this, a0, a1, a2)` 是命令分发器,里面是一条
//!   `wcscmp` 长链,认得 `seekto / volumeto / forward / rewind / playmode /
//!   playUrl / startFrom / catplay / backToPlay / parsexmlcmdraw` 等命令名。
//!   命中 `seekto` 时调 `+0x5DF2F0`(`CQQMusicCommandLineInfo::HandleSeekTo`),
//!   它把值拼成 `/seekto '%s'` 后交给 `this+0x38` 上的 `+0x5E87B0` 去执行。
//!
//! 所以只要拿到分发器的 `this`,就能在进程内直接调 `HandleSeekTo`,
//! 走的是 QQ 音乐自己的 seek 正路,不依赖进度条像素。
//!
//! 全部功能只在 `%TEMP%\QQMusicInjectorLogs\cmd-probe.flag` 存在时开启。
//! `%TEMP%\QQMusicInjectorLogs\cmd-probe.txt` 里写 `seek:<字符串>` 就现场试一次。

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use minhook::MinHook;
use tracing::{info, warn};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
use windows::core::w;

/// `HandleSeekTo` 在本地执行那条路上发给主窗口的消息号。
const WINDOW_MESSAGE_SEEK: u32 = 0x272D;

/// 命令分发器 `__thiscall(this, a0, a1, a2)`,ret 0Ch。
const DISPATCHER_RVA: usize = 0x5DD_D00;

static DISPATCHER_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
/// `HandleSeekTo` 的入口地址,由 `internal` 按特征定位好,这里只借用。
static SEEK_HANDLER: AtomicUsize = AtomicUsize::new(0);
/// 分发器第一次被调用时抓到的 `this`,也就是那个命令行对象。
static COMMAND_OBJECT: AtomicUsize = AtomicUsize::new(0);

static FLAG_DIR: OnceLock<PathBuf> = OnceLock::new();

fn flag_dir() -> &'static PathBuf {
    FLAG_DIR.get_or_init(|| std::env::temp_dir().join("QQMusicInjectorLogs"))
}

fn flag_enabled() -> bool {
    flag_dir().join("cmd-probe.flag").exists()
}

/// 装分发器钩子并起一个指令文件监听线程。只在 flag 存在时干活。
pub(crate) unsafe fn install() {
    if !flag_enabled() {
        return;
    }
    let Some(module) = (unsafe { GetModuleHandleW(w!("QQMusic.dll")) }).ok() else {
        warn!("cmd-probe: 拿不到 QQMusic.dll");
        return;
    };
    let base = module.0 as usize;
    // internal 已经按特征解析过入口;万一没成功,用同一个 RVA 兜底。
    let handler = match crate::internal::handler_address() {
        0 => {
            warn!("cmd-probe: internal 没定位到 HandleSeekTo,改用写死的 RVA");
            base + crate::internal::FALLBACK_RVA
        }
        address => address,
    };
    SEEK_HANDLER.store(handler, Ordering::Relaxed);

    let target = (base + DISPATCHER_RVA) as *mut c_void;
    let Ok(original) = MinHook::create_hook(target, detour_dispatcher as *mut c_void) else {
        warn!("cmd-probe: 挂不上命令分发器");
        return;
    };
    DISPATCHER_ORIGINAL.store(original as usize, Ordering::Relaxed);
    if MinHook::enable_hook(target).is_err() {
        warn!("cmd-probe: 启用钩子失败");
        return;
    }
    info!(
        dispatcher = format_args!("QQMusic.dll+{DISPATCHER_RVA:#x}"),
        handler = format_args!("{:#x}", handler - base),
        "cmd-probe 已就绪,等命令分发器被调用"
    );
    std::thread::spawn(|| {
        loop {
            std::thread::sleep(Duration::from_millis(400));
            run_directive();
        }
    });
}

type DispatcherFn =
    unsafe extern "thiscall" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> i32;

unsafe extern "thiscall" fn detour_dispatcher(
    this: *mut c_void,
    a0: *mut c_void,
    a1: *mut c_void,
    a2: *mut c_void,
) -> i32 {
    let previous = COMMAND_OBJECT.swap(this as usize, Ordering::SeqCst);
    info!(
        this = format_args!("{this:p}"),
        first = previous == 0,
        gate_19c = unsafe { field(this as usize, 0x19C) },
        gate_1a0 = unsafe { field(this as usize, 0x1A0) },
        "命令分发器被调用"
    );
    describe_arg(b"a0", a0);
    describe_arg(b"a1", a1);
    describe_arg(b"a2", a2);
    if previous == 0 {
        describe_object(this);
    }
    let original = DISPATCHER_ORIGINAL.load(Ordering::Relaxed);
    if original == 0 {
        return 0;
    }
    let original: DispatcherFn = unsafe { std::mem::transmute(original) };
    unsafe { original(this, a0, a1, a2) }
}

/// 把某个参数当指针看:先按 `wchar_t*` 读,再按 `wchar_t**` 读一层。
unsafe fn describe_arg(label: &[u8; 2], value: *mut c_void) {
    let addr = value as usize;
    if addr == 0 {
        info!(arg = %String::from_utf8_lossy(label), "空指针");
        return;
    }
    let direct = read_wide(addr as *const u16);
    let deref = match crate::try_seh(|| unsafe {
        let inner = std::ptr::read_unaligned(addr as *const *const u16);
        if inner.is_null() {
            None
        } else {
            Some(read_wide(inner))
        }
    }) {
        Ok(v) => v,
        Err(_) => None,
    };
    info!(
        arg = %String::from_utf8_lossy(label),
        addr = format_args!("0x{addr:X}"),
        as_wstr = %direct.unwrap_or_default(),
        deref_wstr = %deref.flatten().unwrap_or_default(),
        dword0 = crate::try_seh(|| unsafe { std::ptr::read_unaligned(addr as *const u32) }).unwrap_or(0),
        "参数"
    );
}

unsafe fn read_wide(mut p: *const u16) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut chars = Vec::new();
    for _ in 0..96 {
        let c = crate::try_seh(|| unsafe { std::ptr::read_unaligned(p) }).ok()?;
        if c == 0 {
            break;
        }
        chars.push(c);
        p = unsafe { p.add(1) };
    }
    if chars.is_empty() {
        None
    } else {
        Some(String::from_utf16_lossy(&chars))
    }
}

/// 打分发器对象的关键字段:`+0x19C`/`+0x1A0` 是 HandleSeekTo 里的两道闸门。
unsafe fn describe_object(this: *mut c_void) {
    let base = this as usize;
    let mut fields = String::new();
    for offset in (0..0x40).step_by(4) {
        let value =
            crate::try_seh(|| unsafe { std::ptr::read_unaligned((base + offset) as *const u32) });
        match value {
            Ok(v) => fields.push_str(&format!("{offset:#x}={v:#x} ")),
            Err(_) => break,
        }
    }
    info!(
        head = %fields,
        gate_19c = unsafe { field(base, 0x19C) },
        gate_1a0 = unsafe { field(base, 0x1A0) },
        "命令行对象头部"
    );
}

unsafe fn field(base: usize, offset: usize) -> u32 {
    crate::try_seh(|| unsafe { std::ptr::read_unaligned((base + offset) as *const u32) })
        .unwrap_or(0)
}

/// 读 `%TEMP%\QQMusicInjectorLogs\cmd-probe.txt`,内容变了就执行一次。
unsafe fn run_directive() {
    let path = flag_dir().join("cmd-probe.txt");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let directive = text.trim().to_string();
    let previous = LAST_DIRECTIVE
        .lock()
        .map(|mut v| std::mem::replace(&mut *v, Some(directive.clone())))
        .unwrap_or(None);
    if directive.is_empty() || previous.as_deref() == Some(directive.as_str()) {
        return;
    }
    // 六种打法,用来分辨 QQ 音乐内部到底怎么消费 `/seekto` 的值:
    //   direct: 走生产代码那条路(`internal::seek_to`),用来在插件里自证直调可用
    //   seek:   HandleSeekTo(this, &p, 1) —— 它内部会 SendMessage(0x272D)
    //   blind:  同 seek,但 this 传空,验证本地执行那条路依不依赖命令行对象
    //   fwd:    HandleSeekTo(this, &p, 0) —— 回传给另一个实例那条路(受 this+0x19C 闸门)
    //   msg:    直接 SendMessage(主窗口, 0x272D, 0, &p),lParam 是"指向字符串指针的指针"
    //   msgs:   同上,但 lParam 直接就是字符串本身
    let head = match directive.split_once(':') {
        Some((head, _)) => head.to_ascii_lowercase(),
        None => {
            if directive == "peek" {
                let this = COMMAND_OBJECT.load(Ordering::SeqCst);
                if this != 0 {
                    unsafe { describe_object(this as *mut c_void) };
                }
            }
            return;
        }
    };
    if !matches!(
        head.as_str(),
        "direct" | "seek" | "blind" | "fwd" | "msg" | "msgs"
    ) {
        return;
    }
    let value = directive[head.len() + 1..].to_string();
    let handler = SEEK_HANDLER.load(Ordering::Relaxed);
    if handler == 0 {
        return;
    }
    let this = match head.as_str() {
        "blind" => 0,
        _ => COMMAND_OBJECT.load(Ordering::SeqCst),
    };
    if this == 0 && matches!(head.as_str(), "seek" | "fwd") {
        warn!("还没抓到命令行对象:先跑一次 QQMusic.exe \"/seekto '1'\" 让分发器走一遍");
        return;
    }

    let before = crate::seek::read_song(|song| song.progress) as u64;
    let duration = crate::seek::read_song(|song| song.duration) as u64;
    if head == "direct" {
        let seconds: u64 = value.trim_matches(['\'', ' ']).parse().unwrap_or(0);
        let ok = crate::internal::seek_to(seconds * 1000);
        // 跳转是异步生效的,立刻读只会读到旧值。
        std::thread::sleep(Duration::from_millis(1_200));
        let landed = crate::seek::read_song(|song| song.progress) as u64;
        info!(value = %value, ok, before_ms = before, after_ms = landed,
              duration_ms = duration, "internal::seek_to 结果");
        return;
    }
    let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let mut pointer: *const u16 = wide.as_ptr();
    let lparam: *mut c_void = if head == "msgs" {
        pointer as *mut c_void
    } else {
        &raw mut pointer as *mut c_void
    };

    let outcome = match head.as_str() {
        "seek" | "blind" | "fwd" => {
            type HandlerFn =
                unsafe extern "thiscall" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;
            let handler: HandlerFn = unsafe { std::mem::transmute(handler) };
            let local = usize::from(head != "fwd");
            crate::try_seh(|| unsafe {
                handler(
                    this as *mut c_void,
                    &raw mut pointer as *mut c_void,
                    local as *mut c_void,
                )
            })
            .map(|code| format!("HandleSeekTo→{code}"))
        }
        _ => {
            let hwnd = main_window();
            if hwnd.is_none() {
                warn!("找不到主窗口,msg 类指令需要它");
                return;
            }
            let hwnd = hwnd.unwrap();
            crate::try_seh(|| unsafe {
                SendMessageW(
                    hwnd,
                    WINDOW_MESSAGE_SEEK,
                    Some(WPARAM(0)),
                    Some(LPARAM(lparam as isize)),
                )
            })
            .map(|result| format!("SendMessage(0x{WINDOW_MESSAGE_SEEK:X})→{}", result.0))
        }
    };
    match outcome {
        Ok(note) => info!(value = %value, via = %head, note = %note, "指令已执行"),
        Err(exception) => {
            warn!(value = %value, via = %head, exception = format_args!("0x{exception:X}"), "指令抛异常");
            return;
        }
    }

    // 观察 3 秒,看进度是否被这条命令挪动。
    let mut samples = String::new();
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(150));
        samples.push_str(&format!(
            "{} ",
            crate::seek::read_song(|song| song.progress) as u64 / 100
        ));
    }
    let last = crate::seek::read_song(|song| song.progress) as u64;
    info!(
        value = %value,
        before_ms = before,
        after_ms = last,
        duration_ms = duration,
        samples_ds = %samples,
        "命令行 seek 观察结果(单位 0.1s)"
    );
}

static LAST_DIRECTIVE: Mutex<Option<String>> = Mutex::new(None);

/// 主窗口句柄只查一次并缓存,`msg:` 类指令每条都要用。
fn main_window() -> Option<HWND> {
    static CACHED: AtomicUsize = AtomicUsize::new(0);
    let cached = CACHED.load(Ordering::Relaxed);
    if cached != 0 {
        return Some(HWND(cached as *mut c_void));
    }
    let hwnd = unsafe { crate::find_main_window() };
    if hwnd.0.is_null() {
        return None;
    }
    let _ = CACHED.compare_exchange(0, hwnd.0 as usize, Ordering::Relaxed, Ordering::Relaxed);
    Some(hwnd)
}
