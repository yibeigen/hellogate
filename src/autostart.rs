//! 开机自启模块（Phase 4）
//!
//! 职责：通过写入 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 实现开机自启。
//!
//! # 为什么用 HKCU 而不是 HKLM？
//! - `HKCU`（当前用户）**无需管理员权限**即可写入，符合「零提权、零杀软误报」的设计目标。
//! - `HKLM`（本机）虽然对所有用户生效，但需要管理员权限，会触发 UAC，体验差。
//! - 对应用锁工具而言，保护「当前用户的应用使用」用 HKCU 完全够用。

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};

/// 注册表 Run 键路径。
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");

/// 自启项的键名（会显示在任务管理器的「启动」选项卡里）。
const VALUE_NAME: PCWSTR = w!("HelloGate");

/// 判断当前是否已设置开机自启。
pub fn is_enabled() -> bool {
    unsafe {
        let mut hkey = HKEY::default();
        // 以只读方式打开 Run 键
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            Some(0),
            KEY_QUERY_VALUE,
            &mut hkey,
        )
        .is_err()
        {
            return false;
        }

        // 查询我们的键名是否存在
        let mut data_type = Default::default();
        let mut size = 0u32;
        let result = RegQueryValueExW(
            hkey,
            VALUE_NAME,
            None,
            Some(&mut data_type),
            None,
            Some(&mut size),
        );
        let _ = RegCloseKey(hkey);

        // 查询成功（键存在）即视为已启用
        result.is_ok()
    }
}

/// 启用或禁用开机自启。
///
/// # 参数
/// - `enable`：`true` 写入自启项，`false` 删除自启项。
pub fn set_enabled(enable: bool) -> Result<(), String> {
    unsafe {
        let mut hkey = HKEY::default();
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            Some(0),
            KEY_SET_VALUE,
            &mut hkey,
        )
        .ok()
        .map_err(|e| format!("打开 Run 注册表键失败: {e}"))?;

        let result = if enable {
            // 取当前 exe 的完整路径，作为启动命令
            let exe = std::env::current_exe()
                .map_err(|e| format!("获取当前 exe 路径失败: {e}"))?;
            let exe_str = exe.to_string_lossy().to_string();

            // 转成 UTF-16 并以 NUL 结尾（REG_SZ 需要）
            let wide: Vec<u16> = exe_str.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = std::slice::from_raw_parts(
                wide.as_ptr() as *const u8,
                wide.len() * std::mem::size_of::<u16>(),
            );

            let write_result = RegSetValueExW(hkey, VALUE_NAME, Some(0), REG_SZ, Some(bytes))
                .ok()
                .map_err(|e| format!("写入自启项失败: {e}"));

            // ★ 品牌改名清理：顺手删掉同一 Run 键下旧品牌名的自启值。
            // 【为什么】老版本用户在注册表里留的是 "WinGate" 值，指向旧 exe 路径；
            // 不清掉的话开机时新旧两个自启项都会拉起，出现"重复启动旧程序"的怪象。
            // 【失败直接忽略】多数用户根本没有这个旧值（新用户从未装过旧版），
            // RegDeleteValueW 返回 ERROR_FILE_NOT_FOUND 属正常情况，无需处理。
            // 这里的字面量 "WinGate" 是**故意保留**的旧值名，不是替换遗漏。
            let _ = RegDeleteValueW(hkey, w!("WinGate"));

            write_result
        } else {
            // 删除自启项。
            // 注意：windows 0.62 中 RegDeleteValueW 直接返回 WIN32_ERROR（不是 Result），
            // 因此要拿它和 ERROR_SUCCESS / ERROR_FILE_NOT_FOUND 比较，而不是 match Ok/Err。
            let status = RegDeleteValueW(hkey, VALUE_NAME);
            if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
                // 删除成功，或本来就不存在——都视为成功。
                Ok(())
            } else {
                Err(format!("删除自启项失败: {:?}", status))
            }
        };

        let _ = RegCloseKey(hkey);
        result
    }
}

// 保留 HSTRING 的显式引用，避免未使用导入告警。
#[allow(dead_code)]
fn _anchor(_: HSTRING) {}
