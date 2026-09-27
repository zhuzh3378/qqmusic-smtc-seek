//! 直接调用 QQ 音乐自己的 `/seekto` 命令处理函数,用它做进度跳转。
//!
//! 链路(QQMusic 22.71):
//!   `HandleSeekTo(this, wchar_t** 值, 标志)`,标志非 0 时它只做一件事:
//!   取主窗口对象再 `SendMessage(0x272D, 0, 值)`,全程不碰 `this`,
//!   所以实测 `this` 传空也照样生效。不需要抓任何对象、不依赖进度条几何。
//!   标志为 0 是另一条路(把命令回传给另一个实例),受闸门字段控制,别用。
//!
//! 值的单位是**整秒**(内部按 `_wtoi` 截断),超出时长会被当成 0,所以调用方要自己夹住。
//! 实测误差 0–211ms、窗口最小化也生效,比模拟点击(受进度条像素量化,约 1s/px)又准又稳。
//!
//! 函数入口不写死:先用 .rdata 里那条 `/seekto '%s'` 字面量(全镜像唯一)定位运行时地址,
//! 再找 `push <该地址>` 的那条指令,从它往前回找 MSVC 的 SEH 函数头。
//! 这样客户端改版只要字面量还在就能自己跟上;实在找不到就退回写死的 RVA,
//! 再不行调用会抛异常,由调用方转成点击兜底。

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

use tracing::{info, warn};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::core::w;

/// 22.71 实测 RVA,只在特征定位失败时兜底。
pub(crate) const FALLBACK_RVA: usize = 0x5DF_2F0;
/// 分发器认这个命令名,`HandleSeekTo` 会把值拼进这条格式串再往下传。
const COMMAND_FORMAT: &str = "/seekto '%s'";
/// MSVC `/GS` + SEH 的函数头:`push ebp; mov ebp,esp; push -1; push <handler>`。
const PROLOGUE: [u8; 6] = [0x55, 0x8B, 0xEC, 0x6A, 0xFF, 0x68];
/// 从 `push 字面量地址` 往前找函数头的最大距离。
const PROLOGUE_LOOKBACK: usize = 0x200;

static HANDLE_SEEKTO: AtomicUsize = AtomicUsize::new(0);

/// 解析 `HandleSeekTo` 的地址并记下来。找不到就放弃(点击路径仍然可用)。
pub(crate) fn install() {
    let module = match unsafe { GetModuleHandleW(w!("QQMusic.dll")) } {
        Ok(module) => module,
        Err(_) => {
            warn!("拿不到 QQMusic.dll 句柄,命令直调 seek 不可用");
            return;
        }
    };
    let base = module.0 as usize;
    let located = locate_handle_seekto(base);
    let address = located.unwrap_or(base + FALLBACK_RVA);
    HANDLE_SEEKTO.store(address, Ordering::Relaxed);
    info!(
        address = format_args!("{:#x}", address - base),
        by_signature = located.is_some(),
        "已定位 /seekto 命令处理函数"
    );
}

pub(crate) fn available() -> bool {
    HANDLE_SEEKTO.load(Ordering::Relaxed) != 0
}

/// 已解析出的函数入口,给 `probe` 的指令通道复用,免得两边各算一份。
pub(crate) fn handler_address() -> usize {
    HANDLE_SEEKTO.load(Ordering::Relaxed)
}

/// 按整秒跳转。返回是否成功执行(不代表一定到位,调用方要回读校验)。
pub(crate) fn seek_to(target_ms: u64) -> bool {
    let handler = HANDLE_SEEKTO.load(Ordering::Relaxed);
    if handler == 0 {
        return false;
    }
    let text = format!("{}", target_ms / 1000);
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let mut pointer: *const u16 = wide.as_ptr();

    type HandlerFn = unsafe extern "thiscall" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;
    let handler: HandlerFn = unsafe { std::mem::transmute(handler) };
    let outcome = crate::try_seh(|| unsafe {
        handler(
            std::ptr::null_mut(),
            &raw mut pointer as *mut c_void,
            1usize as *mut c_void,
        )
    });
    match outcome {
        Ok(_) => true,
        Err(code) => {
            warn!(command = %text, exception = format_args!("0x{code:X}"), "调用 HandleSeekTo 异常");
            false
        }
    }
}

fn locate_handle_seekto(base: usize) -> Option<usize> {
    let image = read_image(base)?;
    let literal_rva = find_bytes(&image, &utf16_bytes(COMMAND_FORMAT))?;
    // 镜像里 `push <字面量地址>` 只有一处,就是 HandleSeekTo 里那次格式化。
    let mut pattern = vec![0x68u8];
    pattern.extend(((base + literal_rva) as u32).to_le_bytes());
    let site = find_bytes(&image, &pattern)?;
    let back = (0..PROLOGUE_LOOKBACK).find(|back| {
        *back <= site && image[site - back..site - back + PROLOGUE.len()] == PROLOGUE
    })?;
    Some(base + site - back)
}

fn utf16_bytes(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(|c| c.to_le_bytes()).collect()
}

fn read_image(base: usize) -> Option<Vec<u8>> {
    let e_lfanew = u32_at(base + 0x3C)? as usize;
    // OptionalHeader 在 NT headers + 24,SizeOfImage 在 PE32 OptionalHeader + 56。
    let size_of_image = u32_at(base + e_lfanew + 24 + 56)? as usize;
    if !(0x1000..=256 * 1024 * 1024).contains(&size_of_image) {
        return None;
    }
    crate::try_seh(|| unsafe {
        std::slice::from_raw_parts(base as *const u8, size_of_image).to_vec()
    })
    .ok()
}

fn u32_at(address: usize) -> Option<u32> {
    crate::try_seh(|| unsafe { std::ptr::read_unaligned(address as *const u32) }).ok()
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
