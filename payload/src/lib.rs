use minhook::MinHook;
use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use std::{panic, thread};
use tracing::{error, info, warn};
use tracing_appender::rolling::{RollingFileAppender, Rotation};

mod bar;
mod internal;
mod mem;
mod probe;
mod seek;
mod trace;
use windows::{
    Foundation::Uri,
    Media::{
        ISystemMediaTransportControlsDisplayUpdater_Vtbl, MusicDisplayProperties,
        SystemMediaTransportControls,
    },
    Storage::Streams::RandomAccessStreamReference,
    Win32::{
        Foundation::{E_FAIL, FALSE, HINSTANCE, HWND, LPARAM, MAX_PATH, TRUE},
        Graphics::Gdi::{BLENDFUNCTION, HDC},
        Storage::FileSystem::{
            CopyFileW, GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO,
            VerQueryValueW,
        },
        System::{
            LibraryLoader::{
                DisableThreadLibraryCalls, GetModuleFileNameW, GetModuleHandleW, GetProcAddress,
                LoadLibraryW,
            },
            Memory::{
                MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
                PAGE_GUARD, PAGE_READONLY, PAGE_READWRITE, VirtualQuery,
            },
            SystemInformation::GetSystemDirectoryW,
            SystemServices::DLL_PROCESS_ATTACH,
            Threading::GetCurrentProcessId,
            WinRT::{ISystemMediaTransportControlsInterop, RO_INIT_MULTITHREADED, RoInitialize},
        },
        UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, IsWindowVisible},
    },
    core::{BOOL, Error, HRESULT, HSTRING, IInspectable, Interface, PCSTR, Result, factory, w},
};

static LOG_GUARD: OnceLock<tracing_appender::non_blocking::WorkerGuard> = OnceLock::new();

fn init_logging(prefix: &str) {
    let temp_dir = std::env::temp_dir().join("QQMusicInjectorLogs");
    let Ok(file_appender) = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(prefix)
        .filename_suffix("log")
        .max_log_files(7)
        .build(temp_dir)
    else {
        return; // 日志开不出来不影响主功能,别在 DllMain 里 panic
    };
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    let builder = tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_file(true)
        .with_line_number(true);
    // 同一个进程可能被装成两个模块,谁先注册谁生效。
    let _ = builder.try_init();
    let _ = LOG_GUARD.set(guard);
}

pub(crate) fn safe_call<F, T>(fallback: T, func: F) -> T
where
    F: FnOnce() -> T + panic::UnwindSafe,
{
    match panic::catch_unwind(func) {
        Ok(result) => result,
        Err(e) => {
            let message = e.downcast_ref::<&'static str>().map_or_else(
                || {
                    e.downcast_ref::<String>()
                        .map_or("未知类型的 Panic", |s| s.as_str())
                },
                |s| *s,
            );
            error!("一个 FFI 调用发生了 Panic: {message}");
            fallback
        }
    }
}

// https://github.com/Kxnrl/NetEase-Cloud-Music-DiscordRPC/blob/d3b77c679379aff1294cc83a285ad4f695376ad6/Vanessa/Players/Tencent.cs
/*
版本 20.43 ~ 21.93 结构
struct CurrentSongInfo {
    std::string m_szSong;              // 0x0
    std::string m_szArtist;            // 0x18
    std::string m_szAlbum;             // 0x30
    std::string m_szAlbumThumbnailUrl; // 0x48
    uint32_t m_nSongId;                // 0x60
private: char pad_64[0x4]; public:
    uint32_t m_nSongDuration;          // 0x68
    uint32_t m_nSongProgress;          // 0x6c
    uint32_t m_nPlayStatus;            // 0x70
}; // Size: 0x74
*/

// 如何更新Pattern:
// 1. 在 QQMusic.dll 里搜字符串 "Tencent Technology (Shenzhen) Company Limited", 并且找到引用的函数（理论上只有一个引用）
// 2. 引用到字符串的是一个初始化的函数，在引用到字符串的地方往上找类似这样的伪代码，理论上来讲这个伪代码会重复3次
//    byte_xxxxx = 0;
//    dword_xxxxx+0x10 = 0;
//    dword_xxxxx+0x14 = 15;
//    这个是初始化 std::string, 我们要找的是第一个，这个是当前播放的歌的名字
// 3. 选中然后在汇编页面生成Pattern
const PATTERN: &str =
    "A2 ?? ?? ?? ?? A3 ?? ?? ?? ?? C7 05 ?? ?? ?? ?? ?? ?? ?? ?? A2 ?? ?? ?? ?? A3";

