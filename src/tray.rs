//! 系统托盘模块（Phase 4）
//!
//! 职责：在任务栏通知区显示图标，提供「打开主控台 / 重新锁定 / 退出」菜单。
//!
//! 对应 DESIGN_AND_ROADMAP.md §七 Roadmap 的「托盘自启」。
//!
//! # 实现要点
//! - `Shell_NotifyIconW`：向通知区增删改图标（`NIM_ADD` / `NIM_DELETE`）。
//! - 托盘图标不直接用 `CreateWindowExW` 建可见窗口，而是复用一个**消息专用窗口**
//!   （只收消息、从不显示），这是 Windows 托盘应用的标准做法。
//! - 图标资源：使用编进 exe 的品牌图标（build.rs winres 登记的 ID=1 图标组，
//!   由 tools\make_icon.ps1 生成，与界面顶栏手绘 Logo 同构）。

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};

use crate::logln;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, SetForegroundWindow,
    TrackPopupMenu, MF_SEPARATOR, MF_STRING, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP,
};

/// 托盘图标发出的回调消息 ID（自定义，只要不与系统消息冲突即可）。
/// 系统会把用户与托盘图标的交互（左键、右键等）通过这条消息发给我们。
///
/// 用 `WM_APP + 1` 表达而不是硬编码 `0x8000 + 1`：
/// `WM_APP`（0x8000）正是 Windows 预留给应用自定义消息的起点，
/// 这样写能一眼看出它属于「自定义消息区间」，也方便和其它自定义消息统一编号。
pub const WM_TRAY_ICON: u32 = WM_APP + 1;

/// 托盘菜单项命令 ID。
pub const CMD_OPEN_CONSOLE: usize = 1001;
pub const CMD_RELOCK_ALL: usize = 1002;
pub const CMD_EXIT: usize = 1003;
/// 「组件画廊（调试）」——沿用 1001 起递增的分配规律，1004 不与上面三项撞号。
pub const CMD_GALLERY: usize = 1004;

/// 右键菜单里「打开主控台」的文案。
const MENU_OPEN: &str = "打开主控台";
const MENU_RELOCK: &str = "重新锁定全部";
/// 调试入口：静态展示各控件各状态，方便目视核对视觉体系（Task 8 / 决策 D3）。
const MENU_GALLERY: &str = "组件画廊（调试）";
const MENU_EXIT: &str = "退出 HelloGate";
const TRAY_TIP: &str = "HelloGate - 应用锁";

/// 添加托盘图标。
///
/// # 参数
/// - `hwnd`：接收托盘消息的窗口句柄（通常是一个隐藏的消息窗口）。
pub fn add_icon(hwnd: HWND) -> Result<(), String> {
    unsafe {
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1; // 图标 ID，同一窗口可有多个图标
        // 指定我们关心的消息类型：
        // - NIF_MESSAGE：接收鼠标事件
        // - NIF_ICON：显示图标
        // - NIF_TIP：鼠标悬停显示提示文字
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY_ICON;

        // 自有品牌图标：从 exe 资源加载 ID=1 图标组（build.rs winres 编入，
        // 与主控台顶栏手绘 Logo 同构）。不再是 PoC 阶段的系统盾牌占位。
        // 资源缺失时 load_app_icon 内部回退系统默认图标并写日志，这里无需再判错。
        nid.hIcon = crate::ui::load_app_icon();

        // 提示文字：拷进定长数组（NOTIFYICONDATAW.szTip 是 [u16; 128]）
        let tip: Vec<u16> = TRAY_TIP.encode_utf16().collect();
        let n = tip.len().min(nid.szTip.len() - 1);
        nid.szTip[..n].copy_from_slice(&tip[..n]);

        // NIM_ADD：把图标加进通知区
        if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
            return Err("Shell_NotifyIconW(NIM_ADD) 失败".to_string());
        }
        Ok(())
    }
}

