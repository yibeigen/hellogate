//! 轻量 Toast 提示（UI 优化方案 A1）：给"无需决策的短反馈"用的非阻塞通知。
//!
//! # 为什么需要 Toast
//! 之前连"该应用已经在列表里"这种一句话提示都要弹一个系统 MessageBox——
//! 用户必须移动鼠标、点掉它才能继续，而这句话根本不值得一次点击确认。
//! 方案里把这类反馈单独归档成 Toast：深色胶囊、右下角浮出、3 秒自动消失，
//! 不抢焦点、不打断操作、不阻塞调用方。
//!
//! # 和 dialog.rs 的分工（判断标准：需不需要用户做决定）
//! | 场景 | 形态 | 原因 |
//! |------|------|------|
//! | 重复添加同一个应用 | **Toast** | 无需决策，瞄一眼即可 |
//! | 保存成功 | **Toast** | 同上 |
//! | 两次输入的主密码不一致 | dialog::warn | 要停下来改正，必须点掉 |
//! | 未保存的修改（是/否） | dialog::confirm | 必须做选择 |
//!
//! # 实现要点
//! - **非阻塞**：`show()` 只负责把窗口建出来就返回，动画由 `SetTimer`
//!   驱动窗口自己的消息回调完成——宿主（主控台/主窗口）的消息循环照常跑。
//! - **不发 WM_QUIT**：这是和 dialog.rs 最大的区别。模态弹窗用
//!   `PostQuitMessage` 结束自己的循环，但 Toast 是寄生在宿主循环里的，
//!   发 WM_QUIT 会把宿主的循环一起干掉（实打实的"关掉主控台"事故）。
//! - **淡入淡出**：窗口带 `WS_EX_LAYERED`，每个 tick 调一次
//!   `SetLayeredWindowAttributes` 改整体透明度，25ms 一档，
//!   6 档淡入（150ms）→ 停 3 秒 → 10 档淡出（250ms）→ 自毁。
//! - **同线程只留一条**：新 Toast 进来先把旧的销毁掉，避免叠罗汉。

use std::cell::{Cell, RefCell};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleDC, CreateFontW, DeleteDC, DeleteObject, DrawTextW, EndPaint,
    SelectObject, HFONT, PAINTSTRUCT, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DT_CALCRECT, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, FF_DONTCARE, FW_NORMAL, OUT_DEFAULT_PRECIS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowRect, IsWindow,
    KillTimer, LoadCursorW, RegisterClassW, SetLayeredWindowAttributes, SetTimer, SetWindowPos,
    SystemParametersInfoW, WNDCLASSW, WS_CLIPCHILDREN, WS_EX_LAYERED,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WM_CLOSE, WM_CREATE, WM_DESTROY, WM_ERASEBKGND,
    WM_PAINT, WM_TIMER, SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
// 配色与绘制原语：Toast 也走 ui.rs，颜色能跟着主题一起换
use crate::ui::{draw_text_left, fill_round, Palette};

// ============================================================================
// 常量
// ============================================================================

const TOAST_CLASS: &str = "HelloGateToastClass";
// 注意：timer id 在本仓库的 windows crate 版本里是 usize（和 password.rs 一致），
// 不是 Win32 头文件里的 UINT——照抄 password::PWD_TIMER_ID 的写法才编得过。
const TIMER_ID: usize = 1; // 动画定时器
const TICK_MS: u32 = 25; // 每 25ms 走一档
const FADE_IN: i32 = 6; // 6 档 = 150ms 淡入
const HOLD: i32 = 120; // 120 档 = 3000ms 停留
const FADE_OUT: i32 = 10; // 10 档 = 250ms 淡出
const MARGIN: i32 = 16; // 距屏幕/宿主右下角的间距（逻辑像素）

/// Toast 底色 #1F2937（深蓝灰，深色胶囊在浅色界面上"跳"得出来，又不像纯黑那么硬）。
/// COLORREF 是 0x00BBGGRR，所以 #1F2937 → 0x0037291F。
const TOAST_BG: u32 = 0x0037291F;
/// 胶囊上的文字（纯白）。
const TOAST_FG: u32 = 0x00FFFFFF;