#[repr(C)]
#[derive(Copy, Clone)]
union StringData {
    buf: [u8; 16],
    ptr: *const u8,
}

#[repr(C)]
struct MsStdString {
    data: StringData,
    len: usize,
    cap: usize,
}

impl std::fmt::Debug for MsStdString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.to_string_lossy())
    }
}

impl MsStdString {
    #[must_use]
    fn to_string_lossy(&self) -> String {
        unsafe {
            // 短字符串 (SSO) 优化
            let bytes = if self.cap < 16 {
                if self.len > 16 {
                    return String::from("<无效的字符串>");
                }
                &self.data.buf[..self.len]
            } else {
                if self.data.ptr.is_null() {
                    return String::from("<空指针>");
                }
                std::slice::from_raw_parts(self.data.ptr, self.len)
            };
            String::from_utf8_lossy(bytes).to_string()
        }
    }
}

#[repr(C)]
#[derive(Debug)]
pub(crate) struct CurrentSongInfo {
    name: MsStdString,                // 0x00
    artist: MsStdString,              // 0x18
    album: MsStdString,               // 0x30
    album_thumbnail_url: MsStdString, // 0x48
    id: u32,                          // 0x60
    _pad_64: [u8; 4],                 // 0x64 (padding)
    duration: u32,                    // 0x68
    progress: u32,                    // 0x6C
    play_status: u32,                 // 0x70
} // Size: 0x74

const _: () = {
    assert!(std::mem::offset_of!(CurrentSongInfo, id) == 0x60);
    assert!(std::mem::size_of::<MsStdString>() == 24);
};

unsafe fn is_target_process() -> bool {
    let mut filename = [0u16; MAX_PATH as usize];
    let len = unsafe { GetModuleFileNameW(None, &mut filename) };
    if len == 0 {
        return false;
    }
    let full_path = String::from_utf16_lossy(&filename[..len as usize]);
    let path = std::path::Path::new(&full_path);
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("QQMusic.exe"))
}

struct SysFuncs {
    alpha_blend: Option<AlphaBlendFn>,
    transparent_blt: Option<TransparentBltFn>,
    gradient_fill: Option<GradientFillFn>,
}

struct AppState {
    song_struct_addr: AtomicUsize,
    original_update: AtomicPtr<c_void>,
    sys_funcs: OnceLock<SysFuncs>,
    // 上一次写入 SMTC 的歌曲 ID。
    // 避免每一次 Update() 都重新设置 Thumbnail。
    last_song_id: AtomicUsize,
}

impl AppState {
    const fn new() -> Self {
        Self {
            song_struct_addr: AtomicUsize::new(0),
            original_update: AtomicPtr::new(std::ptr::null_mut()),
            sys_funcs: OnceLock::new(),
            last_song_id: AtomicUsize::new(0),
        }
    }

    fn sys(&self) -> &SysFuncs {
        self.sys_funcs.get_or_init(|| unsafe { init_sys_funcs() })
    }
}

static STATE: AppState = AppState::new();