/// 移除托盘图标（退出时必须调用，否则图标会残留在通知区直到鼠标划过）。
pub fn remove_icon(hwnd: HWND) {
    unsafe {
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
}

/// 弹出一条气泡通知（Windows 10 / 11 会把它显示成右下角的横幅）。
///
/// # 为什么需要这个能力
/// 出厂默认密码状态下，解锁流程是**直接放行**的（见 password::password_unlock_flow）。
/// 也就是说此刻其实没有任何东西在拦——如果用户不知道这件事，他会误以为
/// HelloGate 已经开始保护他了。必须有一条"不打扰操作"的提示告诉他去设置密码。
///
/// 托盘气泡正好符合要求：
/// - 不抢前台焦点（不会打断用户正在做的事）；
/// - 几秒后自动消失（不需要用户点"确定"）；
/// - 点托盘图标就能顺手打开主控台去设置。
///
/// # 参数
/// - `hwnd`：托盘图标所属的窗口（必须和 `add_icon` 用的是同一个，否则系统认不出是哪个图标）；
/// - `title`：标题，系统最多显示 63 个字符；
/// - `text`：正文，系统最多显示 255 个字符；**这里绝不能写任何密码值**。
pub fn show_balloon(hwnd: HWND, title: &str, text: &str) {
    unsafe {
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1; // 必须与 add_icon 里的图标 ID 一致，否则系统找不到这个图标
        // NIM_MODIFY = 修改"已有图标"的状态；NIF_INFO = 本次修改附带一段气泡文本。
        // （不能用 NIM_ADD：图标已经加过了，再加一次会出现重复图标。）
        nid.uFlags = NIF_INFO;

        // 标题和正文都是定长宽字符数组，这里各留最后一位给字符串结尾的 0。
        let title_utf16: Vec<u16> = title.encode_utf16().collect();
        let n = title_utf16.len().min(nid.szInfoTitle.len() - 1);
        nid.szInfoTitle[..n].copy_from_slice(&title_utf16[..n]);

        let text_utf16: Vec<u16> = text.encode_utf16().collect();
        let n = text_utf16.len().min(nid.szInfo.len() - 1);
        nid.szInfo[..n].copy_from_slice(&text_utf16[..n]);

        // NIIF_INFO = 普通信息图标（蓝色 i）。这里不用"警告/错误"，
        // 因为默认密码状态并不算故障，只是"还没配置好"。
        nid.dwInfoFlags = NIIF_INFO;
        // 显示时长（毫秒）。系统对它有上下限约束，10 秒足够读完这两行字。
        nid.Anonymous.uTimeout = 10_000;

        if !Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
            // 常见原因：用户在系统设置里关掉了本应用的通知，或通知区正忙。
            // 提示发不出去不影响主流程，只记一行日志。
            logln!("[tray] 气泡通知发送失败（可能已被系统通知设置屏蔽）");
        }
    }
}

/// 在鼠标位置弹出右键菜单。
///
/// # 返回
/// - `Some(cmd)`：用户选择了某个菜单项（命令 ID）。
/// - `None`：用户点空白处取消了。
pub fn show_menu(hwnd: HWND) -> Option<usize> {
    unsafe {
        let menu = CreatePopupMenu().ok()?;

        // 把菜单项转成宽字符串（AppendMenuW 需要 NUL 结尾）
        let to_wide = |s: &str| -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        };
        let open = to_wide(MENU_OPEN);
        let relock = to_wide(MENU_RELOCK);
        let gallery = to_wide(MENU_GALLERY);
        let exit = to_wide(MENU_EXIT);

        // 逐项添加：文本项 + 分隔线 + 退出项
        let _ = AppendMenuW(menu, MF_STRING, CMD_OPEN_CONSOLE, PCWSTR(open.as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, CMD_RELOCK_ALL, PCWSTR(relock.as_ptr()));
        // 调试入口放在分隔线之前：它和「重新锁定」同属"功能项"，
        // 分隔线右侧只留「退出」这一项破坏性操作，职责分区更清楚。
        let _ = AppendMenuW(menu, MF_STRING, CMD_GALLERY, PCWSTR(gallery.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, CMD_EXIT, PCWSTR(exit.as_ptr()));

        // 取鼠标当前位置作为菜单位置
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);

        // 关键技巧：先 SetForegroundWindow，否则菜单弹出后点别处不会自动消失
        // （这是 Windows 托盘菜单的经典坑，官方文档明确要求这么做）。
        let _ = SetForegroundWindow(hwnd);

        // TPM_RIGHTBUTTON：响应右键；TPM_RETURNCMD：直接把选中的命令 ID 作为返回值。
        // 注意 windows 0.62 中 TrackPopupMenu 的 nReserved 参数是 Option<i32>，要传 Some(0)。
        let cmd = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_RETURNCMD, pt.x, pt.y, Some(0), hwnd, None);

        let _ = DestroyMenu(menu);

        // cmd 为 0 表示用户取消
        if cmd.0 == 0 {
            None
        } else {
            // TrackPopupMenu 返回 i32，而菜单命令 ID 是 usize，这里做一次安全转换。
            Some(cmd.0 as usize)
        }
    }
}

// 保留 GetModuleHandleW 的显式引用，避免未使用导入告警。
#[allow(dead_code)]
fn _anchor() {
    let _ = unsafe { GetModuleHandleW(None) };
}
