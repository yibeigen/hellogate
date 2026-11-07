//! 自绘提示 / 确认对话框（UI 优化方案 A1：系统弹窗清零）
//!
//! # 为什么要有这个模块
//! 之前 HelloGate 的每一句提示都是 `MessageBoxW`——系统原生弹窗：
//! 灰白配色、Tahoma 字体、方头直角按钮，和主控台这套"浅灰底 + 白卡片 +
//! 琥珀警告条"的界面放在一起，视觉上完全是两个软件（方案文档里那几张
//! "系统弹窗拖后腿"的截图说的就是它）。更糟的是 MessageBox 无法自定义
//! 尺寸、按钮文案和排版，想对齐现有设计只能自己画。
//!
//! 这个模块提供三个入口，覆盖原来 10 处 `MessageBoxW` 调用的全部形态：
//! - [`info`]：蓝色 i 图标 + 单个「确定」（提示类，如"还没有选中任何行"）；
//! - [`warn`]：琥珀 ! 图标 + 单个「确定」（错误类，如"两次输入的主密码不一致"）；
//! - [`confirm`]：是 / 否二选一（决策类，如"设置尚未保存"），返回用户选了什么。
//!
//! # 设计要点
//! - **外观与主控台完全同一套**：白色顶栏 + 品牌 Logo + ✕、浅灰底、白色圆角
//!   卡片、品牌色胶囊主按钮——全部复用 `ui.rs` 里的绘制原语，不是重新画一套。
//! - **自带模态消息循环**：`MessageBoxW` 的核心价值是"弹出来之后卡住调用方，
//!   直到用户回答"。我们在 `show()` 里自己跑 `GetMessageW` 循环实现同样的阻塞，
//!   同时用 `EnableWindow(owner, false)` 禁掉宿主窗口，杜绝"弹窗开着还能在底下
//!   乱点"。主控台、托盘常驻、守门进程三个线程都能直接调用（各自没有循环也没关系）。
//! - **正文自动量高**：消息可能是多行甚至几段（"设置尚未保存"那种），
//!   窗口高度按 `DrawTextW(DT_CALCRECT)` 量出来的真实高度生成，永不截字。
//! - **Esc = 否 / 关闭，Enter = 是**：模态循环里手工接了这两个键，
//!   因为自绘按钮不是标准对话框控件，`IsDialogMessageW` 不会替我们翻译回车。

use std::cell::{Cell, RefCell};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleDC, CreateFontW, DeleteDC, DeleteObject, DrawTextW, EndPaint,
    SelectObject, HFONT, PAINTSTRUCT, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET,
    DT_CALCRECT, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_WORDBREAK, FF_DONTCARE, FW_NORMAL,
    FW_SEMIBOLD, OUT_DEFAULT_PRECIS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
// DRAWITEMSTRUCT + ODS_SELECTED：自绘按钮（BS_OWNERDRAW）的绘制请求与"正被按住"状态
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_SELECTED};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect,
    GetDlgItem, GetSystemMetrics, GetWindowRect, IsDialogMessageW, LoadCursorW, GetMessageW,
    PostMessageW, PostQuitMessage, RegisterClassW, SendMessageW, SetForegroundWindow,
    SetWindowPos, TranslateMessage, DispatchMessageW, BS_OWNERDRAW, CS_HREDRAW, CS_VREDRAW,
    IDOK, IDCANCEL, MSG, SM_CXSCREEN, SM_CYSCREEN, SWP_SHOWWINDOW, WNDCLASSW, WINDOW_EX_STYLE,
    WINDOW_STYLE, WS_CHILD, WS_CLIPCHILDREN, WS_EX_TOOLWINDOW, WS_POPUP, WS_TABSTOP, WS_VISIBLE,
    WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_DRAWITEM, WM_LBUTTONDOWN, WM_NCLBUTTONDOWN,
    WM_PAINT, HMENU,
};
// 配色与绘制原语：对话框必须和主控台长得"像同一个软件"
use crate::ui::{
    draw_button, draw_card_soft, draw_logo, draw_text_center, draw_text_left, fill_round,
    fill_solid, round_window, Palette,
};

