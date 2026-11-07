//! 进程识别模块（Phase 3 P0-1）
//!
//! 职责：由窗口句柄（HWND）反查它所属进程的 **exe 绝对路径**，
//! 为后续「是否受保护」的判定提供依据。
//!
//! 对应 DESIGN_AND_ROADMAP.md §2.2 的识别链路前两步：
//! `HWND → PID → 完整路径`。签名与 Hash 校验留到 Phase 4。

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

/// 由窗口句柄拿到其所属进程的 PID。
///
/// # 重要陷阱
/// `GetWindowThreadProcessId` 的**返回值是「线程 ID」**，进程 ID 必须通过
/// 第二个参数（`lpdwProcessId`，即 out 指针）取回。
/// 早期版本误把返回值当 PID 用，导致后续 `OpenProcess` 报「参数错误 (0x80070057)」。
pub fn pid_from_hwnd(hwnd: HWND) -> u32 {
    let mut pid: u32 = 0;
    unsafe {
        // 返回值（线程 ID）我们不需要，只关心 out 参数写回的进程 ID。
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    pid
}

/// 由 PID 拿到进程 exe 的**完整绝对路径**（例如 `C:\Windows\System32\notepad.exe`）。
///
/// # 关键点
/// - 使用 `PROCESS_QUERY_LIMITED_INFORMATION` 权限而非 `PROCESS_ALL_ACCESS`：
///   权限最小化，**普通用户权限即可打开大多数进程**，且不会引起杀软警惕。
/// - 高权限（管理员）进程在普通权限下可能打不开，此时返回 `None`，
///   上层可选择跳过或提示用户以管理员身份运行。
pub fn process_path(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }

    unsafe {
        // 1. 以最小权限打开进程（失败通常是因为目标是高权限进程，静默跳过即可）
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;

        // 2. 准备接收路径的缓冲区。
        //    【为什么不用 MAX_PATH(260)】260 只保证"传统短路径"。装在深层目录
        //    下的程序（如某些 IDE / 游戏的完整路径）很容易超过 260 字符，
        //    QueryFullProcessImageNameW 会写不下而失败 → 识别不出进程 →
        //    受保护程序直接漏锁。给到 1024 字符，覆盖所有正常路径
        //    （Windows 路径上限 32768，但 1024 已远超实际会遇到的情况）。
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;

        // 3. 查询完整路径（Win32 形式，非 DOS 设备形式）
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0), // 0 = Win32 路径格式
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );

        // 无论成功与否都要关闭句柄，避免句柄泄漏
        let _ = CloseHandle(handle);

        if result.is_err() {
            return None;
        }

        // 4. 按实际长度截断并转成 String
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// 从完整路径中取出小写的文件名（含扩展名），便于日志与匹配展示。
pub fn file_name_lower(path: &str) -> String {
    path.rsplit(['\\', '/'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase()
}

/// 便捷组合：由 HWND 一步拿到 (pid, 完整路径, 小写文件名)。
pub fn identify(hwnd: HWND) -> Option<(u32, String, String)> {
    let pid = pid_from_hwnd(hwnd);
    if pid == 0 {
        return None;
    }
    let path = process_path(pid)?;
    let name = file_name_lower(&path);
    Some((pid, path, name))
}

// ============================================================================
// 跨进程命令行读取（代启动请求的身份绑定校验用）
// ============================================================================

/// 进程句柄的 RAII 守卫：离开作用域自动 CloseHandle。
/// 【为什么需要】command_line 内部有多个提前 return 的分支，
/// 手动逐个关句柄容易漏，RAII 保证任何退出路径都不泄漏。
struct HandleGuard(windows::Win32::Foundation::HANDLE);
impl Drop for HandleGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// 读取目标进程的**完整命令行**（守门进程身份校验用）。
///
/// # 为什么要读命令行
/// 「代启动」本质上是一个提权原语（见 main.rs handle_relay）：谁能让常驻实例
/// 代启动程序，谁就能以管理员权限运行 exe。仅凭"发送者进程是 hellogate.exe"
/// 不够——攻击者可以把自己的窗口句柄冒充成真正守门进程的句柄塞进 wParam
/// （WM_COPYDATA 的 wParam 系统不校验，是发送方自己填的）。
/// 而真正的守门进程有一条**无法伪造**的特征：它的命令行是系统经 IFEO 拼出来的，
/// 形如 `"<hellogate路径>" --gate "<目标程序>" <原参数>`。常驻实例读出这条命令行，
/// 核对其中确实包含本次请求的目标路径，就能确认"发送者正是拦截了这次启动的
/// 那个守门进程"，而不是别的进程冒名顶替。
///
/// # 实现方式（仅 x64）
/// Windows 没有公开"读别的进程命令行"的官方 API，标准做法是走 ntdll 的
/// `NtQueryInformationProcess`：拿到 PEB 地址 → 读 ProcessParameters 指针 →
/// 读其中的 UNICODE_STRING CommandLine。跨进程读内存用 `ReadProcessMemory`，
/// 需要 PROCESS_VM_READ 权限（常驻实例以管理员运行，读普通权限的守门进程
/// 没有问题）。偏移量（PEB+0x20、参数块+0x70）是 x64 下的固定布局。
///
/// # 返回
/// - 成功：完整命令行原文。
/// - 失败 / 非 x64 平台：None。**调用方必须把 None 当作"校验不过"处理**
///   （fail-closed：宁可多拦一次，不可漏放一次）。
#[cfg(target_arch = "x86_64")]
pub fn command_line(pid: u32) -> Option<String> {
    use windows::core::{s, w};
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, GetModuleHandleW};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };

    unsafe {
        // 1. 打开进程：查询信息 + 读内存。打不开（权限异常等）直接失败。
        let handle = OpenProcess(
            PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
            false,
            pid,
        )
        .ok()?;
        let _guard = HandleGuard(handle); // RAII：任何分支退出都自动关句柄

        // 2. 动态解析 NtQueryInformationProcess。
        //    ntdll 没被 windows crate 封装，只能 GetProcAddress 运行时取。
        //    返回值是"无参函数指针"（FARPROC），必须转形成真实签名才能调。
        //    （w! 宏直接产生 PCWSTR；ntdll.dll 必然存在，不需要判空分支。）
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        let proc_addr = GetProcAddress(ntdll, s!("NtQueryInformationProcess"))?;
        type NtQueryInformationProcessFn = unsafe extern "system" fn(
            isize,                        // 进程句柄
            u32,                          // 信息类别（0 = PROCESS_BASIC_INFORMATION）
            *mut core::ffi::c_void,       // 接收缓冲区
            u32,                          // 缓冲区大小
            *mut u32,                     // 实际写入长度
        ) -> i32;                          // NTSTATUS，0 = 成功
        let nt_query: NtQueryInformationProcessFn = core::mem::transmute(proc_addr);

        // 3. 查询 PROCESS_BASIC_INFORMATION（x64 下固定 48 字节），
        //    其中偏移 +8 处就是 PEB（进程环境块）的地址。
        let mut pbi = [0u8; 48];
        let mut ret_len = 0u32;
        let st = nt_query(
            handle.0 as isize,
            0, // ProcessBasicInformation
            pbi.as_mut_ptr() as *mut core::ffi::c_void,
            pbi.len() as u32,
            &mut ret_len,
        );
        if st != 0 {
            return None;
        }
        let peb = u64::from_le_bytes(pbi[8..16].try_into().ok()?);
        if peb == 0 {
            return None;
        }

        // 4. 读内存的小工具：每次读一段，失败即整体失败（None）。
        //    （windows 0.62 里 ReadProcessMemory 返回 Result，用 is_ok 判断。
        //      闭包位于外层 unsafe 块内，无需再标 unsafe。）
        let read_mem = |addr: u64, buf: &mut [u8]| -> bool {
            ReadProcessMemory(
                handle,
                addr as *const core::ffi::c_void,
                buf.as_mut_ptr() as *mut core::ffi::c_void,
                buf.len(),
                None,
            )
            .is_ok()
        };

        // 5. PEB + 0x20 = ProcessParameters 指针（x64 固定偏移）。
        let mut tmp = [0u8; 8];
        if !read_mem(peb + 0x20, &mut tmp) {
            return None;
        }
        let params = u64::from_le_bytes(tmp);
        if params == 0 {
            return None;
        }

        // 6. ProcessParameters + 0x70 = CommandLine（UNICODE_STRING 结构）：
        //    前 2 字节是"字节数长度"，+8 处是字符串指针（UTF-16）。
        let mut us = [0u8; 16];
        if !read_mem(params + 0x70, &mut us) {
            return None;
        }
        let len = u16::from_le_bytes([us[0], us[1]]) as usize;
        let buf_ptr = u64::from_le_bytes(us[8..16].try_into().ok()?);
        // 合理性检查：长度非零、不超过命令行上限（32768 字符 × 2 字节），
        // 防止读到垃圾数据时分配超大内存。
        if len == 0 || buf_ptr == 0 || len > 32768 * 2 {
            return None;
        }

        // 7. 按长度读出 UTF-16 字节流，还原成 Rust 字符串。
        let mut raw = vec![0u8; len];
        if !read_mem(buf_ptr, &mut raw) {
            return None;
        }
        let wide: Vec<u16> = raw
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Some(String::from_utf16_lossy(&wide))
    }
}

/// 非 x64 平台：暂不支持读命令行，恒返回 None。
/// 调用方（handle_relay）把 None 当作校验不过——宁可不能代启动，也不留后门。
#[cfg(not(target_arch = "x86_64"))]
pub fn command_line(_pid: u32) -> Option<String> {
    None
}