unsafe fn init_sys_funcs() -> SysFuncs {
    // 加载相同模块名时，windows 会返回相同的句柄，导致循环加载相同函数并因栈溢出而崩溃
    // 所以先复制一份到临时目录再加载
    let mut sys_dir_buf = [0u16; 260];
    let len = unsafe { GetSystemDirectoryW(Some(&mut sys_dir_buf)) } as usize;
    if len == 0 {
        error!("无法获取系统目录");
        return SysFuncs {
            alpha_blend: None,
            transparent_blt: None,
            gradient_fill: None,
        };
    }
    let sys_dir = String::from_utf16_lossy(&sys_dir_buf[..len]);
    let source_path = format!("{sys_dir}\\msimg32.dll");

    let pid = unsafe { GetCurrentProcessId() };
    let temp_dir = std::env::temp_dir();
    if let Ok(entries) = std::fs::read_dir(&temp_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name.starts_with("_msimg32_qq_")
                && std::path::Path::new(name)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("dll"))
            {
                if let Err(e) = std::fs::remove_file(&path) {
                    warn!(path = ?path, "清理旧的临时 DLL 失败: {e}");
                } else {
                    info!(path = ?path, "已清理旧的临时 DLL");
                }
            }
        }
    }

    let dest_path_buf = temp_dir.join(format!("_msimg32_qq_{pid}.dll"));
    let dest_path = dest_path_buf.to_string_lossy().to_string();
    let source_h = HSTRING::from(&source_path);
    let dest_h = HSTRING::from(&dest_path);
    if let Err(e) = unsafe { CopyFileW(&source_h, &dest_h, false) } {
        error!("复制系统 msimg32 失败: {e:?}");
    }

    let lib = match unsafe { LoadLibraryW(&dest_h) } {
        Ok(h) => h,
        Err(e) => {
            error!("无法加载系统 msimg32 副本: {e}");
            return SysFuncs {
                alpha_blend: None,
                transparent_blt: None,
                gradient_fill: None,
            };
        }
    };
    info!(path = %dest_path, "正在加载代理 DLL");

    let get_func = |name: &str| -> Option<usize> {
        let name_h = std::ffi::CString::new(name).ok()?;
        let addr = unsafe { GetProcAddress(lib, PCSTR(name_h.as_ptr().cast::<u8>())) };
        addr.map(|f| f as usize)
    };

    unsafe {
        SysFuncs {
            alpha_blend: get_func("AlphaBlend").map(|f| std::mem::transmute(f)),
            transparent_blt: get_func("TransparentBlt").map(|f| std::mem::transmute(f)),
            gradient_fill: get_func("GradientFill").map(|f| std::mem::transmute(f)),
        }
    }
}

#[unsafe(no_mangle)]
unsafe extern "system" fn DllMain(
    hinstance: HINSTANCE,
    reason: u32,
    _reserved: *const c_void,
) -> BOOL {
    safe_call(FALSE, || {
        if reason == DLL_PROCESS_ATTACH {
            unsafe {
                let _ = DisableThreadLibraryCalls(hinstance.into());
            };

            if !unsafe { is_target_process() } {
                return TRUE;
            }

            init_logging("payload");
            thread::spawn(|| unsafe { main_thread() });
        }
        TRUE
    })
}

unsafe fn main_thread() {
    unsafe {
        info!("Hook 已加载，等待初始化...");
        log_host_version();

        let _ = RoInitialize(RO_INIT_MULTITHREADED);

        thread::spawn(|| scan_for_address());
        // 挂住"提交播放位置"函数,用来捕获播放器对象并支持内部直调 seek。
        internal::install();
        // 逆向探针:抓命令行命令对象,试走 QQ 音乐自己的 /seekto 通道。flag 存在才装。
        probe::install();

        if let Err(e) = install_hook() {
            error!("Hook 安装失败: {e:?}");
        }
    }
}

pub(crate) unsafe fn find_main_window() -> HWND {
    unsafe extern "system" fn enum_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let current_pid = unsafe { GetCurrentProcessId() };
        let mut window_pid = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut window_pid)) };

        if window_pid == current_pid && unsafe { IsWindowVisible(hwnd).as_bool() } {
            let target_ptr = lparam.0 as *mut HWND;
            if target_ptr.is_null() {
                error!("target_ptr 指针为空");
            } else {
                unsafe { *target_ptr = hwnd };
            }
            return FALSE;
        }
        TRUE
    }

    let current_pid = unsafe { GetCurrentProcessId() };
    let start_time = Instant::now();
    let timeout = Duration::from_secs(15);
    info!(pid = current_pid, "开始查找主窗口");
    loop {
        let mut found_hwnd = HWND(std::ptr::null_mut());
        let _ =
            unsafe { EnumWindows(Some(enum_window_proc), LPARAM(&raw mut found_hwnd as isize)) };
        if !found_hwnd.0.is_null() {
            info!(hwnd = ?found_hwnd, "找到主窗口句柄");
            return found_hwnd;
        }
        if start_time.elapsed() > timeout {
            warn!("查找主窗口超时，初始化可能会失败");
            return HWND(std::ptr::null_mut());
        }
        thread::sleep(Duration::from_millis(500));
    }
}