// ============================================================================
// 常量：96 DPI 下的逻辑布局（和其他窗口一样，画之前统一乘当前窗口的 DPI）
// ============================================================================
//
// 结构自上而下：
//   0..56    白色顶栏（品牌 Logo + HelloGate + 右上 ✕，和主控台/密码框一致）
//   72..底   白色圆角卡片：图标 + 标题 一行，正文若干行，右下按钮排
const DIALOG_CLASS: &str = "HelloGateDialogClass";
const W_LOGICAL: i32 = 460; // 窗口宽
const HEADER_H: i32 = 56; // 顶栏高（和密码框一致）
const CARD_TOP: i32 = 72; // 卡片上边
const PAD: i32 = 20; // 卡片内边距
const CARD_X: i32 = 16; // 卡片左右到窗口的距离
const MSG_Y: i32 = 134; // 正文起始纵坐标（图标 90..122，留 12px 呼吸）
const BTN_H: i32 = 40; // 按钮高
const BTN_GAP: i32 = 12; // 两个按钮之间的间距
const BODY_W: i32 = W_LOGICAL - CARD_X * 2 - PAD * 2; // 正文可用宽度 = 388

/// 正文高度 + 按钮行 + 卡片下边距 + 窗口下边距（换算见 `build`）。
const TAIL_H: i32 = PAD + BTN_H + PAD + CARD_X; // = 20+40+20+16 = 96

/// 弹窗形态。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 提示：蓝 i 图标，单按钮「确定」。
    Info,
    /// 警告：琥珀 ! 图标，单按钮「确定」。
    Warn,
    /// 确认：两按钮，返回用户是否点了「是」。
    Confirm,
}

/// 一次弹窗的全部输入（同一线程同时只会有一次弹窗，用 thread_local 足够）。
struct Params {
    kind: Kind,
    title: String,
    message: String,
    /// 主按钮文字：Info/Warn 固定「确定」，Confirm 由调用方给（如「保存并关闭」）。
    primary: String,
    /// 次按钮文字：仅 Confirm 使用（如「不保存」）。
    secondary: String,
}

thread_local! {
    static PARAMS: RefCell<Option<Params>> = RefCell::new(None);
    /// 本次弹窗创建的字体（存裸指针，窗口销毁时统一 DeleteObject）。
    /// 按 console.rs 的做法存 raw isize：HFONT 句柄随窗口生命周期，
    /// 存结构体反而要处理所有权，裸值最省事。
    static FONTS: RefCell<Fonts> = RefCell::new(Fonts::default());
    /// 当前窗口 DPI（96 = 100%）。
    static SCALE: Cell<i32> = const { Cell::new(96) };
    /// 正文量出来的高度（物理像素），WM_PAINT 排版时要用同一份值。
    static MSG_H: Cell<i32> = const { Cell::new(0) };
    /// 窗口最终尺寸（物理像素），WM_CREATE 算好后由 show 取用。
    static VIEW: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
    /// 结果：true = 用户点了主按钮（是 / 确定）。
    static RESULT: Cell<bool> = const { Cell::new(false) };
}

/// 弹窗用到的 4 号字体，raw isize；0 = 还没创建。
#[derive(Default, Clone, Copy)]
struct Fonts {
    head: isize, // "HelloGate" 顶栏文字
    title: isize, // 卡片标题 + 图标里的 i / !
    body: isize, // 正文（也用于量高）
    btn: isize, // 按钮文字
}

impl Fonts {
    /// 列出全部非零句柄，销毁时逐个释放（漏一个就是每次弹窗泄漏一个 GDI 对象）。
    fn all(&self) -> [isize; 4] {
        [self.head, self.title, self.body, self.btn]
    }
}

// ============================================================================
// 对外入口
// ============================================================================

/// 提示型弹窗（蓝 i 图标 + 单个「确定」），阻塞到用户点掉为止。
///
/// 替代原来的 `MessageBoxW(MB_OK | MB_ICONINFORMATION)`。
pub fn info(owner: Option<HWND>, title: &str, message: &str) {
    // SAFETY：show 内部全是建窗 / 泵消息这类 Win32 调用（edition 2021 下
    // unsafe fn 的函数体自动是 unsafe 上下文）。窗口句柄、参数都由它自管。
    unsafe { show(owner, Kind::Info, title, message, "确定", "") };
}

/// 警告型弹窗（琥珀 ! 图标 + 单个「确定」），阻塞到用户点掉为止。
///
/// 替代原来的 `MessageBoxW(MB_OK | MB_ICONWARNING)`。
pub fn warn(owner: Option<HWND>, title: &str, message: &str) {
    unsafe { show(owner, Kind::Warn, title, message, "确定", "") };
}