thread_local! {
    /// 本线程当前 Toast 的 HWND 裸值（0 = 没有）。
    static CURRENT: Cell<isize> = const { Cell::new(0) };
    /// 动画走到第几档（见 FADE_IN / HOLD / FADE_OUT）。
    static TICK: Cell<i32> = const { Cell::new(0) };
    /// 当前窗口 DPI（96 = 100%）。
    static SCALE: Cell<i32> = const { Cell::new(96) };
    /// 窗口尺寸（物理像素），WM_CREATE 量好后给 show 摆位用。
    static VIEW: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
    /// 文字内容（WM_PAINT 要画）。
    static TEXT: RefCell<String> = RefCell::new(String::new());
    /// 字体裸句柄，WM_DESTROY 释放。
    static FONT: Cell<isize> = const { Cell::new(0) };
}

// ============================================================================
// 对外入口
// ============================================================================

/// 在 `owner` 窗口右下角浮出一条 Toast，**立即返回**（不阻塞调用方）。
///
/// - `owner = Some(hwnd)`：贴着该窗口的右下角（如主控台右下）；
/// - `owner = None`：贴屏幕工作区右下角（工作区已扣掉任务栏，不会被挡住）。
///
/// 寄生在宿主窗口所在**线程的消息循环**上：调用它的线程必须有循环在跑
/// （主循环 / 主控台循环都满足）。窗口 3 秒后自毁，不需要调用方收尾。
pub fn show(owner: Option<HWND>, text: &str) {
    // SAFETY：内部是建窗 / 定时器等 Win32 调用，参数都是普通值。
    unsafe { spawn(owner, text) };
}

/// 便捷入口：主控台保存成功时用（宿主消息窗口没有可用的矩形，
/// 传 None 会自动落到屏幕工作区右下角）。
pub fn save_ok() {
    show(None, "设置已保存，新的保护配置已生效");
}

// ============================================================================
// 实现
// ============================================================================

unsafe fn spawn(owner: Option<HWND>, text: &str) {
    // ---- 1. 同线程只保留一条 Toast：旧的还在就先销毁 ----
    let prev = CURRENT.with(|c| c.get());
    if prev != 0 && IsWindow(Some(HWND(prev as *mut _))).as_bool() {
        let _ = KillTimer(Some(HWND(prev as *mut _)), TIMER_ID);
        let _ = DestroyWindow(HWND(prev as *mut _));
    }
    CURRENT.with(|c| c.set(0));

    // ---- 2. 注册窗口类 ----
    // 背景画刷不设：胶囊和文字全由 WM_PAINT 画（和 dialog.rs 同一理由）。
    let class_name = wide(TOAST_CLASS);
    let Ok(hinstance) = GetModuleHandleW(None) else {
        return;
    };
    let wc = WNDCLASSW {
        lpfnWndProc: Some(toast_wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hCursor: LoadCursorW(None, windows::Win32::UI::WindowsAndMessaging::IDC_ARROW)
            .unwrap_or_default(),
        ..Default::default()
    };
    // 失败通常只是"类已存在"，正常。
    let _ = RegisterClassW(&wc);

    // ---- 3. 建窗 ----
    // WS_EX_LAYERED：整窗可改透明度（淡入淡出全靠它）。
    // WS_EX_TOPMOST：浮在最上层，别被别的窗口盖掉一条"转瞬即逝"的提示。
    // WS_EX_TOOLWINDOW：不进任务栏 / Alt+Tab——Toast 是临时信息，不是窗口。
    // 不带 WS_VISIBLE：先在 WM_CREATE 里量好尺寸、算好位置，再一次性显示，
    // 避免在 (0,0) 闪现。
    TEXT.with(|t| *t.borrow_mut() = text.to_string());
    TICK.with(|c| c.set(0));
    FONT.with(|c| c.set(0));
    VIEW.with(|c| c.set((0, 0)));

    let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_LAYERED,
        PCWSTR(class_name.as_ptr()),
        PCWSTR::null(),
        WS_POPUP | WS_CLIPCHILDREN,
        0,
        0,
        10,
        10,
        owner,
        None,
        Some(hinstance.into()),
        None,
    ) else {
        TEXT.with(|t| t.borrow_mut().clear());
        return;
    };

    // Win11 圆角（胶囊本身已是圆角，这句给窗口外壳补上，Win10 静默失败）。
    let pref: i32 = 2;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWINDOWATTRIBUTE(33),
        &pref as *const _ as *const std::ffi::c_void,
        std::mem::size_of::<i32>() as u32,
    );

    // ---- 4. 摆位：宿主右下角（宿主没有可用矩形时退回工作区右下角）----
    let (w, h) = VIEW.with(|c| c.get());
    if w <= 0 {
        let _ = DestroyWindow(hwnd);
        return;
    }
    let (x, y) = anchor(owner, w, h);
    let _ = SetWindowPos(hwnd, None, x, y, w, h, windows::Win32::UI::WindowsAndMessaging::SWP_SHOWWINDOW);

    // ---- 5. 起手全透明 + 启动动画定时器 ----
    // 必须先 SetLayeredWindowAttributes 定一次"用 alpha 混合"这个模式，
    // 之后每次只改 alpha 值才有淡入淡出效果（不设的话整窗是完全不透明的）。
    let _ = SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 0, windows::Win32::UI::WindowsAndMessaging::LWA_ALPHA);
    let _ = SetTimer(Some(hwnd), TIMER_ID, TICK_MS, None);

    CURRENT.with(|c| c.set(hwnd.0 as isize));
}