unsafe fn install_hook() -> Result<()> {
    let interop: ISystemMediaTransportControlsInterop =
        factory::<SystemMediaTransportControls, ISystemMediaTransportControlsInterop>()?;
    let hwnd = unsafe { find_main_window() };
    let smtc: SystemMediaTransportControls = unsafe { interop.GetForWindow(hwnd) }?;

    // seek 需要 SMTC 本体,放在 DisplayUpdater hook 之前装,失败不影响原有功能。
    if let Err(e) = unsafe { seek::install(&smtc, hwnd) } {
        error!("seek 能力初始化失败,进度跳转不可用: {e}");
    }
    trace::start_if_flagged();

    let updater = smtc.DisplayUpdater()?;

    let updater_raw: IInspectable = updater.cast()?;
    let raw_ptr = updater_raw.as_raw();
    if raw_ptr.is_null() {
        error!("DisplayUpdater 原始指针为空");
        return Err(Error::from(E_FAIL));
    }

    let vtable_ptr =
        unsafe { *raw_ptr.cast::<*const ISystemMediaTransportControlsDisplayUpdater_Vtbl>() };
    if vtable_ptr.is_null() {
        error!("DisplayUpdater VTable 指针为空");
        return Err(Error::from(E_FAIL));
    }

    let update_method_addr = unsafe { (*vtable_ptr).Update } as *mut c_void;
    info!(addr = ?update_method_addr, "获取到 Update 方法地址");

    let target = update_method_addr;
    let detour = detour_update as *mut c_void;

    match unsafe { MinHook::create_hook(target, detour) } {
        Ok(original) => {
            STATE.original_update.store(original, Ordering::Relaxed);
            unsafe { MinHook::enable_hook(target).ok() };
            info!("MinHook 安装成功");
            Ok(())
        }
        Err(e) => {
            error!("MinHook 创建失败: {e:?}");
            Err(Error::from(E_FAIL))
        }
    }
}

unsafe extern "system" fn detour_update(this: *mut c_void) -> HRESULT {
    if this.is_null() {
        return E_FAIL;
    }

    let original_ptr = STATE.original_update.load(Ordering::Relaxed);
    if original_ptr.is_null() {
        return E_FAIL;
    }
    let original: unsafe extern "system" fn(*mut c_void) -> HRESULT =
        unsafe { std::mem::transmute(original_ptr) };

    // ------------------------------------------------------------
    // 1. 在 QQ Music 提交之前，先修改好我们需要的数据
    // ------------------------------------------------------------
    let modify_result = safe_call(Err(Error::from(E_FAIL)), || {
        let struct_addr = STATE.song_struct_addr.load(Ordering::Relaxed);
        if struct_addr == 0 {
            return Ok(());
        }

        // 分离读取逻辑：防止切歌瞬间字符串失效导致整个 try_seh 崩溃
        let id = match try_seh(|| unsafe { (*(struct_addr as *const CurrentSongInfo)).id }) {
            Ok(id) => id,
            Err(code) => {
                error!("读取 song.id 失败 (0x{code:X})，特征码可能已失效");
                STATE.song_struct_addr.store(0, Ordering::Relaxed);
                return Ok(());
            }
        };

        let name = try_seh(|| unsafe {
            (*(struct_addr as *const CurrentSongInfo))
                .name
                .to_string_lossy()
        })
        .unwrap_or_default();

        let cover_url = try_seh(|| unsafe {
            (*(struct_addr as *const CurrentSongInfo))
                .album_thumbnail_url
                .to_string_lossy()
        })
        .unwrap_or_default();

        // 本地音乐或未知状态
        if id == 0 {
            return Ok(());
        }

        // ----------------------------------------------------
        // 2. 修改 Genre
        // ----------------------------------------------------
        if let Some(props) = unsafe { get_music_properties_from_vtable(this) } {
            if let Ok(genres) = props.Genres() {
                let _ = genres.Clear();
                let formatted_id = format!("QQ-{id}");
                let _ = genres.Append(&HSTRING::from(&formatted_id));
                info!(
                    song.id = %formatted_id,
                    song.name = %name,
                    "写入流派字段"
                );
            }
        } else {
            warn!("无法从 VTable 获取 MusicDisplayProperties");
        }

        // ----------------------------------------------------
        // 3. 只有新歌曲才重新设置高清封面
        // ----------------------------------------------------
        let old_id = STATE.last_song_id.swap(id as usize, Ordering::Relaxed);
        if old_id != id as usize {
            if let Err(e) = unsafe { set_hd_thumbnail(this, &cover_url) } {
                warn!("设置高清封面失败: {e:?}");
            }
        }

        Ok(())
    });

    if let Err(e) = modify_result {
        warn!("修改 SMTC 信息失败: {e:?}");
    }

    // ------------------------------------------------------------
    // 4. 调用 QQ Music 原始 Update()
    // 此时我们的修改已经生效，QQ Music 会把我们的修改一起提交给系统
    // 只需要调用一次！完美避开 Windows 的节流机制
    // ------------------------------------------------------------
    unsafe { original(this) }
}