/// 二选一确认弹窗，返回 `true` = 用户点了 `yes`。
///
/// 替代原来的 `MessageBoxW(MB_YESNO | MB_ICONWARNING)`：
/// 「是」= 主按钮（品牌色，页面上最显眼的那个），「否」= 白底描边次按钮。
/// Esc、点右上 ✕ 都等同于「否」（返回 false），和系统 MB_YESNO 的行为一致。
pub fn confirm(owner: Option<HWND>, title: &str, message: &str, yes: &str, no: &str) -> bool {
    unsafe { show(owner, Kind::Confirm, title, message, yes, no) }
}

/// 建窗 + 模态循环 + 取结果（三个入口的共同实现）。
///
/// # 为什么要自己跑消息循环
/// `MessageBoxW` 是"边弹边替你泵消息"的阻塞调用；换掉它之后，
/// 调用方（主控台的保存/删除逻辑等）必须仍然停在原地等用户回答。
/// 这里复制了 password.rs 已经验证过的模态循环写法：
/// `GetMessageW` 取消息 → `IsDialogMessageW` 处理 Tab/方向键 → 翻译分发。
/// 循环在窗口销毁时由 `PostQuitMessage` 结束。
///
/// # unsafe
/// 函数体内是清一色的 Win32 建窗 / 消息 API（windows crate 全部标 unsafe），
/// 这里按 edition 2021 的惯例把整个函数标成 unsafe，入口处三个安全函数
/// 各自用一句 `unsafe {}` 承担责任——和 password.rs::prompt_password 同一取舍。
unsafe fn show(owner: Option<HWND>, kind: Kind, title: &str, message: &str, primary: &str, secondary: &str) -> bool {
    // 1. 参数进 thread_local（窗口过程 / 绘制函数都从这里读）。
    PARAMS.with(|p| {
        *p.borrow_mut() = Some(Params {
            kind,
            title: title.to_string(),
            message: message.to_string(),
            primary: primary.to_string(),
            secondary: secondary.to_string(),
        })
    });
    RESULT.with(|r| r.set(false));
    MSG_H.with(|c| c.set(0));
    VIEW.with(|c| c.set((0, 0)));

    // 2. 注册窗口类。
    //    画刷故意不设（hbrBackground = null）：背景 100% 由我们自己的 WM_PAINT
    //    画。系统背景画刷会在每次重绘前先擦一遍底色，和我们的绘制叠在一起
    //    就是明显的闪烁——无边框自绘窗口的标准做法是完全接管背景。
    let class_name = wide(DIALOG_CLASS);
    let Ok(hinstance) = GetModuleHandleW(None) else {
        return false;
    };
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(dialog_wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hCursor: LoadCursorW(None, windows::Win32::UI::WindowsAndMessaging::IDC_ARROW)
            .unwrap_or_default(),
        // 品牌图标（exe 内嵌 ID=1 资源）：弹窗虽是 WS_EX_TOOLWINDOW 不进任务栏，
        // 但 Alt+Tab 之外的系统场景（如窗口切换器、截图工具）仍会读它。
        // 注：WNDCLASSW 无 hIconSm（WNDCLASSEXW 才有），小图标自动回退 hIcon。
        hIcon: crate::ui::load_app_icon(),
        ..Default::default()
    };
    // 注册失败通常只是"类已存在"（第二次及以后弹窗必现），属于正常情况。
    // 上面没建画刷，所以这里也没有要回收的东西。
    let _ = RegisterClassW(&wc);

    // 3. 创建窗口。
    //    - WS_EX_TOOLWINDOW：不进任务栏、不出现在 Alt+Tab——弹窗是临时的，
    //      不该占一个窗口位；顺带保证它压在宿主窗口之上。
    //    - 不带 WS_VISIBLE：先按 96 DPI 的逻辑尺寸创建（WM_CREATE 里量好
    //      正文、算出真实尺寸），SetWindowPos 定位后才显示，
    //      否则会先在 (0,0) 闪一下错误尺寸。
    //    - parent = owner：WS_POPUP 下这表示"所属窗口"，系统会保证
    //      弹窗永远压在宿主之上，宿主最小化时跟着最小化。
    let title_w = wide(title);
    let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOOLWINDOW,
        PCWSTR(class_name.as_ptr()),
        PCWSTR(title_w.as_ptr()),
        WS_POPUP | WS_CLIPCHILDREN,
        0,
        0,
        W_LOGICAL,
        100, // 临时高度，WM_CREATE 后按实际内容重设
        owner, // Some = 所属窗口（系统保证压在它上面）；None 则是独立顶层弹窗
        None,
        Some(hinstance.into()),
        None,
    ) else {
        PARAMS.with(|p| *p.borrow_mut() = None);
        return false;
    };

    // Win11 圆角：和主控台/密码框一致（Win10 上该调用失败会静默忽略）。
    round_window(hwnd);

    // 4. 按实际内容定尺寸并居中。
    //    尺寸是 WM_CREATE 里量正文算出来的（见 build），这里只负责摆位。
    let (w, h) = VIEW.with(|c| c.get());
    let (w, h) = if w > 0 { (w, h) } else { (W_LOGICAL, 240) };
    let (x, y) = unsafe { center_on(owner, w, h) };
    let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_SHOWWINDOW);

    // 5. 模态化：禁用宿主窗口（MessageBox 的等价行为）。
    //    只有"原本是启用的"才禁用，收尾时也只恢复那种——
    //    否则会把一个本来就禁用的窗口错误地打开。
    let mut need_reenable = false;
    if let Some(o) = owner {
        if IsWindowEnabled(o).as_bool() {
            let _ = EnableWindow(o, false);
            need_reenable = true;
        }
    }

    // 6. 推到前台并把初始焦点放到主按钮（这样回车直接生效，见循环里的注释）。
    let _ = BringWindowToTop(hwnd);
    let _ = SetForegroundWindow(hwnd);
    if let Ok(btn) = GetDlgItem(Some(hwnd), IDOK.0) {
        let _ = SetFocus(Some(btn));
    }

    // 7. 模态消息循环。
    let mut msg = MSG::default();
    loop {
        let r = GetMessageW(&mut msg, None, 0, 0);
        if !r.as_bool() {
            break; // WM_QUIT（窗口销毁时 PostQuitMessage 发出）
        }
        // 自绘按钮不是标准对话框控件，IsDialogMessageW 不会把回车翻译成
        // 「点击默认按钮」（它只对真正的 dialog 模板生效）。这里手动补上：
        // 回车 → IDOK（是 / 确定），Esc → IDCANCEL（否 / 关闭）。
        // 放在 IsDialogMessageW 之前，抢占 Tab/方向键交给它自己处理。
        if msg.message == windows::Win32::UI::WindowsAndMessaging::WM_KEYDOWN {
            match msg.wParam.0 as u32 {
                0x0D => {
                    // VK_RETURN
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_COMMAND,
                        WPARAM(IDOK.0 as usize),
                        LPARAM(0),
                    );
                    continue;
                }
                0x1B => {
                    // VK_ESCAPE
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_COMMAND,
                        WPARAM(IDCANCEL.0 as usize),
                        LPARAM(0),
                    );
                    continue;
                }
                _ => {}
            }
        }
        if !IsDialogMessageW(hwnd, &msg).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // 8. 恢复宿主，取结果，清理参数。
    if need_reenable {
        if let Some(o) = owner {
            let _ = EnableWindow(o, true);
            let _ = SetForegroundWindow(o);
        }
    }
    PARAMS.with(|p| *p.borrow_mut() = None);
    RESULT.with(|r| r.get())
}

