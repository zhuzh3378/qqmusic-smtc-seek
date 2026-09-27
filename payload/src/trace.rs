//! 一次性诊断:给进度字段下硬件写断点,把写入者定位出来。
//!
//! 静态搜 QQMusic.dll 走不通(结构体运行期全靠指针访问),所以改成动态:
//! 用 DR0 盯住 `CurrentSongInfo.progress`,配一个向量化异常处理器记录每次写入的
//! EIP 和调用栈。点击跳转和定时 tick 会留下不同的栈特征,一眼能分开。
//! 就是靠它量出"进度字段其实是大对象的 +0x1AC",进而顺藤摸到命令行命令那条正路。
//!
//! 只在 `%TEMP%\QQMusicInjectorLogs\trace.flag` 存在时启用,跑完自动解除并删掉标志文件。

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tracing::{info, warn};
use windows::Win32::Foundation::{CloseHandle, GetLastError};
use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, CONTEXT, CONTEXT_FLAGS, EXCEPTION_CONTINUE_EXECUTION,
    EXCEPTION_POINTERS, GetThreadContext, RemoveVectoredExceptionHandler, SetThreadContext,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, OpenThread, ResumeThread, SuspendThread,
    THREAD_GET_CONTEXT, THREAD_SET_CONTEXT, THREAD_SUSPEND_RESUME,
};

/// x86 监视 4 字节写入:L0 + 保留位(bit8/10/12/13) + R/W=写 + LEN=4。
/// 注意少了保留位的话,32 位 NtSetContextThread 会静默丢弃 DR7 却仍返回成功。
const DR7_WATCH_WRITE_DWORD: u32 = 0x000D_3501;
const CONTEXT_DEBUG_REGISTERS: CONTEXT_FLAGS = CONTEXT_FLAGS(0x0001_0010);
const EXCEPTION_SINGLE_STEP: i32 = 0x8000_0004_u32 as i32;
const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
const EFLAGS_RESUME: u32 = 0x0001_0000;

/// 每个写入点最多记几条,避免 tick 把日志刷满。
const MAX_EVENTS: u32 = 400;
const TRACE_SECONDS: u64 = 25;

static EVENTS: AtomicU32 = AtomicU32::new(0);
/// 已经装过断点的线程,避免反复挂起别人;新出现的线程要补装。
static ARMED_THREADS: Mutex<Vec<u32>> = Mutex::new(Vec::new());
static KNOWN_SITES: Mutex<Vec<(usize, u32)>> = Mutex::new(Vec::new());
static TARGET_ADDRESS: AtomicU32 = AtomicU32::new(0);

/// 标志文件存在就开一轮追踪。
pub(crate) fn start_if_flagged() {
    let Some(flag) = flag_path() else { return };
    if !flag.exists() {
        return;
    }
    let Some(address) = progress_address() else {
        warn!("还没定位到歌曲结构,硬件断点无法安装");
        return;
    };
    TARGET_ADDRESS.store(address as u32, Ordering::SeqCst);
    info!(
        address = format_args!("0x{address:X}"),
        "准备安装进度字段写断点"
    );

    std::thread::spawn(move || {
        let _ = crate::safe_call((), || unsafe { run_trace(address, &flag) });
    });
}

unsafe fn run_trace(address: usize, flag: &std::path::Path) {
    let cookie = AddVectoredExceptionHandler(1, Some(vectored_handler));
    if cookie.is_null() {
        warn!("AddVectoredExceptionHandler 失败");
        let _ = std::fs::remove_file(flag);
        return;
    }
    inspect_progress_owner(address);
    let armed = set_breakpoint_on_all_threads(address, DR7_WATCH_WRITE_DWORD, true);
    info!(threads = armed, "写断点已安装,请在窗口上拖动进度条");

    // 自测:让一个新线程往同一地址写回原值(语义上是 no-op,但确实是一次写入)。
    // 连它都不触发就说明插桩本身没生效,而不是应用清了调试寄存器。
    if !selftest_write(address) {
        let cleared = set_breakpoint_on_all_threads(0, 0, true);
        let _ = RemoveVectoredExceptionHandler(cookie);
        let _ = std::fs::remove_file(flag);
        info!(threads = cleared, "硬件断点自测未触发,插桩无效,提前解除");
        return;
    }

    // 硬件断点按线程生效,而播放线程可能是之后才创建的,所以要持续补装。
    let deadline = Instant::now() + Duration::from_secs(TRACE_SECONDS);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(700));
        set_breakpoint_on_all_threads(address, DR7_WATCH_WRITE_DWORD, false);
    }

    let cleared = set_breakpoint_on_all_threads(0, 0, true);
    let _ = RemoveVectoredExceptionHandler(cookie);
    let _ = std::fs::remove_file(flag);
    info!(
        events = EVENTS.load(Ordering::Relaxed),
        threads = cleared,
        "写断点已解除"
    );
}