/// 计算 Toast 位置：右下角贴宿主（主控台），宿主不可用就贴工作区右下角。
unsafe fn anchor(owner: Option<HWND>, w: i32, h: i32) -> (i32, i32) {
    // 工作区 = 屏幕扣掉任务栏后的可用区域（任务栏上浮着条提示会挡住一半）。
    let mut work = RECT::default();
    let has_work = SystemParametersInfoW(
        SPI_GETWORKAREA,
        0,
        Some(&mut work as *mut _ as *mut _),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
    .is_ok();

    // 优先锚定宿主窗口右下角：主控台开着时，提示出现在自己窗口边上，
    // 视线不用离开操作区（方案要求"贴着窗口右下角"）。
    if let Some(o) = owner {
        let mut rc = RECT::default();
        if GetWindowRect(o, &mut rc).is_ok() {
            let ow = rc.right - rc.left;
            let oh = rc.bottom - rc.top;
            // 宿主矩形明显有效（隐藏消息窗这类"从未摆过位"的窗口宽高为 0，
            // 直接忽略它），且完全落在工作区内才采用。
            let inside = !has_work
                || (rc.left >= work.left
                    && rc.top >= work.top
                    && rc.right <= work.right
                    && rc.bottom <= work.bottom);
            if ow >= 200 && oh >= 120 && inside {
                return (rc.right - w - MARGIN, rc.bottom - h - MARGIN);
            }
        }
    }

    if has_work {
        (
            work.right - w - MARGIN,
            work.bottom - h - MARGIN,
        )
    } else {
        // 兜底：拿不到工作区就用屏幕右下角，往上多留 48px 让开任务栏。
        let sw = windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_CXSCREEN,
        );
        let sh = windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_CYSCREEN,
        );
        (sw - w - MARGIN, sh - h - MARGIN - 48)
    }
}

// ============================================================================
// 窗口过程
// ============================================================================

unsafe extern "system" fn toast_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    _lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            build(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => {
            // 背景由 WM_PAINT 全量重画，这里直接说"已擦除"，
            // 挡掉系统背景擦除，少一次全窗重绘 = 肉眼可见的闪烁没了。
            LRESULT(1)
        }
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_ID as usize {
                tick(hwnd);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // Toast 没有可点区域，系统级关闭（如任务管理器发来）直接收掉。
            let _ = KillTimer(Some(hwnd), TIMER_ID);
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            // ★ 关键差异：这里绝对不能 PostQuitMessage！
            // Toast 跑在宿主线程的循环里（主控台循环 / 主消息循环），
            // 发 WM_QUIT 等于把宿主循环一并退出——界面会莫名其妙关掉。
            // Toast 的退出靠 tick() 在动画结束时主动 DestroyWindow。
            let _ = KillTimer(Some(hwnd), TIMER_ID);
            FONT.with(|c| {
                let raw = c.get();
                if raw != 0 {
                    let _ = DeleteObject(HFONT(raw as *mut _).into());
                }
                c.set(0);
            });
            CURRENT.with(|c| c.set(0));
            TEXT.with(|t| t.borrow_mut().clear());
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, _lparam),
    }
}

// ============================================================================
// 尺寸 / 绘制 / 动画
// ============================================================================

/// WM_CREATE：按当前窗口 DPI 量文字、定尺寸（高度固定 44 逻辑像素）。
unsafe fn build(hwnd: HWND) {
    let scale = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd).max(96) as i32;
    SCALE.with(|c| c.set(scale));
    let px = |v: i32| (v as f32 * scale as f32 / 96.0).round() as i32;

    // 字体：一行 14px 微软雅黑（和正文一个字族，只是小一号）。
    let face = wide("Microsoft YaHei UI");
    let font = CreateFontW(
        px(-14),
        0,
        0,
        0,
        FW_NORMAL.0 as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY,
        FF_DONTCARE.0.into(),
        PCWSTR(face.as_ptr()),
    );
    FONT.with(|c| c.set(font.0 as isize));

    // 量文字宽度：胶囊宽 = 左品牌条 + 左内距 + 文字 + 右内距，不用写死宽度，
    // 提示语长短都合身（方案里那条"该应用已经在列表中"也就十来个字）。
    let text = TEXT.with(|t| t.borrow().clone());
    let text_w = measure_line(&text, font.0 as isize);
    let h = px(44); // 单行胶囊高度
    let w = px(3 + 16) + text_w + px(20); // 品牌条3 + 左距16 + 文字 + 右距20
    let w = w.max(px(120)); // 太短的提示（如"已复制"）也给个像样的最小宽度
    VIEW.with(|c| c.set((w, h)));
}