/// 计算弹窗位置：有宿主就居中到宿主窗口上（用户视线不用移动），
/// 没有宿主（守门进程 / UAC 失败提示）才居中到屏幕。
///
/// # unsafe 说明
/// 内部调用 `GetWindowRect` / `GetSystemMetrics`（windows crate 标记为 unsafe），
/// 只是读屏幕几何信息，无副作用——调用方保证传入的 HWND 仍然有效即可。
unsafe fn center_on(owner: Option<HWND>, w: i32, h: i32) -> (i32, i32) {
    if let Some(o) = owner {
        let mut rc = RECT::default();
        // GetWindowRect 失败（窗口已销毁之类）就退回屏幕居中。
        if GetWindowRect(o, &mut rc).is_ok() {
            // 直接在宿主窗口里居中，不钳坐标：多显示器下宿主可能落在
            // 负坐标屏上（屏在主屏左侧），强行 max(0) 反而会把弹窗推去主屏。
            // 弹窗比宿主小，居中后必然完整落在宿主范围内，不会跑到屏外。
            let x = rc.left + (rc.right - rc.left - w) / 2;
            let y = rc.top + (rc.bottom - rc.top - h) / 2;
            return (x, y);
        }
    }
    let sw = GetSystemMetrics(SM_CXSCREEN);
    let sh = GetSystemMetrics(SM_CYSCREEN);
    ((sw - w) / 2, (sh - h) / 2)
}

// ============================================================================
// 窗口过程
// ============================================================================