unsafe fn get_music_properties_from_vtable(this: *mut c_void) -> Option<MusicDisplayProperties> {
    if this.is_null() {
        error!("get_music_properties_from_vtable this 指针为空");
        return None;
    }

    let seh_result = try_seh(|| {
        let vtable_ptr_ptr = this.cast::<*const ISystemMediaTransportControlsDisplayUpdater_Vtbl>();
        let vtable_ptr = unsafe { *vtable_ptr_ptr };
        if vtable_ptr.is_null() {
            error!("Update 对象 VTable 指针为空");
            return Err(E_FAIL);
        }

        let mut result_ptr: *mut c_void = std::ptr::null_mut();
        let hr = unsafe { ((*vtable_ptr).MusicProperties)(this, &raw mut result_ptr) };
        Ok((hr, result_ptr))
    });

    match seh_result {
        Ok(Ok((hr, result_ptr))) => match hr.ok() {
            Ok(()) if !result_ptr.is_null() => {
                Some(unsafe { MusicDisplayProperties::from_raw(result_ptr) })
            }
            Ok(()) => {
                warn!("MusicProperties 方法调用成功，但返回了空对象");
                None
            }
            Err(e) => {
                warn!("调用 MusicProperties 方法失败: {e}");
                None
            }
        },
        Ok(Err(_)) => {
            // vtable_ptr 是空指针
            None
        }
        Err(code) => {
            warn!("调用 MusicProperties 时捕获到异常 (0x{code:X})");
            None
        }
    }
}

unsafe fn set_hd_thumbnail(this: *mut c_void, original_url: &str) -> Result<()> {
    if this.is_null() || original_url.is_empty() {
        return Ok(());
    }

    let hd_url = match build_hd_cover_url(original_url) {
        Some(url) => url,
        None => {
            warn!(
                original_url = %original_url,
                "无法从封面 URL 提取 Album MID"
            );
            return Ok(());
        }
    };

    info!(
        hd_url = %hd_url,
        "设置 1500x1500 高清封面"
    );

    let uri = Uri::CreateUri(&HSTRING::from(&hd_url))?;
    let stream_ref = RandomAccessStreamReference::CreateFromUri(&uri)?;

    let updater =
        match windows::Media::SystemMediaTransportControlsDisplayUpdater::from_raw_borrowed(&this) {
            Some(v) => v,
            None => {
                return Err(Error::from(E_FAIL));
            }
        };

    updater.SetThumbnail(&stream_ref)?;
    Ok(())
}

fn build_hd_cover_url(original_url: &str) -> Option<String> {
    // 从：
    // T002R500x500M0000009NU2K0oNnlt_2.jpg
    //
    // 提取：
    // M0000009NU2K0oNnlt
    let marker = "M000";
    let start = original_url.find(marker)?;
    let rest = &original_url[start..];
    let end = rest.find(".jpg")?;
    let mid_part = &rest[..end];

    // 去掉可能存在的 _1 / _2
    let mid = mid_part.split('_').next().unwrap_or(mid_part);

    Some(format!(
        "https://y.qq.com/music/photo_new/T002R1500x1500{mid}.jpg?max_age=2592000"
    ))
}

