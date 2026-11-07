//! IFEO 启动拦截模块（「先验证、后运行」的注册表侧）
//!
//! # 作用
//! 让受保护程序在验证通过前**根本不启动**（对齐参考加壳器"解锁后运行"的体验），
//! 与遮罩互补：遮罩管"已经在运行的窗口"，IFEO 管"新启动的进程"。
//!
//! # 原理（Image File Execution Options）
//! Windows 启动任何 exe 前都会查 IFEO 键，若其下有 `Debugger` 值，
//! 就把"要启动的程序"换成 Debugger 指定的命令，并把**原始命令行追加在后面**。
//! 我们登记：
//!   Debugger = `"<hellogate路径>" --gate`
//! 于是双击 WINWORD.EXE 时，系统实际启动的是
//!   `hellogate.exe --gate "C:\...\WINWORD.EXE" <原参数...>`
//! 守门进程（见 gater.rs）验证通过后，请求提权常驻实例替用户启动真正的 Word
//! （参数原样传递，双击 .docx 文档照常带出文档）；验证不过则 Word 从头到尾没有运行过。
//!
//! # 为什么必须 HKLM（实测踩坑，勿回退 HKCU）
//! 2026-10-31 实测：HKCU 下的 IFEO Debugger 登记**系统根本不认**——登记存在、
//! 启动 WINWORD 却毫无拦截、守门日志一条不打。IFEO 的 Debugger 重定向只在
//! HKLM 生效，而写 HKLM 需要管理员权限，所以 HelloGate 以「运行时 UAC 自举 +
//! 提权常驻」的方式拿到写权限（见 main.rs 的提权自举）。
//!
//! # 为什么写两个视图（64 位 + WOW6432Node）
//! 注册表的 HKLM\SOFTWARE 有 64 位视图与 32 位视图两套：
//!   - 64 位程序启动时系统查主视图；
//!   - 32 位程序启动时系统查 WOW6432Node 视图。
//! 只登记一边，另一边的程序就漏锁。所以每个受保护应用都**双视图各写一份**。
//!
//! # 死循环坑与破法
//! Debugger 重定向对所有启动方式生效——替用户启动 Word 时同样会被再次拦截。
//! 破法：常驻实例（有管理员权限）替启动前先把该应用的 Debugger 值临时删除，
//! 启动立刻恢复（窗口只有毫秒级）。万一删除后崩溃，常驻实例每次启动都会
//! `sync()` 重建登记，保护不会永久丢失。
//! 注意：删/恢复只能发生在**提权常驻实例**里，守门进程（普通权限）没有
//! HKLM 写权限，它验证通过后只发「代启动请求」（见 gater.rs）。

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegEnumKeyW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_SET_VALUE,
    REG_OPEN_CREATE_OPTIONS, REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE,
};

use crate::config::Config;
use crate::logln;

/// 守门模式的命令行标志（main.rs 据此分派到 gater::run）。
pub const GATE_FLAG: &str = "--gate";

/// IFEO 根键：64 位视图（系统级约定，位置不能改）。
const IFEO_ROOT_64: &str =
    r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options";

/// IFEO 根键：32 位视图（WOW6432Node），32 位程序启动时系统查这里。
const IFEO_ROOT_32: &str =
    r"SOFTWARE\WOW6432Node\Microsoft\Windows NT\CurrentVersion\Image File Execution Options";

/// 所有要维护的根键（登记/删除/清理都遍历这两个视图）。
const ROOTS: [&str; 2] = [IFEO_ROOT_64, IFEO_ROOT_32];

/// 要写入/删除的值名：系统认的就是它。
const DEBUGGER: PCWSTR = w!("Debugger");