unsafe extern "system" fn vectored_handler(info: *mut EXCEPTION_POINTERS) -> i32 {
    if info.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let record = unsafe { &*(*info).ExceptionRecord };
    if record.ExceptionCode.0 != EXCEPTION_SINGLE_STEP {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let context = unsafe { &mut *(*info).ContextRecord };
    // DR6 的 bit0 表示 DR0 命中。
    if context.Dr6 & 1 != 0 && EVENTS.fetch_add(1, Ordering::Relaxed) < MAX_EVENTS {
        report_site(context);
    }
    context.EFlags |= EFLAGS_RESUME;
    EXCEPTION_CONTINUE_EXECUTION
}

/// 记录写入点:EIP 的模块偏移、寄存器快照、以及 EBP 链上的几层返回地址。
unsafe fn report_site(context: &mut CONTEXT) {
    let eip = context.Eip as usize;
    let target = TARGET_ADDRESS.load(Ordering::Relaxed) as usize;
    let offset = if eip >= target { eip - target } else { 0 };

    let mut seen = match KNOWN_SITES.lock() {
        Ok(mut guard) => {
            if let Some(entry) = guard.iter_mut().find(|(site, _)| *site == eip) {
                entry.1 += 1;
                if entry.1 > 1 {
                    return; // 同一个点重复命中,只报第一次
                }
            } else {
                guard.push((eip, 1));
            }
            guard.len()
        }
        Err(_) => 0,
    };
    seen += 1;

    let mut frames = Vec::new();
    let mut ebp = context.Ebp as usize;
    for _ in 0..6 {
        let Some(return_address) = crate::mem::read_usize(ebp + 4) else {
            break;
        };
        frames.push(crate::mem::symbol_at(return_address));
        match crate::mem::read_usize(ebp) {
            Some(next) if next > ebp => ebp = next,
            _ => break,
        }
    }
    // 点击链路里 eax 是 owner 对象、esi 是持有 seek 回调的链表节点,都要记。
    let node = context.Esi as usize;
    let node_seek_slot = crate::mem::read_usize(node + OWNER_SEEK_SLOT_OFFSET)
        .map(crate::mem::symbol_at)
        .unwrap_or_default();
    info!(
        site = %crate::mem::symbol_at(eip),
        eax = format_args!("{:#x}", context.Eax),
        esi = format_args!("{:#x}", context.Esi),
        edi = format_args!("{:#x}", context.Edi),
        node_seek = %node_seek_slot,
        hits = seen,
        distance = offset,
        stack = %frames.join(" <- "),
        "进度字段被写入"
    );
}

/// 用一个新线程写回原值,给硬件断点一个确定该命中的样本。返回是否触发。
unsafe fn selftest_write(address: usize) -> bool {
    let before = EVENTS.load(Ordering::Relaxed);
    let writer = std::thread::spawn(move || {
        // 等这个线程自己被补装断点之后才写。
        std::thread::sleep(Duration::from_millis(700));
        let Ok(value) =
            crate::try_seh(|| unsafe { std::ptr::read_volatile(address as *const u32) })
        else {
            return;
        };
        // 写回同一个值:是一次真实写入,但不改变播放状态。
        let _ = crate::try_seh(|| unsafe { std::ptr::write_volatile(address as *mut u32, value) });
    });
    std::thread::sleep(Duration::from_millis(200));
    set_breakpoint_on_all_threads(address, DR7_WATCH_WRITE_DWORD, false);
    let _ = writer.join();
    std::thread::sleep(Duration::from_millis(400));
    let fired = EVENTS.load(Ordering::Relaxed) != before;
    info!(fired, "自测写入是否触发断点");
    fired
}

/// 进度字段其实是更大对象的 +0x1AC,而点击链路是先写这个字段、再调用对象 +0x134
/// 处的函数指针完成跳转。把这两个值读出来,就能直接调 seek,不用再点进度条。
const OWNER_PROGRESS_OFFSET: usize = 0x1AC;
const OWNER_SEEK_SLOT_OFFSET: usize = 0x134;

unsafe fn inspect_progress_owner(address: usize) {
    let Some(owner) = address.checked_sub(OWNER_PROGRESS_OFFSET) else {
        return;
    };
    let Some(seek_fn) = crate::mem::read_usize(owner + OWNER_SEEK_SLOT_OFFSET) else {
        warn!(owner = format_args!("0x{owner:X}"), "读不到 seek 函数指针");
        return;
    };
    info!(
        owner = format_args!("0x{owner:X}"),
        seek_slot = format_args!("0x{seek_fn:X}"),
        target = %crate::mem::symbol_at(seek_fn),
        "找到可直调的 seek 入口"
    );
    // 顺带把对象头几个槽位打出来,便于确认这不是巧合。
    for offset in [0x130usize, 0x134, 0x138, 0x140] {
        if let Some(value) = crate::mem::read_usize(owner + offset) {
            info!(offset = format_args!("+{offset:#x}"), value = %crate::mem::symbol_at(value), "对象槽位");
        }
    }
}

fn progress_address() -> Option<usize> {
    let base = crate::STATE.song_struct_addr.load(Ordering::Relaxed);
    if base == 0 {
        return None;
    }
    Some(base + std::mem::offset_of!(crate::CurrentSongInfo, progress))
}

fn flag_path() -> Option<std::path::PathBuf> {
    Some(
        std::env::temp_dir()
            .join("QQMusicInjectorLogs")
            .join("trace.flag"),
    )
}

/// 给进程内所有线程设置(或清零)DR0/DR7。返回成功的线程数。
unsafe fn set_breakpoint_on_all_threads(address: usize, dr7: u32, reset: bool) -> u32 {
    let pid = unsafe { GetCurrentProcessId() };
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }) else {
        warn!("CreateToolhelp32Snapshot 失败");
        return 0;
    };
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    if reset {
        if let Ok(mut guard) = ARMED_THREADS.lock() {
            guard.clear();
        }
    }
    let mut done = 0u32;
    if unsafe { Thread32First(snapshot, &mut entry) }.is_ok() {
        loop {
            let seen = ARMED_THREADS
                .lock()
                .is_ok_and(|guard| guard.contains(&entry.th32ThreadID));
            // 绝不能挂起自己:SuspendThread 会立刻把当前线程冻住,再也没人恢复它。
            if entry.th32OwnerProcessID == pid
                && entry.th32ThreadID != unsafe { GetCurrentThreadId() }
                && !seen
                && set_breakpoint_on_thread(entry.th32ThreadID, address, dr7)
            {
                if let Ok(mut guard) = ARMED_THREADS.lock() {
                    guard.push(entry.th32ThreadID);
                }
                done += 1;
            }
            if unsafe { Thread32Next(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }
    let _ = unsafe { CloseHandle(snapshot) };
    done
}

unsafe fn set_breakpoint_on_thread(thread_id: u32, address: usize, dr7: u32) -> bool {
    let Ok(handle) = (unsafe {
        OpenThread(
            THREAD_GET_CONTEXT | THREAD_SET_CONTEXT | THREAD_SUSPEND_RESUME,
            false,
            thread_id,
        )
    }) else {
        return false;
    };
    let suspended = unsafe { SuspendThread(handle) };
    let mut context: CONTEXT = unsafe { std::mem::zeroed() };
    context.ContextFlags = CONTEXT_DEBUG_REGISTERS;
    let mut outcome = String::new();
    if unsafe { GetThreadContext(handle, &mut context) }.is_err() {
        outcome = format!("GetThreadContext 失败 {:?}", unsafe { GetLastError() });
    } else {
        context.Dr0 = address as u32;
        context.Dr7 = dr7;
        // 清掉残留的命中状态,避免下一次异常被算到旧地址上。
        context.Dr6 = 0;
        if let Err(e) = unsafe { SetThreadContext(handle, &context) } {
            outcome = format!("SetThreadContext 失败 {e}");
        }
    }
    if suspended != u32::MAX {
        unsafe { ResumeThread(handle) };
    }
    let _ = unsafe { CloseHandle(handle) };
    if !outcome.is_empty() {
        warn!(thread = thread_id, reason = %outcome, "断点安装失败");
        return false;
    }
    true
}