unsafe extern "system" fn dialog_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            if build(hwnd) {
                LRESULT(0)
            } else {
                // 返回 -1 会让 CreateWindowExW 失败，调用方走"没弹出来"分支。
                LRESULT(-1)
            }
        }
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_DRAWITEM => {
            // BS_OWNERDRAW 按钮的绘制请求（是 / 否 / 确定）。
            let dis = lparam.0 as *const DRAWITEMSTRUCT;
            if !dis.is_null() {
                draw_item(&*dis);
            }
            LRESULT(1) // 1 = 已处理
        }
        WM_LBUTTONDOWN => {
            // 顶栏空白处拖动窗口；右上 ✕ = 取消/否。
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            let s = SCALE.with(|c| c.get());
            let px = |v: i32| (v as f32 * s as f32 / 96.0).round() as i32;
            // lParam 低 16 位 = x，高 16 位 = y；先截 16 位再按有符号读，
            // 多屏左侧的负坐标才不会算错。
            let x = (lparam.0 as i32 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 as i32 >> 16) & 0xFFFF) as i16 as i32;
            if y < px(HEADER_H) {
                if x >= rect.right - px(52) {
                    // 关闭按钮：单按钮弹窗等同「确定」，二选一弹窗等同「否」。
                    let accept = PARAMS.with(|p| {
                        p.borrow().as_ref().map(|pp| pp.kind != Kind::Confirm).unwrap_or(false)
                    });
                    RESULT.with(|r| r.set(accept));
                    let _ = DestroyWindow(hwnd);
                } else {
                    // 无边框窗口没有系统标题栏可拖：释放捕获后伪造一次
                    // "点在标题栏上"的消息，让系统接管拖动（和 password.rs 一致）。
                    let _ = windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
                    let _ = SendMessageW(
                        hwnd,
                        WM_NCLBUTTONDOWN,
                        Some(WPARAM(windows::Win32::UI::WindowsAndMessaging::HTCAPTION as usize)),
                        Some(LPARAM(0)),
                    );
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            // wParam 低 16 位 = 控件 ID（按钮通知码在高 16 位，恒为 0）。
            let id = (wparam.0 & 0xFFFF) as i32;
            if id == IDOK.0 {
                RESULT.with(|r| r.set(true));
                let _ = DestroyWindow(hwnd);
            } else if id == IDCANCEL.0 {
                RESULT.with(|r| r.set(false));
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // Alt+F4 / 系统关闭：单按钮弹窗视为已确认（否则 MessageBox 也返回
            // IDOK），二选一弹窗视为「否」。
            let accept = PARAMS.with(|p| {
                p.borrow().as_ref().map(|pp| pp.kind != Kind::Confirm).unwrap_or(false)
            });
            RESULT.with(|r| r.set(accept));
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            // 释放本次弹窗创建的 5 个字体（不释放的话反复弹窗会累积
            // GDI 句柄，进程级上限 1 万个左右，满了之后全界面开始画不出来）。
            FONTS.with(|f| {
                let fonts = *f.borrow();
                for raw in fonts.all() {
                    if raw != 0 {
                        let _ = DeleteObject(HFONT(raw as *mut _).into());
                    }
                }
                *f.borrow_mut() = Fonts::default();
            });
            PARAMS.with(|p| *p.borrow_mut() = None);
            // 结束模态循环（见 show 的第 7 步）。
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ============================================================================
// 建控件 / 量尺寸
// ============================================================================

/// WM_CREATE：量正文高度、定窗口尺寸、创建按钮。
/// 返回 false 表示失败（会让整个建窗流程中止）。
unsafe fn build(hwnd: HWND) -> bool {
    // 当前窗口所在屏幕的 DPI（96 = 100%）。窗口此刻还在 (0,0)，
    // 系统会按主屏 DPI 返回——和 password.rs 同一处理方式，
    // 之后 SetWindowPos 到宿主所在的屏时，若两屏 DPI 不同会有细微缩放偏差
    // （Win32 单窗口只能取一个 DPI，除非监听 WM_DPICHANGED，此处按现网主流做法简化）。
    let scale = GetDpiForWindow(hwnd).max(96) as i32;
    SCALE.with(|c| c.set(scale));
    let px = |v: i32| (v as f32 * scale as f32 / 96.0).round() as i32;

    // ---- 字体（微软雅黑 UI，和主控台/密码框同一字族，视觉才统一）----
    // FW_* 在新版 windows crate 里是 FONT_WEIGHT 结构体（内部 u32），
    // 取 `.0 as i32` 换回 CreateFontW 要的字重参数。
    let fonts = Fonts {
        head: make_font(px(-13), FW_SEMIBOLD.0 as i32).0 as isize,
        title: make_font(px(-17), FW_SEMIBOLD.0 as i32).0 as isize,
        body: make_font(px(-15), FW_NORMAL.0 as i32).0 as isize,
        btn: make_font(px(-14), FW_NORMAL.0 as i32).0 as isize,
    };
    FONTS.with(|f| *f.borrow_mut() = fonts);

    let params = match current_params() {
        Some(p) => p,
        None => return false,
    };

    // ---- 量正文高度：窗口高度完全由内容决定 ----
    let msg_h = measure_multiline(&params.message, fonts.body, px(BODY_W)).max(px(22));
    MSG_H.with(|c| c.set(msg_h));

    // 窗口高 = 正文起始 134 + 正文高 + (卡片内距 20 + 按钮 40 + 卡片内距 20 + 窗口下边距 16)
    let w = px(W_LOGICAL);
    let h = px(MSG_Y) + msg_h + px(TAIL_H);
    VIEW.with(|c| c.set((w, h)));

    // ---- 按钮 ----
    // 宽度按文字实际量（DT_CALCRECT），下限 96px：
    // 「保存并关闭」这类 5 字按钮和「确定」这种 2 字按钮都不该被裁字，
    // 也不会出现"文案改一个字按钮就变形"的老问题。
    let btn_class = wide("Button");
    let style = WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32);
    let btn_top = px(MSG_Y) + msg_h + px(PAD);
    let right = px(W_LOGICAL - CARD_X - PAD); // 按钮右边界与正文右边界对齐
    let primary_w = measure_line(&params.primary, fonts.btn).max(px(96));
    let primary_rect = RECT {
        left: right - primary_w,
        top: btn_top,
        right,
        bottom: btn_top + px(BTN_H),
    };
    let Ok(hinstance) = GetModuleHandleW(None) else {
        return false;
    };

    // 主按钮（IDOK = 是 / 确定）：品牌色胶囊，页面上最显眼的可点元素。
    let primary_text = wide(&params.primary);
    if CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(btn_class.as_ptr()),
        PCWSTR(primary_text.as_ptr()),
        style,
        primary_rect.left,
        primary_rect.top,
        primary_rect.right - primary_rect.left,
        primary_rect.bottom - primary_rect.top,
        Some(hwnd),
        Some(HMENU(IDOK.0 as *mut _)),
        Some(hinstance.into()),
        None,
    )
    .is_err()
    {
        return false;
    }

    // 次按钮（IDCANCEL = 否）：只在确认弹窗里存在，白底细描边，
    // 视觉权重刻意低于主按钮——避免用户误点"不保存"。
    if params.kind == Kind::Confirm {
        let secondary_w = measure_line(&params.secondary, fonts.btn).max(px(88));
        let secondary_text = wide(&params.secondary);
        let r = RECT {
            left: primary_rect.left - px(BTN_GAP) - secondary_w,
            top: btn_top,
            right: primary_rect.left - px(BTN_GAP),
            bottom: primary_rect.bottom,
        };
        if CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(btn_class.as_ptr()),
            PCWSTR(secondary_text.as_ptr()),
            style,
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            Some(hwnd),
            Some(HMENU(IDCANCEL.0 as *mut _)),
            Some(hinstance.into()),
            None,
        )
        .is_err()
        {
            return false;
        }
    }

    true
}