/// 按当前配置同步 IFEO 登记（HKLM 双视图）。
///
/// 调用时机：提权常驻实例启动后一次（修复"exe 被移动 / 登记被误删 / 代启动崩溃"），
/// 以及主控台每次保存配置后（新增应用立即生效、移除的应用自动清理）。
pub fn sync(cfg: &Config) -> Result<(), String> {
    // ★ 总开关关闭：删除本程序的**全部**启动拦截登记，然后什么都不登记。
    //
    // 【为什么这样实现"关掉验证"】IFEO 的拦截登记一旦写在注册表里，
    // 即使 HelloGate 主控台不再弹验证，双击程序照样会被系统转给 hellogate --gate。
    // 所以"关掉"必须等于"把登记清理干净"，让系统彻底不再把启动事件交给我们，
    // 用户看到的效果就是：双击程序直接打开，和没装 HelloGate 一模一样。
    //
    // 注意：只删 Debugger 值指向本程序的那部分（`debugger_points_here`），
    // 别家软件在 IFEO 里的登记绝不碰。
    if !cfg.guard_enabled {
        for root in ROOTS {
            cleanup_stale(root, &[]); // 空白名单 = 把所有属于我们的登记都算失效
        }
        logln!("[ifeo] 总开关已关闭：已清理全部启动拦截登记，程序将不再被拦截");
        return Ok(());
    }

    let dbg_value = debugger_command()?;

    // 1. 为每个启用的受保护应用登记 Debugger 重定向（64/32 两个视图各写一份）。
    let mut keep: Vec<String> = Vec::new();
    for app in cfg.apps.iter().filter(|a| a.enabled) {
        keep.push(app.file_name.to_lowercase());
        for root in ROOTS {
            let key_path = format!(r"{root}\{}", app.file_name);
            let hk = create_key(&key_path)?;
            let r = set_debugger(hk, &dbg_value);
            unsafe {
                let _ = RegCloseKey(hk);
            }
            r?;
        }
        logln!("[ifeo] 已登记启动拦截: {}", app.file_name);
    }

    // 2. 清理旧登记：扫描两个视图下所有子键，凡 Debugger 指向我们守门模式、
    //    但已不在当前保护列表里的（应用被移除/禁用、或改名后的残留），删掉值。
    for root in ROOTS {
        cleanup_stale(root, &keep);
    }
    Ok(())
}

/// 临时删除某应用的 Debugger 重定向（提权常驻实例代启动前调用，见模块头"死循环坑"）。
pub fn remove_for_app(exe_name_lower: &str) {
    for root in ROOTS {
        let key_path = format!(r"{root}\{exe_name_lower}");
        unsafe {
            let Ok(hk) = open_key(&key_path, KEY_SET_VALUE) else {
                continue; // 该视图下键本来就不存在，跳过
            };
            let _ = RegDeleteValueW(hk, DEBUGGER);
            let _ = RegCloseKey(hk);
        }
    }
}

/// 恢复（或重建）某应用的 Debugger 重定向（双视图）。
pub fn restore_for_app(exe_name_lower: &str) -> Result<(), String> {
    let dbg_value = debugger_command()?;
    for root in ROOTS {
        let key_path = format!(r"{root}\{exe_name_lower}");
        unsafe {
            let hk = create_key(&key_path)?;
            let r = set_debugger(hk, &dbg_value);
            let _ = RegCloseKey(hk);
            r?;
        }
    }
    Ok(())
}

/// 拼出 Debugger 的值：`"<hellogate.exe 完整路径>" --gate`。
/// 路径必须加引号——它可能含空格或中文（本项目当前路径就是）。
fn debugger_command() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("获取当前 exe 路径失败: {e}"))?;
    Ok(format!("\"{}\" {}", exe.display(), GATE_FLAG))
}

/// 创建（或打开已存在的）HKLM 子键，返回可写句柄。
/// 需要管理员权限——本模块的所有调用方都运行在提权常驻实例内。
fn create_key(subkey: &str) -> Result<HKEY, String> {
    unsafe {
        let mut hk = HKEY::default();
        let p = wide(subkey);
        let st = RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(p.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPEN_CREATE_OPTIONS(0),
            KEY_SET_VALUE,
            None,
            &mut hk,
            None,
        );
        if st == ERROR_SUCCESS {
            Ok(hk)
        } else {
            Err(format!("创建注册表键失败（{subkey}）: {st:?}（需要管理员权限）"))
        }
    }
}