unsafe fn scan_for_address() {
    let module_handle = unsafe { GetModuleHandleW(w!("QQMusic.dll")) };
    if let Ok(handle) = module_handle {
        let base_addr = handle.0 as usize;

        info!(
            base_addr = format_args!("0x{base_addr:X}"),
            "找到 QQMusic.dll 模块基址"
        );

        let scan_limit = base_addr + 20 * 1024 * 1024;
        info!("开始扫描内存特征码...");

        let pattern_bytes = parse_pattern(PATTERN);
        let start_time = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(60);

        loop {
            if start_time.elapsed() > timeout {
                warn!(timeout = ?timeout, "扫描超时，未找到特征码，插件功能将不可用");
                break;
            }

            let mut current_addr = base_addr;
            let mut found = false;

            while current_addr < scan_limit {
                let mut mbi = MEMORY_BASIC_INFORMATION::default();
                let query_size = unsafe {
                    VirtualQuery(
                        Some(current_addr as *const c_void),
                        &raw mut mbi,
                        std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
                    )
                };
                if query_size == 0 {
                    break;
                }

                let next_addr = current_addr + mbi.RegionSize;
                let is_committed = mbi.State == MEM_COMMIT;
                let protect = mbi.Protect.0;
                let is_guard = (protect & PAGE_GUARD.0) != 0;
                let base_protect = protect & 0xFF;
                let is_readable = base_protect == PAGE_READONLY.0
                    || base_protect == PAGE_READWRITE.0
                    || base_protect == PAGE_EXECUTE_READ.0
                    || base_protect == PAGE_EXECUTE_READWRITE.0;

                if is_committed && !is_guard && is_readable {
                    let safe_end = next_addr.min(scan_limit);
                    let safe_len = safe_end.saturating_sub(current_addr);
                    if safe_len > 0 {
                        let slice = unsafe {
                            std::slice::from_raw_parts(current_addr as *const u8, safe_len)
                        };
                        if let Some(offset) = find_pattern_bytes(slice, &pattern_bytes) {
                            let pattern_addr = current_addr + offset;
                            let ptr_addr = (pattern_addr + 1) as *const u32;
                            let struct_addr =
                                unsafe { std::ptr::read_unaligned(ptr_addr) } as usize;

                            let duration = start_time.elapsed();
                            info!(
                                duration = ?duration,
                                pattern_addr = format_args!("0x{pattern_addr:X}"),
                                struct_addr = format_args!("0x{struct_addr:X}"),
                                "特征码定位成功"
                            );
                            STATE.song_struct_addr.store(struct_addr, Ordering::Relaxed);
                            found = true;
                            break;
                        }
                    }
                }
                current_addr = next_addr;
            }

            if found {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(1000));
        }
    } else {
        error!("无法获取 QQMusic.dll 句柄");
    }
}

fn parse_pattern(pattern: &str) -> Vec<Option<u8>> {
    pattern
        .split_whitespace()
        .map(|s| {
            if s == "?" || s == "??" {
                None
            } else {
                u8::from_str_radix(s, 16).ok()
            }
        })
        .collect()
}

fn find_pattern_bytes(data: &[u8], pattern: &[Option<u8>]) -> Option<usize> {
    data.windows(pattern.len()).position(|w| {
        w.iter()
            .zip(pattern)
            .all(|(b, p)| p.is_none_or(|x| *b == x))
    })
}

// 调试用
unsafe fn log_host_version() {
    let mut filename = [0u16; MAX_PATH as usize];
    let len =
        unsafe { windows::Win32::System::LibraryLoader::GetModuleFileNameW(None, &mut filename) };
    if len == 0 {
        warn!("无法获取当前进程文件名");
        return;
    }
    let path = windows::core::PCWSTR(filename.as_ptr());

    let size = unsafe { GetFileVersionInfoSizeW(path, None) };
    if size == 0 {
        warn!("无法获取文件版本信息大小");
        return;
    }

    let mut data = vec![0u8; size as usize];
    if unsafe { GetFileVersionInfoW(path, Some(0), size, data.as_mut_ptr().cast()) }.is_ok() {
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut len: u32 = 0;
        if unsafe { VerQueryValueW(data.as_ptr().cast(), w!("\\"), &raw mut ptr, &raw mut len) }
            .as_bool()
        {
            let info = unsafe { &*(ptr as *const VS_FIXEDFILEINFO) };
            let v1 = (info.dwFileVersionMS >> 16) & 0xFFFF;
            let v2 = info.dwFileVersionMS & 0xFFFF;
            let v3 = (info.dwFileVersionLS >> 16) & 0xFFFF;
            let v4 = info.dwFileVersionLS & 0xFFFF;

            info!(
                version = %format!("{v1}.{v2}.{v3}.{v4}"),
                "找到进程版本"
            );
        } else {
            warn!("无法查询版本值");
        }
    } else {
        warn!("无法读取文件版本信息");
    }
}