/// 把 Rust 字符串转成 NUL 结尾的 UTF-16。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 创建字体（微软雅黑 UI，与主控台统一）。
unsafe fn make_font(height: i32, weight: i32) -> HFONT {
    let face = wide("Microsoft YaHei UI");
    CreateFontW(
        height,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        CLEARTYPE_QUALITY,
        FF_DONTCARE.0.into(),
        PCWSTR(face.as_ptr()),
    )
}

/// 量多行文字的换行后高度（DT_CALCRECT + 自动折行）。
/// 【为什么要量】正文可能是"两段话 + 空行"（设置尚未保存那种），
/// 窗口高度写死必然出现截字；量出来再定高则永远刚好。
unsafe fn measure_multiline(text: &str, font_raw: isize, width: i32) -> i32 {
    if font_raw == 0 {
        return 0;
    }
    let hdc = CreateCompatibleDC(None);
    let old = SelectObject(hdc, HFONT(font_raw as *mut _).into());
    let mut buf: Vec<u16> = text.encode_utf16().collect();
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: 0,
    };
    // DT_NOPREFIX：正文里的 & 就是普通的 &，不当成"快捷键下划线"标记。
    let _ = DrawTextW(hdc, &mut buf, &mut rect, DT_LEFT | DT_WORDBREAK | DT_CALCRECT | DT_NOPREFIX);
    SelectObject(hdc, old);
    let _ = DeleteDC(hdc);
    rect.bottom - rect.top
}