/// 以指定权限打开已存在的 HKLM 子键。
fn open_key(subkey: &str, flags: REG_SAM_FLAGS) -> Result<HKEY, String> {
    unsafe {
        let mut hk = HKEY::default();
        let p = wide(subkey);
        let st = RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(p.as_ptr()), Some(0), flags, &mut hk);
        if st == ERROR_SUCCESS {
            Ok(hk)
        } else {
            Err(format!("打开注册表键失败（{subkey}）: {st:?}"))
        }
    }
}

/// 把 Debugger 值写成 REG_SZ。
fn set_debugger(hk: HKEY, value: &str) -> Result<(), String> {
    unsafe {
        let wide_val = wide(value);
        let bytes =
            std::slice::from_raw_parts(wide_val.as_ptr() as *const u8, wide_val.len() * 2);
        let st = RegSetValueExW(hk, DEBUGGER, Some(0), REG_SZ, Some(bytes));
        if st == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("写入 Debugger 值失败: {st:?}"))
        }
    }
}

/// 扫描指定视图并清理指向守门模式、但已不在保护列表里的旧登记。
fn cleanup_stale(root: &str, keep: &[String]) {
    // 读+写权限一起要：读是为了看 Debugger 内容，写是为了删除。
    let Ok(root_hk) = open_key(root, KEY_QUERY_VALUE | KEY_SET_VALUE) else {
        return; // IFEO 根键都打不开（几乎不可能），放弃清理即可
    };
    unsafe {
        let mut buf = [0u16; 256];
        let mut idx = 0u32;
        loop {
            let st = RegEnumKeyW(root_hk, idx, Some(&mut buf));
            if st != ERROR_SUCCESS {
                break; // 枚举完（ERROR_NO_MORE_ITEMS）或出错，统一退出
            }
            let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
            let name = String::from_utf16_lossy(&buf[..len]);
            idx += 1;
            if name.is_empty() || keep.contains(&name.to_lowercase()) {
                continue;
            }
            // 只看"是不是我们的登记"，别的应用在 IFEO 里的键（调试器配置等）绝不碰。
            let Ok(sub) = open_key(&format!(r"{root}\{name}"), KEY_QUERY_VALUE | KEY_SET_VALUE)
            else {
                continue;
            };
            if debugger_points_here(sub) {
                let del = RegDeleteValueW(sub, DEBUGGER);
                if del == ERROR_SUCCESS {
                    logln!("[ifeo] 已清理失效的启动拦截登记: {name}");
                }
            }
            let _ = RegCloseKey(sub);
        }
        let _ = RegCloseKey(root_hk);
    }
}

/// 判断某子键的 Debugger 值是否指向我们的守门模式（含 --gate 标志即认为是）。
fn debugger_points_here(hk: HKEY) -> bool {
    unsafe {
        // 两步查询：先问长度，再读内容（Win32 注册表标准套路）。
        let mut ty = REG_VALUE_TYPE::default();
        let mut size = 0u32;
        if RegQueryValueExW(hk, DEBUGGER, None, Some(&mut ty), None, Some(&mut size))
            != ERROR_SUCCESS
            || size == 0
        {
            return false;
        }
        let mut buf = vec![0u8; size as usize];
        if RegQueryValueExW(
            hk,
            DEBUGGER,
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr()),
            Some(&mut size),
        ) != ERROR_SUCCESS
        {
            return false;
        }
        let text =
            String::from_utf16_lossy(std::slice::from_raw_parts(buf.as_ptr() as *const u16, buf.len() / 2));
        text.contains(GATE_FLAG)
    }
}

/// Rust 字符串 → NUL 结尾 UTF-16（注册表 API 要 PCWSTR）。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