////////////////////////////////////////////////////////////////////////////////////////////////////

unsafe extern "C" {
    unsafe fn ExecuteWithSEH(
        callback: unsafe extern "C" fn(*mut c_void),
        context: *mut c_void,
    ) -> u32;
}

unsafe extern "C" fn trampoline<F>(context: *mut c_void)
where
    F: FnMut(),
{
    let closure = unsafe { &mut *context.cast::<F>() };
    closure();
}

fn try_seh<F, R>(mut func: F) -> std::result::Result<R, u32>
where
    F: FnMut() -> R,
{
    unsafe fn run_seh_impl<W: FnMut()>(mut wrapper: W) -> std::result::Result<(), u32> {
        let context = std::ptr::addr_of_mut!(wrapper).cast::<c_void>();
        let code = unsafe { ExecuteWithSEH(trampoline::<W>, context) };
        if code == 0 { Ok(()) } else { Err(code) }
    }

    let mut result: Option<R> = None;
    let wrapper = || {
        result = Some(func());
    };
    unsafe { run_seh_impl(wrapper).map(|()| result.unwrap()) }
}

////////////////////////////////////////////////////////////////////////////////////////////////////

type AlphaBlendFn = unsafe extern "system" fn(
    HDC,
    i32,
    i32,
    i32,
    i32,
    HDC,
    i32,
    i32,
    i32,
    i32,
    BLENDFUNCTION,
) -> BOOL;

type TransparentBltFn =
    unsafe extern "system" fn(HDC, i32, i32, i32, i32, HDC, i32, i32, i32, i32, u32) -> BOOL;

// 用 *const c_void 代替指针参数，反正只是透传
type GradientFillFn =
    unsafe extern "system" fn(HDC, *const c_void, u32, *const c_void, u32, u32) -> BOOL;

#[unsafe(no_mangle)]
#[allow(
    clippy::missing_safety_doc,
    reason = "我们不会主动调用这个函数, 不需要文档"
)]
pub unsafe extern "system" fn AlphaBlend(
    hdcdest: HDC,
    xorigin: i32,
    yorigin: i32,
    wdest: i32,
    hdest: i32,
    hdcsrc: HDC,
    xsrc: i32,
    ysrc: i32,
    wsrc: i32,
    hsrc: i32,
    ftn: BLENDFUNCTION,
) -> BOOL {
    safe_call(FALSE, || unsafe {
        STATE.sys().alpha_blend.map_or(FALSE, |func| {
            func(
                hdcdest, xorigin, yorigin, wdest, hdest, hdcsrc, xsrc, ysrc, wsrc, hsrc, ftn,
            )
        })
    })
}

#[unsafe(no_mangle)]
#[allow(
    clippy::missing_safety_doc,
    reason = "我们不会主动调用这个函数, 不需要文档"
)]
pub unsafe extern "system" fn TransparentBlt(
    hdcdest: HDC,
    xorigin: i32,
    yorigin: i32,
    wdest: i32,
    hdest: i32,
    hdcsrc: HDC,
    xsrc: i32,
    ysrc: i32,
    wsrc: i32,
    hsrc: i32,
    crtransparent: u32,
) -> BOOL {
    safe_call(FALSE, || unsafe {
        STATE.sys().transparent_blt.map_or(FALSE, |func| {
            func(
                hdcdest,
                xorigin,
                yorigin,
                wdest,
                hdest,
                hdcsrc,
                xsrc,
                ysrc,
                wsrc,
                hsrc,
                crtransparent,
            )
        })
    })
}

#[unsafe(no_mangle)]
#[allow(
    clippy::missing_safety_doc,
    reason = "我们不会主动调用这个函数, 不需要文档"
)]
pub unsafe extern "system" fn GradientFill(
    hdc: HDC,
    pvertex: *const c_void,
    nvertex: u32,
    pmesh: *const c_void,
    nmesh: u32,
    ulmode: u32,
) -> BOOL {
    safe_call(FALSE, || unsafe {
        STATE.sys().gradient_fill.map_or(FALSE, |func| {
            func(hdc, pvertex, nvertex, pmesh, nmesh, ulmode)
        })
    })
}
