//! 进程内读内存的小工具:所有跨模块的指针跟踪都要先假定"这个地址可能不可读",
//! 所以统一走 SEH 保护,并且顺手把地址翻译成"模块名+偏移"方便对着二进制看。

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows::core::PCWSTR;

/// 读一个指针宽度。地址不可读时返回 None 而不是崩掉宿主。
pub(crate) fn read_usize(addr: usize) -> Option<usize> {
    crate::try_seh(|| unsafe { std::ptr::read_unaligned(addr as *const usize) }).ok()
}

/// 地址属于哪个模块,以及该模块的加载基址。
pub(crate) fn module_of(addr: usize) -> Option<(String, usize)> {
    let mut module = HMODULE(std::ptr::null_mut());
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(addr as *const u16),
            &raw mut module,
        )
    }
    .ok()?;
    let mut buffer = [0u16; 260];
    let len = unsafe { GetModuleFileNameW(Some(module), &mut buffer) } as usize;
    if len == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&buffer[..len]);
    let name = std::path::Path::new(&path)
        .file_name()
        .map_or_else(|| path.clone(), |n| n.to_string_lossy().into_owned());
    Some((name, module.0 as usize))
}

/// 把地址写成 `模块名+0x偏移`,认不出模块时退回裸地址。
pub(crate) fn symbol_at(addr: usize) -> String {
    match module_of(addr) {
        Some((name, base)) => format!("{name}+{:#x}", addr - base),
        None => format!("{addr:#x}"),
    }
}
