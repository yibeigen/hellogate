//! Hook 监听模块（Phase 3 P0-1）
//!
//! 职责：使用 Win32 `SetWinEventHook` 监听**前台窗口切换**与**窗口创建**事件，
//! 并把事件投递到一个通道（mpsc），供上层状态机消费。
//!
//! 设计要点（对应 DESIGN_AND_ROADMAP.md §2.1）：
//! - 使用 `WINEVENT_OUTOFCONTEXT` 标志：钩子运行在本进程上下文，**不做 DLL 注入**，
//!   因此不会触发杀软的跨进程注入告警，也不影响目标进程稳定性。
//! - 事件驱动：没有事件时线程阻塞在消息循环上，CPU 占用接近 0。
//! - 回调函数必须是 `extern "system"` 的裸函数指针，且内部**不能做耗时操作**，
//!   所以我们只把 HWND 塞进通道，真正的识别/遮罩逻辑放到工作线程里做。

use std::sync::mpsc::Sender;
use std::sync::OnceLock;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, TranslateMessage, MSG,
};

/// 前台窗口切换事件（用户切换到另一个窗口时触发）
const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;

/// 钩子回调里产生的事件类型（供上层消费）。
///
/// 目前只关心「前台切换」。窗口创建事件（EVENT_OBJECT_CREATE）曾经也装了钩子，
/// 但上层逻辑只处理前台切换、创建事件全部丢弃，等于让系统每开一个窗口都回调一次、
/// 再往通道塞一条没人看的消息，白白消耗。故已移除，只保留前台切换钩子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinEventKind {
    /// 前台窗口发生了变化
    Foreground,
}

/// 一次原始事件：事件类型 + 目标窗口句柄。
///
/// # 为什么存 `isize` 而不是 `HWND`？
/// `HWND` 内部是 `*mut c_void`，**没有实现 `Send`**，无法跨线程通过 channel 传递。
/// 而窗口句柄本质上就是一个数值，跨线程传递是安全的（句柄在进程内全局有效）。
/// 所以这里存裸数值，使用时再还原成 `HWND`。
#[derive(Debug, Clone, Copy)]
pub struct RawEvent {
    pub kind: WinEventKind,
    pub hwnd: isize,
}

impl RawEvent {
    /// 把内部的数值还原成 `HWND`。
    pub fn to_hwnd(self) -> HWND {
        HWND(self.hwnd as *mut _)
    }
}

/// 全局事件发送端。
///
/// 【为什么用 OnceLock】`SetWinEventHook` 的回调是 C 风格的裸函数指针，
/// **不能捕获任何环境变量**，只能通过全局静态变量把「发送端」传给回调。
/// 钩子在进程生命周期内只安装一次、且安装先于一切回调，OnceLock
/// （一次性初始化、线程安全）刚好匹配——也比旧实现里的 `static mut` 更安全：
/// 后者属于不推荐写法（Rust 2024 起直接编译报错），这里顺手升级掉。
static EVENT_TX: OnceLock<Sender<RawEvent>> = OnceLock::new();

/// 钩子回调函数（由系统在事件发生时调用）。
///
/// # 安全
/// 该函数由 Windows 在任意线程上下文中调用（因为我们用了 OUTOFCONTEXT，
/// 实际会在安装钩子所在线程的消息循环里被派发）。函数体必须极快返回，
/// 否则会阻塞整个消息循环。
unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    // 只关心「窗口级」事件：id_object 必须为 0（OBJID_WINDOW），
    // id_child 必须为 0（CHILDID_SELF），否则是菜单/光标等无关子对象事件。
    if id_object != 0 || id_child != 0 {
        return;
    }
    // 无效窗口句柄直接忽略。
    if hwnd.is_invalid() {
        return;
    }

    let kind = match event {
        EVENT_SYSTEM_FOREGROUND => WinEventKind::Foreground,
        _ => return,
    };

    // 把事件塞进通道。send 失败（接收端已关闭）时静默忽略。
    // 注意：RawEvent.hwnd 是 isize（为了跨线程 Send），这里把 HWND 转成数值。
    if let Some(tx) = EVENT_TX.get() {
        let _ = tx.send(RawEvent {
            kind,
            hwnd: hwnd.0 as isize,
        });
    }
}

/// 安装钩子，返回一个「守卫」对象。
///
/// 守卫在 drop 时会自动 `UnhookWinEvent`，避免句柄泄漏。
pub struct HookGuard {
    foreground: HWINEVENTHOOK,
}

impl Drop for HookGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWinEvent(self.foreground);
        }
    }
}

/// 安装事件钩子，事件通过 `tx` 投递。
///
/// # 参数
/// - `tx`：事件发送端（上层持有对应的接收端 `Receiver`）。
///
/// # 返回
/// - `Ok(HookGuard)`：安装成功，守卫需长期持有（drop 即卸载）。
/// - `Err(String)`：安装失败（通常不会发生）。
pub fn install(tx: Sender<RawEvent>) -> Result<HookGuard, String> {
    // 先保存全局发送端，再安装钩子，保证回调触发时一定能取到。
    // （重复安装时 set 会失败——钩子本就只该装一次，忽略即可。）
    let _ = EVENT_TX.set(tx);

    unsafe {
        // 监听前台切换：从 EVENT_SYSTEM_FOREGROUND 到 EVENT_SYSTEM_FOREGROUND。
        let foreground = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,                       // 不使用外部 DLL
            Some(win_event_proc),       // 回调
            0,                          // 所有进程
            0,                          // 所有线程
            0,                          // dwFlags = 0 → 即 WINEVENT_OUTOFCONTEXT（值为 0）
        );

        if foreground.is_invalid() {
            return Err("SetWinEventHook 安装失败".to_string());
        }

        Ok(HookGuard { foreground })
    }
}

/// 运行消息循环（阻塞当前线程）。
///
/// `SetWinEventHook` 采用 OUTOFCONTEXT 时，事件回调是通过**消息队列**派发的，
/// 因此安装钩子的线程**必须持续泵消息**，否则回调永远不会被调用。
/// 这也是它「零 CPU 空转」的原因：没有消息时 `GetMessageW` 会让线程挂起。
pub fn run_message_loop() {
    unsafe {
        let mut msg = MSG::default();
        // GetMessageW 返回 0 表示收到 WM_QUIT；返回 -1 表示错误。
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