/// WM_PAINT：深色胶囊 + 左侧品牌色竖标 + 白字。
unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let s = SCALE.with(|c| c.get());
    let px = |v: i32| (v as f32 * s as f32 / 96.0).round() as i32;
    let font_raw = FONT.with(|c| c.get());
    let text = TEXT.with(|t| t.borrow().clone());

    let mut crc = RECT::default();
    let _ = GetClientRect(hwnd, &mut crc); // Toast 是纯客户区窗口，画在 0,0 起算的坐标里
    let w = crc.right;
    let h = crc.bottom;

    // ① 深色胶囊本体（半径取高度一半 = 上下两条边完全圆）
    let pill = RECT {
        left: 0,
        top: 0,
        right: w,
        bottom: h,
    };
    fill_round(hdc, pill, TOAST_BG, h / 2);

    // ② 左侧品牌色竖标：一行纯文字太素，一条 3px 的品牌色既点明
    //    "这是 HelloGate 说的话"，又几乎不占空间。直接用 Palette::BRAND，
    //    将来换主题色这里自动跟着变。
    fill_round(
        hdc,
        RECT {
            left: px(10),
            top: px(10),
            right: px(13),
            bottom: h - px(10),
        },
        Palette::BRAND,
        px(2),
    );

    // ③ 白色文字，左对齐、垂直居中
    if font_raw != 0 {
        let _ = SelectObject(hdc, HFONT(font_raw as *mut _).into());
    }
    let _ = windows::Win32::Graphics::Gdi::SetBkMode(
        hdc,
        windows::Win32::Graphics::Gdi::TRANSPARENT,
    );
    draw_text_left(
        hdc,
        &text,
        RECT {
            left: px(23),
            top: 0,
            right: w - px(12),
            bottom: h,
        },
        TOAST_FG,
    );

    let _ = EndPaint(hwnd, &ps);
}

/// 定时器推进一档：算 alpha → 应用 → 到点自毁。
unsafe fn tick(hwnd: HWND) {
    let t = TICK.with(|c| {
        let v = c.get() + 1;
        c.set(v);
        v
    });
    let total = FADE_IN + HOLD + FADE_OUT;
    let alpha: u8 = if t <= FADE_IN {
        // 淡入：第1档 42 … 第6档 255（每档约 1/6）
        (t * 255 / FADE_IN).clamp(0, 255) as u8
    } else if t <= FADE_IN + HOLD {
        255
    } else if t <= total {
        // 淡出：剩下几档就剩多少透明度
        let k = total - t;
        (k * 255 / FADE_OUT).clamp(0, 255) as u8
    } else {
        255 // 走到 else 分支马上销毁，值给多少都无所谓
    };

    if t > total {
        let _ = KillTimer(Some(hwnd), TIMER_ID);
        let _ = DestroyWindow(hwnd);
        return;
    }
    let _ = SetLayeredWindowAttributes(
        hwnd,
        windows::Win32::Foundation::COLORREF(0),
        alpha,
        windows::Win32::UI::WindowsAndMessaging::LWA_ALPHA,
    );
}

/// 量单行文字宽度（胶囊定宽用），返回物理像素宽度。
unsafe fn measure_line(text: &str, font_raw: isize) -> i32 {
    if font_raw == 0 {
        return 0;
    }
    let hdc = CreateCompatibleDC(None);
    let old = SelectObject(hdc, HFONT(font_raw as *mut _).into());
    let mut buf: Vec<u16> = text.encode_utf16().collect();
    let mut rect = RECT::default();
    let _ = DrawTextW(
        hdc,
        &mut buf,
        &mut rect,
        DT_LEFT | DT_SINGLELINE | DT_CALCRECT | DT_NOPREFIX,
    );
    SelectObject(hdc, old);
    let _ = DeleteDC(hdc);
    rect.right - rect.left
}

/// 把 Rust 字符串转成 NUL 结尾的 UTF-16。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
