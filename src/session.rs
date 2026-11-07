//! 会话/电源事件监听模块（Phase 4）
//!
//! 职责：监听 **锁屏**（`WM_WTSSESSION_CHANGE`）与 **睡眠/唤醒**（`WM_POWERBROADCAST`），
//! 在用户离开电脑回来后自动重锁所有已解锁的应用。
//!
//! 对应 DESIGN_AND_ROADMAP.md §4「息屏重锁」。
//!
//! # 为什么需要这个？
//! 宽限期是为了「同一使用时段内少弹窗」。但如果用户锁屏去开会，回来后
//! 宽限期仍然有效就失去了安全意义——所以必须在会话边界处重锁。
//!
//! # 实现方式
//! - `WTSRegisterSessionNotification`：注册后窗口会收到 `WM_WTSSESSION_CHANGE`，
//!   其中 `wparam == WTS_SESSION_LOCK (0x7)` 表示锁屏。
//! - `WM_POWERBROADCAST` + `PBT_APMSUSPEND (0x4)` 表示系统即将睡眠。

use std::sync::mpsc::Sender;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::RemoteDesktop::{WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION};

/// 会话事件类型（供上层消费）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEvent {
    /// 用户锁屏了。
    Locked,
    /// 用户解锁了屏幕。
    Unlocked,
    /// 系统即将进入睡眠。
    Suspend,
    /// 系统从睡眠中唤醒。
    Resume,
}

/// 处理会话/电源消息的入口。
///
/// # 参数
/// - `msg`：窗口消息 ID。
/// - `wparam`：消息的 wParam。
/// - `tx`：事件发送端。
///
/// # 返回
/// - `Some(LRESULT 值)` 表示已处理该消息；`None` 表示不是我们关心的消息。
///
/// # 为什么返回 Option 而不是直接处理？
/// 因为窗口过程需要知道「这条消息是否已被消费」，未消费时要交给 `DefWindowProcW`。
pub fn handle_message(msg: u32, wparam: WPARAM, tx: &Sender<SessionEvent>) -> Option<isize> {
    // WM_WTSSESSION_CHANGE = 0x02B1（会话状态变化）
    const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
    // WM_POWERBROADCAST = 0x0218（电源广播）
    const WM_POWERBROADCAST: u32 = 0x0218;

    // WTS 会话状态常量
    const WTS_SESSION_LOCK: usize = 0x7;
    const WTS_SESSION_UNLOCK: usize = 0x8;

    // 电源事件常量
    const PBT_APMSUSPEND: usize = 0x4;
    const PBT_APMRESUMEAUTOMATIC: usize = 0x12;

    match msg {
        WM_WTSSESSION_CHANGE => match wparam.0 {
            WTS_SESSION_LOCK => {
                let _ = tx.send(SessionEvent::Locked);
                Some(0)
            }
            WTS_SESSION_UNLOCK => {
                let _ = tx.send(SessionEvent::Unlocked);
                Some(0)
            }
            _ => None,
        },
        WM_POWERBROADCAST => match wparam.0 {
            PBT_APMSUSPEND => {
                let _ = tx.send(SessionEvent::Suspend);
                // 注意：PBT_APMSUSPEND 必须返回 TRUE(1) 才允许系统继续睡眠。
                Some(1)
            }
            PBT_APMRESUMEAUTOMATIC => {
                let _ = tx.send(SessionEvent::Resume);
                Some(1)
            }
            _ => None,
        },
        _ => None,
    }
}

/// 为指定窗口注册会话通知（使其能收到 `WM_WTSSESSION_CHANGE`）。
///
/// # 返回
/// - `Ok(())`：注册成功。
/// - `Err(String)`：注册失败。
pub fn register(hwnd: HWND) -> Result<(), String> {
    unsafe {
        // WTSRegisterSessionNotification 在 windows 0.62 中直接返回 Result<(), Error>，
        // 不需要先 .ok() 再 map_err，否则会变成对 Option 调用 map_err 而报错。
        WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION)
            .map_err(|e| format!("WTSRegisterSessionNotification 失败: {e}"))
    }
}

// 保留 LPARAM 的显式引用，避免未使用导入告警。
#[allow(dead_code)]
fn _anchor(_: LPARAM) {}