/// 量单行文字的宽度（按钮文案用）。
unsafe fn measure_line(text: &str, font_raw: isize) -> i32 {
    if font_raw == 0 {
        return 0;
    }
    let hdc = CreateCompatibleDC(None);
    let old = SelectObject(hdc, HFONT(font_raw as *mut _).into());
    let mut buf: Vec<u16> = text.encode_utf16().collect();
    let mut rect = RECT::default();
    let _ = DrawTextW(hdc, &mut buf, &mut rect, DT_LEFT | DT_SINGLELINE | DT_CALCRECT | DT_NOPREFIX);
    SelectObject(hdc, old);
    let _ = DeleteDC(hdc);
    rect.right - rect.left
}

// ============================================================================
// 绘制
// ============================================================================

/// WM_PAINT：整窗自绘（顶栏 / 卡片 / 图标 / 标题 / 正文）。
///
/// 背景全部由这里画——窗口类没设背景画刷（见 show 第 2 步），
/// 所以先用页面底色铺满客户区，再往上面盖顶栏和卡片。
unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let s = SCALE.with(|c| c.get());
    let px = |v: i32| (v as f32 * s as f32 / 96.0).round() as i32;
    let fonts = FONTS.with(|f| *f.borrow());
    let params = current_params();
    let Some(params) = params else {
        let _ = EndPaint(hwnd, &ps);
        return;
    };

    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);

    // ① 页面底色
    fill_solid(hdc, rc, Palette::BG);

    // ② 白色顶栏 + 1px 分隔线（和密码框完全一致的头部结构）
    fill_solid(
        hdc,
        RECT {
            left: 0,
            top: 0,
            right: rc.right,
            bottom: px(HEADER_H),
        },
        Palette::CARD, // 顶栏用卡片白（Palette 里没有单独的 WHITE，CARD 就是 #FFFFFF）
    );
    fill_solid(
        hdc,
        RECT {
            left: 0,
            top: px(HEADER_H),
            right: rc.right,
            bottom: px(HEADER_H) + 1,
        },
        Palette::BORDER, // 顶栏和内容之间那条 1px 分隔线
    );

    // ③ 头部：品牌 Logo（手绘，与 exe 内嵌 .ico 同构）+ "HelloGate" + 右上 ✕
    draw_logo(
        hdc,
        RECT {
            left: px(20),
            top: px(17),
            right: px(40),
            bottom: px(37),
        },
    );
    if fonts.head != 0 {
        let _ = SelectObject(hdc, HFONT(fonts.head as *mut _).into());
    }
    draw_text_left(
        hdc,
        "HelloGate",
        RECT {
            left: px(48),
            top: px(17),
            right: px(220),
            bottom: px(37),
        },
        Palette::TEXT,
    );
    // ✕ 不用字体里的图标字形，而和主控台一样当文字画（两笔交叉，任何
    // 微软雅黑版本都有这个码位），省一套画线逻辑。
    draw_text_center(
        hdc,
        "✕",
        RECT {
            left: px(410),
            top: px(14),
            right: px(434),
            bottom: px(38),
        },
        Palette::TEXT_SUB, // ✕ 用次要文字灰：够看清，但不跟标题抢注意力
    );

    // ④ 白色圆角卡片（带柔和投影，复用主控台的卡片画法）
    let card = RECT {
        left: px(CARD_X),
        top: px(CARD_TOP),
        right: rc.right - px(CARD_X),
        bottom: rc.bottom - px(CARD_X),
    };
    draw_card_soft(hdc, card, px(24));

    // ⑤ 图标胶囊：Info = 品牌蓝，Warn = 琥珀（配色来自 ui::Palette 的 WARN 系）
    let icon = RECT {
        left: px(CARD_X + PAD),
        top: px(90),
        right: px(CARD_X + PAD + 32),
        bottom: px(122),
    };
    let icon_color = if params.kind == Kind::Warn {
        Palette::WARN
    } else {
        Palette::BRAND
    };
    fill_round(hdc, icon, icon_color, px(10));
    if fonts.title != 0 {
        let _ = SelectObject(hdc, HFONT(fonts.title as *mut _).into());
    }
    draw_text_center(
        hdc,
        if params.kind == Kind::Warn { "!" } else { "i" },
        icon,
        Palette::ON_BRAND, // 图标底上的白色字形
    );

    // ⑥ 标题（和图标同一行，垂直居中）
    draw_text_left(
        hdc,
        &params.title,
        RECT {
            left: px(CARD_X + PAD + 44),
            top: px(90),
            right: card.right - px(PAD),
            bottom: px(122),
        },
        Palette::TEXT, // 卡片标题用正文色（#111827）
    );

    // ⑦ 正文：顶部对齐的多行文字。
    //    高度用 MSG_H（WM_CREATE 量出来的同一份值），保证"量多高画多高"，
    //    不会因为画的时候再量一次出现 1px 级的对不齐。
    if fonts.body != 0 {
        let _ = SelectObject(hdc, HFONT(fonts.body as *mut _).into());
    }
    let msg_h = MSG_H.with(|c| c.get());
    let mut mrect = RECT {
        left: px(CARD_X + PAD),
        top: px(MSG_Y),
        right: card.right - px(PAD),
        bottom: px(MSG_Y) + msg_h,
    };
    let mut buf: Vec<u16> = params.message.encode_utf16().collect();
    windows::Win32::Graphics::Gdi::SetBkMode(
        hdc,
        windows::Win32::Graphics::Gdi::TRANSPARENT,
    );
    let _ = windows::Win32::Graphics::Gdi::SetTextColor(hdc, COLORREF(Palette::TEXT));
    let _ = DrawTextW(
        hdc,
        &mut buf,
        &mut mrect,
        DT_LEFT | DT_WORDBREAK | DT_NOPREFIX,
    );

    let _ = EndPaint(hwnd, &ps);
}

/// 自绘按钮（是 / 否 / 确定）。
///
/// 【原理】带 BS_OWNERDRAW 的按钮不再自己画，系统把"画在哪、什么状态"
/// 打包成 DRAWITEMSTRUCT 发给父窗口（和 console/password 同一套机制）。
unsafe fn draw_item(dis: &DRAWITEMSTRUCT) {
    let hdc = dis.hDC;
    let rc = dis.rcItem;
    let pressed = (dis.itemState.0 & ODS_SELECTED.0) != 0;

    // 显式选按钮字体：自绘 DC 的字体是系统默认的，不选就会用宋体小字画按钮，
    // 和量宽度时用的雅黑对不上（按钮文字会被裁掉半截）。
    let fonts = FONTS.with(|f| *f.borrow());
    if fonts.btn != 0 {
        let _ = SelectObject(hdc, HFONT(fonts.btn as *mut _).into());
    }

    let s = SCALE.with(|c| c.get());
    let px = |v: i32| (v as f32 * s as f32 / 96.0).round() as i32;
    let radius = px(BTN_H / 2); // 胶囊：半径 = 高度一半
    let params = current_params();
    let Some(params) = params else { return };

    if dis.CtlID as i32 == IDOK.0 {
        // 主按钮：按下时压深一号（和主控台保存按钮同一套反馈）。
        let bg = if pressed {
            Palette::BRAND_DARK
        } else {
            Palette::BRAND
        };
        draw_button(
            hdc,
            rc,
            Palette::CARD, // 按钮底下是白卡片，圆角外圈必须先铺卡片色
            bg,
            Palette::ON_BRAND,
            &params.primary,
            None,
            radius,
        );
    } else {
        // 次按钮：白底 + 浅描边，按下时整块变浅灰，给出"按下去了"的即时反馈。
        let bg = if pressed {
            Palette::BG
        } else {
            Palette::CARD
        };
        draw_button(
            hdc,
            rc,
            Palette::CARD,
            bg,
            Palette::TEXT,
            &params.secondary,
            Some(Palette::BORDER), // 次按钮的浅灰描边（和输入框外框同一档深浅）
            radius,
        );
    }
}

/// 读取当前弹窗参数的**克隆**。
///
/// 【为什么要克隆一份】绘制 / 建控件的代码可能在借着参数的同时再读一次
/// 参数（例如按钮绘制要拿按钮文案），长期持有 `RefCell` 的借用会在第二次
/// 借用时直接 panic（"already borrowed"）。取出来一份值，借用立刻释放，
/// 谁在什么时候读都安全；弹窗一次只有一份参数，克隆开销可以忽略。
fn current_params() -> Option<Params> {
    PARAMS.with(|p| {
        p.borrow().as_ref().map(|pp| Params {
            kind: pp.kind,
            title: pp.title.clone(),
            message: pp.message.clone(),
            primary: pp.primary.clone(),
            secondary: pp.secondary.clone(),
        })
    })
}
