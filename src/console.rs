//! 主控台 UI 模块（Phase 4）
//!
//! 职责：提供一个图形界面让用户管理受保护应用列表、选择认证方式、设置主密码与自启开关。
//!
//! # UI 技术选型说明（为什么用 Win32 原生控件而非 WebView/Tauri）
//! - **零依赖**：不引入 WebView2 运行时，exe 体积保持在数百 KB。
//! - **毫秒级启动**：原生控件无 WebView 初始化开销。
//! - **零杀软误报**：不加载脚本引擎、不访问网络，行为完全可预测。
//!
//! # 视觉设计（本版重点：彻底重做系统控件）
//! 原生控件的默认外观停留在 Windows 2000 年代：灰底 3D 按钮、凹框输入框、
//! 报表风格的 ListView、带小三角的 ComboBox。本版把这三样**全部换掉**：
//! - **白色顶栏**（不是整块靛蓝）：左侧一枚品牌色圆角方块当 Logo，
//!   标题与副标题用文字层级区分，右上角是浅灰色自绘 ✕ —— 这是
//!   Stripe / Linear 这类 SaaS 控制台的典型头部；
//! - **两张白色圆角卡片**（带柔和投影）浮在浅灰底上 → 用留白和层次代替边框；
//! - **应用列表换成自绘控件**（见 `applist.rs`）：没有表头、没有网格、没有系统
//!   滚动条，行高更宽松，勾选框是圆角小方块；
//! - **认证方式换成"分段控件"**：两个并排胶囊替代下拉框，选项一眼可见、少一次点击；
//! - **输入框去掉系统凹陷边框**：Edit 本身不画边框，由父窗口在它下面画圆角外框，
//!   聚焦时外框变成品牌色（网页里的 focus ring）；
//! - **按钮 / 复选框全部自绘**（`BS_OWNERDRAW` + `WM_DRAWITEM`）→ 摆脱系统灰白渐变。
//!
//! # 界面布局（760 x 820，尺寸为 96 DPI 下的逻辑像素）
//! ```text
//! ┌──────────────────────────────────────────────────────────┐
//! │ [W] HelloGate                                      ✕       │ ← 白色顶栏 h=84
//! │     应用锁 · 打开前验证，通过才启动                       │
//! ├──────────────────────────────────────────────────────────┤
//! │  ┌────────────────────────────────────────────────────┐  │
//! │  │ 受保护的应用                          (＋添加)(移除)│  │ ← 卡片一 (24,100)-(736,468)
//! │  │ ┌────────────────────────────────────────────────┐ │  │
//! │  │ │ [x] 记事本                                     │ │  │ ← 自绘列表（行高 64）
//! │  │ │     notepad.exe                                │ │  │
//! │  │ └────────────────────────────────────────────────┘ │  │
//! │  │ 勾选框=启用/停用；点行选中后才能移除                 │  │
//! │  └────────────────────────────────────────────────────┘  │
//! │  ┌────────────────────────────────────────────────────┐  │
//! │  │ 验证开关                                            │  │ ← 卡片二 (24,484)-(736,744)
//! │  │ 认证方式 [系统验证|自设密码]  解锁：…              │  │
//! │  │ 新密码（留空表示不修改）    确认新密码   [x]显示密码│  │
//! │  │ ┌────────────────┐ ┌────────────────┐            │  │
//! │  │ 当前状态：尚未设置主密码 —— 请设置一个自己的密码    │  │
//! │  └──────────────────────────────────────────────────┘  │
//! │                                   ( 取消 )  ( 保存 )   │
//! └────────────────────────────────────────────────────────┘
//! ```

use std::cell::{Cell, RefCell};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
// 说明：本文件只保留主控台"直接用得到"的 GDI 函数。
// 圆角卡片、手绘对勾、居中文字这些基础件已经搬到 ui.rs，所以这里不再导入
// DrawTextW / RoundRect / CreatePen 等——留着会变成编译警告。
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, EndPaint, HDC, HFONT, InvalidateRect,
    SelectObject, SetBkColor, SetBkMode, SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, FF_DONTCARE, FW_NORMAL, FW_SEMIBOLD, OPAQUE, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
    TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
// 顶栏入口的悬停高亮：要登记「鼠标移出窗口」通知，否则高亮会一直留着。
// （SetCapture / ReleaseCapture / EnableWindow 也在这个模块——
//   password.rs 里 ReleaseCapture 就是从这里导入的。）
use windows::Win32::UI::Input::KeyboardAndMouse::{
    // SetCapture：按下顶栏入口后把鼠标消息"抓"到本窗口，
    // 即使按住移出窗口范围也照样能收到 WM_LBUTTONUP（标准按钮语义的地基）。
    EnableWindow, GetKeyState, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE,
    TRACKMOUSEEVENT,
    // VK_CONTROL：Ctrl 键的虚拟键码，用来识别 Ctrl+S 保存快捷键。
    VK_CONTROL,
};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Controls::{
    // 自绘按钮/复选框时系统会把绘制任务打包成 DRAWITEMSTRUCT 发给我们；
    // ODS_SELECTED 用来判断"按钮正处于被按下"的状态（按下时要画深一号的颜色）；
    // ODS_FOCUS = 系统标记"该自绘控件当前持有键盘焦点"（Tab 轮到它 / 点击它时置位），
    // 靠它才能给自绘控件补画键盘焦点环——系统不会替自绘控件画焦点框。
    DRAWITEMSTRUCT, ODS_FOCUS, ODS_SELECTED,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowW,
    GetClientRect, GetDlgItem, GetForegroundWindow, GetWindowThreadProcessId,
    // IsDialogMessageW：把主控台的消息循环变成"对话框"——
    // 由它把 Esc 翻译成 IDCANCEL、Enter 翻译成 IDOK、Tab 切焦点（方案 D）。
    IsDialogMessageW,
    // IDOK / IDCANCEL：IsDialogMessageW 翻译出来的两个标准命令 ID
    // （MESSAGEBOX_RESULT 类型，比较时用 .0 取底层 i32）。
    IDOK, IDCANCEL,
    // KillTimer / SetTimer：「保存」按钮 200ms 防连点定时器（见 begin_save / WM_TIMER 分支）。
    // PostMessageW：把 Ctrl+S 转发成一条"点保存"的 WM_COMMAND（见 open 的消息循环）。
    HMENU, KillTimer, LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassW, SendMessageW,
    SetForegroundWindow, SetTimer, SetWindowTextW, ShowWindow,
    CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT,
    HTCAPTION, IDC_ARROW, SW_MINIMIZE, SW_SHOW, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLOREDIT,
    WM_CTLCOLORSTATIC, WM_DESTROY,
    // WM_KEYDOWN：识别 Ctrl+S 快捷键；WM_LBUTTONUP：顶栏入口"松开才触发"的后半段。
    WM_DRAWITEM, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCLBUTTONDOWN,
    // WM_TIMER：保存按钮 200ms 防连点定时器到点的通知。
    WM_NCHITTEST, WM_PAINT, WM_SETFONT, WM_TIMER, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW,
    WS_CHILD, WS_EX_APPWINDOW, WS_POPUP, WS_TABSTOP,
    WS_CLIPCHILDREN,
    WS_VISIBLE,
    // BS_OWNERDRAW：让按钮/复选框把"怎么画"交给我们自己处理（见 draw_owner_item）。
    // 这是本次美化能摆脱系统灰白按钮的关键开关。
    BS_OWNERDRAW,
};

use crate::about;
use crate::applist;
use crate::config::{Config, ProtectedApp};
// 配色与基础绘制原语统一放在 ui.rs（解锁弹窗 password.rs 也用同一套），
// 这样两个窗口的按钮、卡片、圆角永远一致，不会各自漂移成两种样子。
use crate::ui::{
    create_input, draw_button, draw_card_soft, draw_check_box, draw_close_glyph, draw_input_state,
    // draw_logo：手绘品牌 Logo（与 exe 内嵌 .ico 同构），顶栏左上角用。
    draw_logo,
    // draw_round_border：只描边不填充的圆角矩形，自绘控件的键盘焦点环复用它
    //（和输入框外框是同一套画法，保证两种焦点提示长得一样）。
    draw_round_border, draw_segment, draw_text_center, draw_text_left, fill_round, fill_solid,
    round_window, solid_brush, window_text, InputState, Palette,
};
// 用 main.rs 里的 logln! 宏写日志（它保证落盘，不像 println! 在 GUI 进程里会被丢弃）。
use crate::logln;

// 控件 ID
// 说明：原来给 ListView 用的 IDC_LIST 已经随 ListView 一起删掉——
// 应用列表现在是 applist.rs 里的自绘控件，不再需要 ID 去查找它。
const IDC_ADD: i32 = 2002;
const IDC_REMOVE: i32 = 2003;
/// ★ 总开关：勾选 = 打开受保护软件之前先验证；取消 = 完全不拦（等效"关掉锁"）。
const IDC_GUARD: i32 = 2015;
const IDC_AUTOSTART: i32 = 2007;
const IDC_SAVE: i32 = 2008;
const IDC_CANCEL: i32 = 2009;
/// 主密码输入框（新密码）。
const IDC_PWD: i32 = 2011;
/// 主密码输入框（确认）。
const IDC_PWD2: i32 = 2012;
/// 认证方式的动态说明文字（随分段控件的选择变化）。
const IDC_AUTH_HINT: i32 = 2014;
/// 主密码的「当前状态」提示行。
/// 之所以要有独立 ID：这一行在"出厂默认密码"时要用琥珀色标出来，
/// 窗口过程需要知道它到底是哪个控件（见 WM_CTLCOLORSTATIC）。
const IDC_PWD_STATUS: i32 = 2016;
/// 「显示密码」复选框：勾上后两个密码框临时显示明文，方便用户核对有没有打错。
/// 它的状态同样是自绘控件自己记账（见 SHOW_PWD_CHECKED）。
const IDC_SHOW_PWD: i32 = 2017;
/// 认证方式分段控件的两个片段（替代原来的下拉框）：
/// 2018 = 系统验证，2019 = 自设密码。
const IDC_AUTH_SYS: i32 = 2018;
const IDC_AUTH_PWD: i32 = 2019;
/// 总开关右侧那句淡化提示（"取消勾选后，这些程序可直接打开"）。
/// 【为什么给它一个 ID】它是全界面唯一需要**更浅**文字色的说明，
/// WM_CTLCOLORSTATIC 要靠 ID 认出它、单独设成 TEXT_MUTED。
const IDC_GUARD_HINT: i32 = 2020;

/// 「主密码状态」banner 的三种语气（A2 状态行 banner 化）。
///
/// 【为什么用 u8 而不是 bool】原来只有"警示/不警示"两态，够用；
/// banner 化之后多了"绿色成功态"（两次输入一致），三态用一个字节最直白，
/// match 起来也比嵌套 if 清楚。
const TONE_NORMAL: u8 = 0; // 普通：底色就是卡片白（视觉上"没有 banner"，归于平静）
const TONE_WARN: u8 = 1; // 警示：淡琥珀底 + 琥珀竖条（尚未设置密码 / 输入不一致）
const TONE_SUCCESS: u8 = 2; // 成功：淡绿底 + 绿色竖条（两次输入一致，可以保存了）

/// 主控台窗口类名。
const CONSOLE_CLASS: &str = "HelloGateConsoleClass";

/// `WM_MOUSELEAVE` 的消息号（winuser.h 里有，windows crate 没有导出这个常量）。
const WM_MOUSELEAVE: u32 = 0x02A3;

/// 「保存」防连点定时器的 ID（WM_TIMER 到点时用 wparam 报出这个 ID 来区分）。
/// console 目前没有创建过任何定时器，选 7 不会与已有 ID 冲突——
/// 定时器 ID 只要求在"同一个窗口"内唯一，7 远离常用的小数值即可。
const SAVE_TIMER_ID: usize = 7;

/// 顶栏右侧三个入口的矩形（物理像素）。
///
/// 横向位置全部从**客户区右边界**往左推算，而不是写死坐标：
/// 这样即使以后把窗口改宽，入口也始终贴着右上角，不会漂到屏幕中间。
/// 返回值：`[0]` = GitHub，`[1]` = 作者，`[2]` = 检查更新。
fn header_link_rects(client_right: i32) -> [RECT; 3] {
    // 最右边是三个窗口按钮（最小化/最大化/关闭，见 window_btn_rects），
    // 共占 132px；入口区从按钮区左侧再让出 8px 空隙开始往左排。
    // 垂直方向：顶栏高 84，入口胶囊高 32 → 上下各留 (84-32)/2 = 26 居中。
    let author_right = client_right - sc(140);
    let author = RECT {
        left: author_right - sc(96),
        top: sc(26),
        right: author_right,
        bottom: sc(58),
    };
    // GitHub 入口：紧挨在作者左边，间隔 8px，宽 68（14px 字下"GitHub"刚好放下）。
    let github = RECT {
        left: author.left - sc(8) - sc(68),
        top: sc(26),
        right: author.left - sc(8),
        bottom: sc(58),
    };
    // 检查更新入口：GitHub 再往左，宽 84（"检查更新"四个 14px 汉字 ≈ 56 + 左右内边距）。
    let update = RECT {
        left: github.left - sc(8) - sc(84),
        top: sc(26),
        right: github.left - sc(8),
        bottom: sc(58),
    };
    [github, author, update]
}

/// 顶栏最右侧三个窗口按钮的矩形（物理像素）。
///
/// 【为什么自绘而不用系统标题栏】主控台是无边框 WS_POPUP 窗口（为了
/// 圆角和自定义顶栏视觉），没有系统标题栏，最小化/最大化/关闭只能自己画。
/// 每个按钮 44px 宽、整条顶栏高，和系统窗口的点击热区尺寸一致，好按。
/// 返回值：`[0]` = 最小化，`[1]` = 最大化/还原，`[2]` = 关闭。
fn window_btn_rects(client_right: i32) -> [RECT; 3] {
    let close = RECT {
        left: client_right - sc(44),
        top: 0,
        right: client_right,
        bottom: sc(84),
    };
    let max = RECT {
        left: client_right - sc(88),
        top: 0,
        right: client_right - sc(44),
        bottom: sc(84),
    };
    let min = RECT {
        left: client_right - sc(132),
        top: 0,
        right: client_right - sc(88),
        bottom: sc(84),
    };
    [min, max, close]
}

/// ★ 是否处于"最大化"状态（v1.0.0）。
///
/// 【为什么是进程级原子量而不是 thread_local】sc() 是 pub(crate) 全局换算函数，
/// about.rs / applist.rs 都在用它；缩放基准（SYSTEM_DPI）本身也是进程级静态量，
/// 最大化标志和它必须同生命周期，否则"谁在最大化"会对不上。
static MAXIMIZED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 最大化 / 还原切换（v1.0.0）。
///
/// # 实现思路：不重排版，只换"缩放基准"
/// 界面按 760×820 逻辑像素设计，所有几何都经 sc() 换算成物理像素。
/// sc() 读的是进程级 SYSTEM_DPI——所以"最大化"只需要：
/// 1. 把 SYSTEM_DPI 临时抬到"工作区刚好装得下 760×820"的倍率；
/// 2. 销毁全部子控件、按新 DPI 重建、窗口居中到工作区。
/// 整套布局（卡片、列表、输入框）自动等比放大，**永远不会排坏**——
/// 这比给每个控件写"拉伸规则"省几百行代码，视觉上也保持设计稿的比例关系。
/// 还原 = 重新调用 detect_dpi() 把基准算回正常值，再走同一套重建流程。
unsafe fn toggle_maximize(hwnd: HWND) {
    // BOOL 在 windows crate 的 core 模块；TRUE 没导出，用 BOOL(1) 表示。
    use windows::core::BOOL;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumChildWindows, SystemParametersInfoW, SPI_GETWORKAREA,
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };

    // 1. 先采集界面当前状态：子控件马上要整体销毁重建，
    //    不先收下来，用户刚勾选的应用、刚选的认证方式就丢了。
    let cur = CONTROLS.with(|c| c.borrow().clone()).map(|h| collect_config(h));

    // 2. 算目标缩放基准。
    if !MAXIMIZED.load(Ordering::SeqCst) {
        // 最大化：取"工作区能装下 760×820"的最大倍率（上限 3.0 倍，
        // 防止 4K 大屏把字放到荒谬的大小）。
        let mut work = RECT::default();
        if SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_err()
        {
            return; // 拿不到工作区就放弃，不动现状
        }
        let avail_w = (work.right - work.left) as f32;
        let avail_h = (work.bottom - work.top) as f32;
        if avail_w <= 0.0 || avail_h <= 0.0 {
            return;
        }
        let scale = (avail_w / 760.0).min(avail_h / 820.0).min(3.0);
        SYSTEM_DPI.store(((scale * 96.0).round() as u32).max(96), Ordering::Relaxed);
    } else {
        // 还原：detect_dpi 会按系统 DPI + 工作区钳制重新算回"正常基准"。
        detect_dpi();
    }

    // 3. 销毁全部子控件。
    //    EnumChildWindows 逐个回调，回调里 DestroyWindow——
    //    父窗口只负责画背景，子控件全部由 build_controls 重建。
    unsafe extern "system" fn destroy_child(child: HWND, _lparam: LPARAM) -> BOOL {
        let _ = DestroyWindow(child);
        BOOL(1)
    }
    let _ = EnumChildWindows(
        Some(hwnd),
        Some(destroy_child),
        LPARAM(0),
    );

    // 4. 新尺寸 + 居中（sc(760)/sc(820) 已经按新基准放大）。
    center_window(hwnd, sc(760), sc(820));

    // 5. 按采集到的状态重建控件，整窗重绘。
    if let Some(cfg) = cur {
        build_controls(hwnd, &cfg);
    }
    let _ = InvalidateRect(Some(hwnd), None, true);

    MAXIMIZED.store(!MAXIMIZED.load(Ordering::SeqCst), Ordering::SeqCst);
}

/// 判断一个点是否落在矩形内（顶栏入口的点击与悬停判定共用）。
fn point_in(rc: RECT, x: i32, y: i32) -> bool {
    x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom
}

/// 只让顶栏那一条区域失效重绘（不整窗刷新，避免大面积闪烁）。
///
/// 【为什么单独抽出来】顶栏的悬停、按下、松开三种状态变化都要"只重画顶栏"，
/// 这段取客户区 + 拼矩形的代码重复太多遍了，抽成函数保证三处画的是同一块区域。
unsafe fn invalidate_header(hwnd: HWND) {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let header = RECT {
        left: 0,
        top: 0,
        right: rc.right,
        bottom: sc(84), // 顶栏高度（与 draw_console_background 里画白条的高度一致）
    };
    let _ = InvalidateRect(Some(hwnd), Some(&header), false);
}

/// 主密码「当前状态」banner 的矩形（已按当前 DPI 换算成物理像素）。
///
/// 【唯一事实来源】create_controls 用它摆 Static、draw_console_background
/// 用它画圆角底和左侧竖条——两边取同一个矩形，几何永远不会再分叉
/// （改位置只改这一处）。
/// 位置：卡片二内、密码框（底部 698）下方、卡片底（744）上方。
/// 高 32 = 14px 文字行（20）+ 上下各 6px 呼吸位。
fn pwd_status_banner_rect() -> RECT {
    RECT {
        left: sc(48),
        top: sc(706),
        right: sc(712),
        bottom: sc(738),
    }
}

// ============================================================================
// 配色表
// ============================================================================
//
// 【关键概念：COLORREF 的字节顺序】
// Win32 的颜色值不是网页里的 #RRGGBB，而是 0x00BBGGRR——**红蓝互换**。
// 例如 #4F46E5（R=4F, G=46, B=E5）写成 COLORREF 就是 0x00E5464F。
// 下面每个常量后面都标了对应的网页色值，改主题时照着换算即可。
//
// 把所有颜色集中在一个结构里：以后想换主题（比如改成墨绿、深蓝），
// 只改这一处，不用在几百行绘图代码里到处找颜色数字。
// 具体颜色常量见 crate::ui::Palette（已在文件顶部 use 进来）。
// 这里保留这段说明是因为颜色是"改起来最容易出错"的地方：
// 记住 COLORREF 是 0x00BBGGRR（红蓝互换），改色时务必照此换算。

// 用户点击「保存」后的结果，供调用方读取。
// 用线程局部存储，避免全局可变状态。
thread_local! {
    /// 保存后的配置（None 表示未保存/已取消）。
    static SAVED_CONFIG: RefCell<Option<Config>> = const { RefCell::new(None) };
    /// 控件句柄集合（窗口过程里频繁使用，缓存起来避免重复查找）。
    static CONTROLS: RefCell<Option<ControlHandles>> = const { RefCell::new(None) };
    /// 窗口**打开那一刻**的配置快照。
    /// 关窗时拿「界面当前状态」和它对比，就能知道用户有没有改了没保存
    /// （见 request_close），避免"删了行却没点保存，下次又回来了"的困惑。
    static ORIGINAL_CONFIG: RefCell<Option<Config>> = const { RefCell::new(None) };
    /// 「主密码状态」banner 当前的语气（见 TONE_* 常量）。
    /// 【为什么单独记】WM_CTLCOLORSTATIC 选文字/底色、父窗口画圆角底和竖条，
    /// 两处都要按语气取色——创建控件/实时引导时算一次存起来，
    /// 比每次重绘都去解密配置重新判断便宜得多。
    static PWD_STATUS_TONE: Cell<u8> = const { Cell::new(TONE_NORMAL) };
    /// banner 语气的**兜底值**（窗口打开那一刻的状态）。
    /// 用户在密码框打字时语气会临时变化（见 update_pwd_status），
    /// 两个框清空后要靠它还原成"当前状态：…"对应的语气。
    static PWD_STATUS_BASE_TONE: Cell<u8> = const { Cell::new(TONE_NORMAL) };
    /// 「主密码状态」这一行的**兜底文案**（窗口打开那一刻的"当前状态：…"）。
    ///
    /// 【为什么需要】这一行现在是"动态"的：用户在两个密码框里打字时，
    /// 它会临时变成"两次输入不一致 / 请再输一遍"这类**实时引导**；
    /// 等两个框清空（用户放弃修改），又要变回原来的"当前状态：…"。
    /// 所以创建控件时把那段兜底文案存一份，实时反馈随时能退回去。
    static PWD_STATUS_BASE: RefCell<String> = RefCell::new(String::new());
    /// 自绘控件时选进设备上下文（DC）的字体句柄（存裸值绕开 Send/Sync 限制）。
    ///
    /// 【为什么要自己存】自绘按钮收到的 DC 不一定带了我们想要的字体，
    /// 不显式 SelectObject 的话，文字会退回系统默认的宋体/小字号，
    /// 和旁边的原生控件明显不一致。
    static UI_FONT: Cell<isize> = const { Cell::new(0) };
    /// 两个自绘复选框的勾选状态（我们自己记账，不依赖系统控件）。
    ///
    /// 【为什么不直接用 BM_GETCHECK】
    /// 复选框改成 `BS_OWNERDRAW` 之后，它不再是"有勾选语义的复选框"，
    /// 只是一个需要我们全权负责外观的按钮。实测 `BM_SETCHECK` 设进去的状态
    /// `BM_GETCHECK` 读不回来——界面就会永远显示"没勾选"。
    /// 与其依赖这种未定义行为，不如自己存两个 bool：读写确定、调试直观。
    static GUARD_CHECKED: Cell<bool> = const { Cell::new(true) };
    static AUTOSTART_CHECKED: Cell<bool> = const { Cell::new(false) };
    /// 「显示密码」是否处于勾选（= 显示明文）状态。
    /// 和上面两个复选框一样，BS_OWNERDRAW 不保留勾选语义，必须自己记账。
    static SHOW_PWD_CHECKED: Cell<bool> = const { Cell::new(false) };
    /// 认证方式分段控件的选中项：`true` = 自设密码，`false` = 系统验证。
    ///
    /// 【为什么用 bool 而不是原来那个下拉框索引】
    /// 分段控件只有两个选项，一个布尔就够了；而且下拉框被删掉后，
    /// 再没有"没有选中项（-1）"这种状态需要考虑。
    /// 这个值在 build_controls 里按配置初始化，点击片段时翻转（见 set_auth_method）。
    static AUTH_SEL: Cell<bool> = const { Cell::new(true) };
    /// 顶栏入口的悬停状态：0 = 没有，1 = GitHub，2 = 作者。
    ///
    /// 【为什么要自己记】顶栏是父窗口自己画的（没有对应的子控件），
    /// 想给鼠标一个"这里能点"的反馈，就必须在 WM_MOUSEMOVE 里判断
    /// 鼠标落在哪个入口上，然后只重绘顶栏那一条区域。
    static HEADER_HOVER: Cell<u8> = const { Cell::new(0) };
    /// 当前哪个密码框拥有键盘焦点：0 = 都没有，1 = 新密码框，2 = 确认框。
    ///
    /// 【为什么要自己记】两个输入框的系统边框已经去掉了，圆角外框是由父窗口
    /// 在 WM_PAINT 里画的。父窗口不知道 Edit 什么时候拿到/失去焦点，
    /// 只能靠 Edit 发来的 EN_SETFOCUS / EN_KILLFOCUS 通知来记账，
    /// 然后重绘自己，把外框切换成"品牌色"或"浅灰"。
    static FOCUSED_PWD: Cell<u8> = const { Cell::new(0) };
    /// 「移除」按钮是否处于"待确认"状态（两段式删除，方案 D · 危险操作）。
    /// 第一次点击只把它置为 true、按钮变红字「确认移除？」；
    /// 第二次点击才真正删行——误触一次不会立刻丢数据。
    static REMOVE_ARMED: Cell<bool> = const { Cell::new(false) };
    /// 进入待确认状态时选中的是哪一行（-1 = 无）。
    /// 选中行一旦变化就重新"预备"，避免"红字还亮着、实际选的却是另一行"，
    /// 一下删掉没被确认过的行。
    static REMOVE_ARMED_SEL: Cell<i32> = const { Cell::new(-1) };
    /// 顶栏入口的按下状态：0 = 没按，1 = GitHub，2 = 作者。
    ///
    /// 【为什么要自己记】标准按钮语义是"按下先只记账，松开时鼠标仍落在同一入口内
    /// 才真正执行动作"。原来鼠标一按下就立刻 `about::open`，按住拖走再松开也会
    /// 误触发，而且没有"按下去了"的视觉反馈。按下时用 SetCapture 捕获鼠标后，
    /// 靠这个变量 + WM_LBUTTONUP 里的矩形复判来决定要不要真触发。
    static HEADER_PRESSED: Cell<u8> = const { Cell::new(0) };
    /// 保存进行中的门闩：true = 已点过保存、正处于 200ms「保存中…」窗口期。
    ///
    /// 【为什么需要】保存要走校验 + PBKDF2 派生 + IFEO 同步一整套流程，
    /// 手快连点（或按住 Enter 不放）会重复触发整套流程。这个门闩让
    /// 第二次及以后的点击直接被忽略，是防连点的核心（见 begin_save）。
    static SAVING: Cell<bool> = const { Cell::new(false) };
}

/// 读取自绘复选框的勾选状态。
fn check_state(id: i32) -> bool {
    match id {
        IDC_GUARD => GUARD_CHECKED.with(|c| c.get()),
        IDC_AUTOSTART => AUTOSTART_CHECKED.with(|c| c.get()),
        IDC_SHOW_PWD => SHOW_PWD_CHECKED.with(|c| c.get()),
        _ => false,
    }
}

/// 写入自绘复选框的勾选状态。
fn set_check_state(id: i32, on: bool) {
    match id {
        IDC_GUARD => GUARD_CHECKED.with(|c| c.set(on)),
        IDC_AUTOSTART => AUTOSTART_CHECKED.with(|c| c.set(on)),
        IDC_SHOW_PWD => SHOW_PWD_CHECKED.with(|c| c.set(on)),
        _ => {}
    }
}

/// 主控台里各控件的句柄。
#[derive(Clone, Copy)]
struct ControlHandles {
    /// 自绘应用列表的窗口句柄（增删行后要通知它重绘）。
    list: HWND,
    // 注：两个复选框的句柄不再需要缓存——它们的勾选状态由
    // GUARD_CHECKED / AUTOSTART_CHECKED 记账（见上方说明）。
    // 认证方式也不再是控件句柄：选中项存在 AUTH_SEL 里。
    /// 主密码：新密码框 / 确认框。
    pwd: HWND,
    pwd2: HWND,
}

/// 把 Rust 字符串转成 NUL 结尾的 UTF-16。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ============================================================================
// DPI 缩放
// ============================================================================
//
// 界面里所有尺寸（窗口大小、控件坐标、字号）都是按「96 DPI = 100% 缩放」写的逻辑值。
// 在 125% / 150% / 200% 的屏幕上，必须把它们乘以缩放比例，否则：
//   - 窗口会被系统放大而控件还挤在左上角；
//   - 或者文字小得看不清、按钮尺寸比例失调。
//
// 做法：进程启动时已声明 DPI 感知（见 main.rs），这里再把系统 DPI 读出来，
// 之后所有像素值统一过一遍 `sc()` 换算成真实物理像素。

use std::sync::atomic::{AtomicU32, Ordering};

/// 实际使用的缩放基准（96 = 100%）。
/// 注意：它不一定等于系统 DPI——屏幕装不下时会主动往下压，见 detect_dpi。
/// 缓存成静态变量，避免每次创建控件都调一次 API。
static SYSTEM_DPI: AtomicU32 = AtomicU32::new(96);

/// 读取并缓存缩放基准。必须在创建任何窗口之前调用一次。
///
/// # 这里做的不只是"读 DPI"
/// 界面按 760x820 逻辑像素设计。如果直接按系统 DPI 放大，在 125% / 175% 缩放
/// 的小屏笔记本上会得到一个比屏幕还高的窗口——底部的「保存」按钮直接被切掉，
/// 用户根本点不到（这是实测踩到的问题）。
///
/// 所以这里做两步：
/// 1. 先取系统 DPI（96 = 100%，168 = 175%）；
/// 2. 再用「工作区能装下多少」去压这个系数，保证窗口永远完整可见。
///    宁可字小一点，也不能让关键按钮跑到屏幕外面。
fn detect_dpi() {
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };
    unsafe {
        let dpi = GetDpiForSystem();
        if dpi == 0 {
            return;
        }

        // 第 1 步：名义缩放倍数（96 DPI = 1.0 倍）。
        let mut scale = dpi as f32 / 96.0;

        // 第 2 步：按桌面工作区（已扣掉任务栏）压一压。
        // 设计尺寸 760x820，额外留 24 像素边距，避免窗口紧贴屏幕边缘。
        let mut work = RECT::default();
        if SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
        {
            let avail_w = (work.right - work.left) as f32;
            let avail_h = (work.bottom - work.top) as f32;
            if avail_w > 0.0 && avail_h > 0.0 {
                // 分母 784 = 760+24，844 = 820+24：保证任何屏幕尺寸下
                // 窗口（含边距）都装得进工作区，小屏只是整体按比例缩小。
                scale = scale.min(avail_w / 784.0).min(avail_h / 844.0);
            }
        }

        // 下限 0.5 倍：再小的屏幕也不至于把界面缩到没法看。
        let effective = ((scale * 96.0).round() as u32).max(48);
        SYSTEM_DPI.store(effective, Ordering::Relaxed);
    }
}

/// 把「96 DPI 下的逻辑像素」换算成当前 DPI 下的真实物理像素。
///
/// 例：`sc(760)` 在 200% 屏幕上返回 1520。
pub(crate) fn sc(logical: i32) -> i32 {
    // DPI 来源不改（见 ui::scale 文档与决策 D2）：继续读进程级缓存 SYSTEM_DPI，
    // 因为本函数在 CreateWindowExW 之前就会被调用，那时窗口还不存在，取不到
    // GetDpiForWindow；且 detect_dpi() 含"窗口不得超出工作区"的钳制逻辑。
    crate::ui::scale(logical, SYSTEM_DPI.load(Ordering::Relaxed) as i32)
}

/// 对外暴露当前缩放基准（96 = 100%）。
///
/// 【谁需要它】自绘列表控件 `applist.rs` 在创建时要传一份 DPI：
/// 它内部的行高、内边距、勾选框尺寸都按逻辑像素设计，必须用同一个系数换算，
/// 否则列表里的行高会和卡片里其它元素对不上（高分屏上尤其明显）。
/// 这里只读缓存，不重新查询系统，开销可以忽略。
pub fn dpi() -> i32 {
    SYSTEM_DPI.load(Ordering::Relaxed) as i32
}

/// 注册主控台窗口类。
fn register_class() -> Result<(), String> {
    let class_name = wide(CONSOLE_CLASS);
    unsafe {
        let hinstance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        // 背景刷：用**实色**浅灰白画刷，而不是 NULL_BRUSH（不擦除）。
        //
        // 【为什么】
        // NULL_BRUSH 表示"我不擦背景"，系统就真的不擦——于是凡是自绘没有覆盖到的
        // 地方（比如两个按钮之间的空隙）都会露出未初始化的黑色，界面像破了个洞。
        // 换成实色刷之后，系统会自动把整个客户区涂成浅色，任何缝隙都是干净的。
        // 这个画刷生命周期与进程相同，不需要（也不能提前）释放。
        // 底色统一取配色表里的页面底色，保证「系统擦除背景」和「我们的自绘」
        // 用的是同一个颜色，页面不会出现两种深浅不一的灰。
        let bg_brush = CreateSolidBrush(COLORREF(Palette::BG));
        // 品牌图标（exe 内嵌 ID=1 资源，与顶栏手绘 Logo 同构）。
        // 主控台是 WS_APPWINDOW 窗口——任务栏按钮和 Alt-Tab 缩略图用的就是它，
        // 是全应用最关键的图标落点。
        // 注：WNDCLASSW 没有 hIconSm 字段（那是 WNDCLASSEXW 的）；
        // 系统取"小图标"时会自动回退到 hIcon，所以填 hIcon 一处即全覆盖。
        let app_icon = crate::ui::load_app_icon();
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(console_wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: bg_brush,
            hIcon: app_icon,
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
        Ok(())
    }
}

/// 如果主控台已经打开，就把它带到前台，返回 `true`；否则返回 `false`。
///
/// # 为什么需要这个检查？
/// 「打开主控台」有三条触发路径：托盘左键单击、托盘右键菜单、以及再次双击 exe。
/// 如果每条路径都新建一个窗口，用户点几下就会看到好几个主控台叠在一起。
/// 所以统一在这里做一次「已存在就复用」的检查，保证任何时候最多只有一个主控台。
pub fn focus_existing() -> bool {
    let class_name = wide(CONSOLE_CLASS);
    unsafe {
        let Ok(hwnd) = FindWindowW(PCWSTR(class_name.as_ptr()), PCWSTR::null()) else {
            return false;
        };
        if hwnd.0.is_null() {
            return false;
        }
        // 先恢复显示（窗口可能被最小化），再强行抢到前台。
        force_foreground(hwnd);
        true
    }
}

/// 把窗口强行推到前台并获得焦点。
///
/// # 为什么需要这个
/// Windows 有一条「前台锁定」规则：只有**当前拥有前台窗口**的进程，
/// 才有资格调用 `SetForegroundWindow` 把自己的窗口提到最前。
/// 我们的主控台是用户双击 exe 后由后台进程创建的，此时前台是别的程序（比如浏览器），
/// 于是 `SetForegroundWindow` 会被系统**静默忽略**——窗口创建成功了、也"可见"，
/// 但就是压在别人下面。用户看到的仍然是原来的界面，自然会觉得"双击没反应"。
///
/// # 三道保险，逐级加强
/// 1. `ShowWindow`：确保窗口没被最小化（最小化窗口不能成为前台窗口）。
/// 2. 「置顶 → 立刻取消置顶」：这一对组合把窗口推到**普通窗口栈的最顶端**，
///    用户能看见，但**不会永远压在所有软件上面**。
///    【为什么不用永久置顶】之前把主控台设成常驻 TOPMOST，造成两个问题：
///    (1) 用户切去别的软件，主控台还赖在最前面挡着，体验很差；
///    (2) 之后弹出的文件选择框反而被压在主控台**下面**，用户以为程序卡死了。
/// 3. `AttachThreadInput` + `SetForegroundWindow`：抢键盘焦点，让用户能直接打字。
///    这一步失败也不影响「能看见窗口」这个核心目标。
unsafe fn force_foreground(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    };

    // 1. 确保窗口处于显示状态
    let _ = ShowWindow(hwnd, SW_SHOW);

    // 2. 先置顶保证一定升到最顶，再立刻取消置顶落回普通窗口栈。
    //    注意 windows 0.62 里第二个参数是 Option<HWND>，两个常量都要包 Some()。
    let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW;
    let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
    let _ = SetWindowPos(hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, flags);

    // 3. 常规方式抢焦点（如果本进程本来就有前台权限，这一步就够了）
    let _ = BringWindowToTop(hwnd);
    let _ = SetForegroundWindow(hwnd);

    let foreground = GetForegroundWindow();
    if foreground == hwnd {
        return; // 已经拿到前台，收工
    }

    // 4. 抢不到就走 AttachThreadInput 这条"后门"
    let mut fg_pid = 0u32;
    let fg_thread = GetWindowThreadProcessId(foreground, Some(&mut fg_pid));
    let my_thread = GetCurrentThreadId();

    // 同一线程的窗口本来就能自由设置前台，不需要 attach；
    // fg_thread 为 0 说明前台窗口无效，也没法 attach。
    if fg_thread == 0 || fg_thread == my_thread {
        return;
    }

    // 挂上对方的输入队列 → 再次设置前台 → 立刻解绑
    if AttachThreadInput(my_thread, fg_thread, true).as_bool() {
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        let _ = AttachThreadInput(my_thread, fg_thread, false);
    }
}

/// 打开主控台窗口（阻塞直到窗口关闭）。
///
/// # 参数
/// - `cfg`：当前配置（会作为界面的初始值）。
///
/// # 返回
/// - `Some(Config)`：用户点击了「保存」，返回新配置。
/// - `None`：用户取消或直接关窗。
pub fn open(cfg: &Config) -> Option<Config> {
    // 先看看是不是已经有一个主控台开着。有的话直接拉到前台复用，不要叠出第二个窗口。
    if focus_existing() {
        return None;
    }

    // 读取系统 DPI，后面的所有尺寸都按它换算（高分屏必备）
    detect_dpi();

    // 清空上一次的结果，并记下"打开时的原始配置"作为关窗对比的基线
    SAVED_CONFIG.with(|c| *c.borrow_mut() = None);
    ORIGINAL_CONFIG.with(|o| *o.borrow_mut() = Some(cfg.clone()));

    // 复位可能残留的交互门闩（thread_local 跨窗口存活于同一线程）：
    // - SAVING：上一个窗口若恰好在 200ms 保存窗口期里被关掉，WM_TIMER 再也不会
    //   到达（窗口已销毁），不复位的话下次打开时"保存"按钮会被门闩永久挡住；
    // - HEADER_PRESSED：同理，按住鼠标时窗口被销毁会留下残值。
    SAVING.with(|s| s.set(false));
    HEADER_PRESSED.with(|p| p.set(0));

    unsafe {
        // 说明：这里原来会调用 InitCommonControlsEx 初始化 ListView 公共控件。
        // ListView 已彻底移除，只剩 Button / Static / Edit 这类"系统自带"控件，
        // 它们不需要公共控件库初始化，所以这段可以整段删掉。
        if let Err(e) = register_class() {
            logln!("[console] 注册窗口类失败: {e}");
            return None;
        }

        let class_name = wide(CONSOLE_CLASS);
        let title = wide("HelloGate - 应用锁");

        // 用当前配置的堆副本指针作为 lpCreateParams 传给 WM_CREATE。
        // 注意：WM_CREATE 里会立刻 clone 一份，所以这个 Box 在 CreateWindowExW
        // 返回后即可释放（窗口过程已经拿到自己需要的数据）。
        let cfg_box = Box::new(cfg.clone());
        let cfg_ptr = Box::into_raw(cfg_box);

        // 创建一个无边框弹出窗口（自绘标题栏），居中显示
        //
        // 【窗口样式的关键取舍——这里踩过一个大坑】
        // 最初用了 `WS_EX_LAYERED | WS_EX_TOOLWINDOW`，结果用户双击后"什么都看不到"，
        // 原因有两个：
        //
        // 1. `WS_EX_LAYERED`（分层窗口）如果**没有**配套调用
        //    `SetLayeredWindowAttributes` 设置不透明度，系统会把它画成**完全透明**。
        //    这种情况下窗口确实存在、`IsWindowVisible` 返回真、甚至在任务栏有图标，
        //    但屏幕上一片空白——用户看到的只有它后面那个窗口。
        //    本模块并没有实现亚克力（亚克力在 overlay.rs），所以完全不需要 layered。
        //
        // 2. `WS_EX_TOOLWINDOW`（工具窗口）会让窗口既不出现在任务栏、也不进 Alt+Tab。
        //    配合上一条，用户就彻底没有地方能找到这个界面了。
        //
        // 所以：去掉 LAYERED，改用 `WS_EX_APPWINDOW` 强制显示在任务栏上。
        //
        // `WS_CLIPCHILDREN`：父窗口重绘时避开子控件所占区域，减少闪烁。
        let created = CreateWindowExW(
            WS_EX_APPWINDOW,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_POPUP | WS_VISIBLE | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            // 窗口尺寸 720x756 → 760x820（"轻量现代化"批次）：
            // 宽度 +40 = 卡片左右外边距 16→24 各吃 8，再加内容区 24；
            // 高度 +64 = 顶栏 +8、列表行高 56→64（4 行 +32）、
            // 输入框/按钮加高、保存行与卡片二的间距加大。
            sc(760),
            sc(820),
            None,
            None,
            Some(GetModuleHandleW(None).unwrap().into()),
            Some(cfg_ptr as *const _), // 通过 lpParam 传初始配置
        );

        // 无论成功与否，都要把 Box 收回来释放，避免内存泄漏。
        // （此处已在 unsafe 块内，无需再套一层 unsafe。）
        let _ = Box::from_raw(cfg_ptr);

        let Ok(hwnd) = created else {
            logln!("[console] 创建主控台窗口失败");
            return None;
        };

        // Win11 圆角窗口：无边框窗口默认是直角，调用一次 DWM 属性即可。
        round_window(hwnd);

        // 让窗口在屏幕中央显示（尺寸要跟着 DPI 走）
        center_window(hwnd, sc(760), sc(820));

        // 把窗口强行推到最前面。
        //
        // 【为什么光有 WS_VISIBLE 还不够】
        // Windows 有「前台锁定」机制：一个没有焦点的后台进程，不能随便把自己的窗口
        // 抢到最前面（防止恶意弹窗）。所以我们创建完窗口后，它可能老老实实地
        // 躲在用户当前正在用的窗口（比如编辑器）后面，用户完全看不见。
        // 对主控台这种"用户主动双击才出现"的窗口，必须突破这层限制。
        force_foreground(hwnd);

        // 模态消息循环：窗口关闭（DestroyWindow）后 PostQuitMessage 会让
        // GetMessageW 返回 0，从而退出循环。
        // 注意：必须加 `msg.hwnd.is_null()` 判断，只响应 WM_QUIT（线程级消息），
        // 不要响应其他窗口的 WM_DESTROY（否则子控件销毁会误退出循环）。
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, GetMessageW, TranslateMessage, MSG,
        };
        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if !r.as_bool() {
                break; // 收到 WM_QUIT
            }
            // Ctrl+S（VK_S = 0x53）→ 转发成"点保存"的 WM_COMMAND，和按钮 / Enter
            // 走同一个入口 begin_save（自带防连点门闩，连按也不会重复保存）。
            //
            // 【为什么放在 IsDialogMessageW 之前】它只负责 Tab/Enter/Esc 的对话框
            // 翻译，根本不认识 Ctrl+S，不先截获这条快捷键就永远漏了。
            // 【为什么不吞掉原消息】不 continue、照常 Translate/Dispatch：
            // Ctrl+S 不产生可打印字符，密码框里的输入不受影响。
            if msg.message == WM_KEYDOWN
                && msg.wParam.0 == 0x53 // 'S' 的虚拟键码
                // GetKeyState 返回 i16，最高位 = 该键当前正按着；转 u16 取 0x8000 位判断。
                && (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0
            {
                // PostMessageW 首参是 Option<HWND>；WM_COMMAND 的命令 ID 放在
                // wparam 低 16 位，IDC_SAVE 是 i32(2008) → as usize 转进去。
                let _ = PostMessageW(
                    Some(hwnd),
                    WM_COMMAND,
                    WPARAM(IDC_SAVE as usize),
                    LPARAM(0),
                );
            }
            // 【方案 D · Esc / Enter / Tab（P1）】让主控台具备标准对话框键盘行为：
            // - Esc → WM_COMMAND IDCANCEL → 走「取消」的未保存检查后关窗；
            // - Enter → WM_COMMAND IDOK → 触发「保存」（密码框里回车即保存）；
            // - Tab / Shift+Tab → 在带 WS_TABSTOP 的控件间切换焦点。
            // 返回 true = 消息已被它翻译处理，不要再走 Translate/Dispatch
            // （否则会把翻译好的 WM_COMMAND 又原样派发一遍）。
            if IsDialogMessageW(hwnd, &msg).as_bool() {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    SAVED_CONFIG.with(|c| c.borrow_mut().take())
}

/// 把窗口按给定尺寸居中显示。
unsafe fn center_window(hwnd: HWND, w: i32, h: i32) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SetWindowPos, SystemParametersInfoW, SM_CXSCREEN, SM_CYSCREEN,
        SPI_GETWORKAREA, SWP_NOZORDER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };

    // 【为什么不用整块屏幕居中】整块屏幕包含任务栏区域，按它居中会让窗口
    // 往下压、底部按钮被任务栏挡住。工作区（SPI_GETWORKAREA）已经扣掉任务栏，
    // 在它上面居中才是真正的"视觉居中"。
    let mut sw = GetSystemMetrics(SM_CXSCREEN);
    let mut sh = GetSystemMetrics(SM_CYSCREEN);
    let mut ox = 0;
    let mut oy = 0;
    let mut work = RECT::default();
    if SystemParametersInfoW(
        SPI_GETWORKAREA,
        0,
        Some(&mut work as *mut _ as *mut _),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
    .is_ok()
    {
        sw = work.right - work.left;
        sh = work.bottom - work.top;
        ox = work.left;
        oy = work.top;
    }

    // 再兜一层：万一窗口比工作区还大（极端小屏），至少把左上角顶到工作区起点，
    // 保证标题栏和关闭按钮始终可见、可点。
    let x = ox + ((sw - w) / 2).max(0);
    let y = oy + ((sh - h) / 2).max(0);
    let _ = SetWindowPos(hwnd, None, x, y, w, h, SWP_NOZORDER);
}

/// 主控台窗口过程。
unsafe extern "system" fn console_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            // lparam 是 CREATESTRUCTW 指针，其 lpCreateParams 存着我们传入的 Config 指针。
            let cs = lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW;
            let cfg_ptr = (*cs).lpCreateParams as *const Config;
            let cfg = if cfg_ptr.is_null() {
                Config::default()
            } else {
                (*cfg_ptr).clone()
            };
            build_controls(hwnd, &cfg);
            LRESULT(0)
        }
        WM_PAINT => {
            draw_console_background(hwnd);
            LRESULT(0)
        }
        // Static（说明文字）控件默认会给自己刷一层系统灰底，
        // 在我们这套浅色界面里看起来就像一条条灰色胶带，很突兀。
        // 这里把文字背景设成透明，并返回和**卡片同色（白）**的画刷，
        // 让它彻底融进背景（注意不能再用页面底色，否则卡片里的文字会露出灰底）。
        WM_CTLCOLORSTATIC => {
            let hdc = HDC(wparam.0 as *mut _);
            let ctrl = HWND(lparam.0 as *mut _);
            let _ = SetBkMode(hdc, TRANSPARENT);

            // 判断这一句是不是「主密码当前状态」那一行（A2 banner 化）。
            let is_status = GetDlgItem(Some(hwnd), IDC_PWD_STATUS)
                .map(|h| h == ctrl)
                .unwrap_or(false);

            // 【A2 状态行 banner】状态行不再是"一行会变色的字"，而是一条
            // 有底色、有左侧竖条的 banner。分工是：
            // - 父窗口画圆角底 + 左侧 3px 竖条（见 draw_console_background）；
            // - Static 自己把**自己那块矩形**刷成同色底、画上文字（这里）。
            // 因为窗口带 WS_CLIPCHILDREN，父窗口画不到 Static 底下，
            // 两边必须同色才能无缝拼在一起。
            if is_status {
                let tone = PWD_STATUS_TONE.with(|t| t.get());
                // (底色, 文字色) 三态：淡琥珀警示 / 淡绿成功 / 卡片白普通。
                // 普通态底色=卡片白，banner 就"隐身"在卡片里——
                // 已正常设置密码的平静状态不需要任何视觉噪音。
                let (bg, fg) = match tone {
                    TONE_WARN => (Palette::WARN_BG, Palette::WARN),
                    TONE_SUCCESS => (Palette::SUCCESS_BG, Palette::SUCCESS),
                    _ => (Palette::CARD, Palette::TEXT_SUB),
                };
                // 【为什么这里要改回不透明】上面统一设了 TRANSPARENT（其它说明
                // 文字靠它融进白卡片）。但 banner 需要 Static **自己刷满底色**，
                // 透明就不刷了、会露出底下的残影——所以状态行单独切回 OPAQUE，
                // 用下面返回的画刷把矩形刷成 banner 同色。
                let _ = SetBkMode(hdc, OPAQUE);
                let _ = SetBkColor(hdc, COLORREF(bg));
                let _ = SetTextColor(hdc, COLORREF(fg));
                return LRESULT(solid_brush(bg).0 as isize);
            }

            // 总开关右侧那句提示是"更弱"的信息（看了才懂的补充说明），
            // 用三级灰 TEXT_MUTED，避免和左边的勾选框抢注意力。
            let is_muted = GetDlgItem(Some(hwnd), IDC_GUARD_HINT)
                .map(|h| h == ctrl)
                .unwrap_or(false);

            // 两种文字色：弱提示浅灰 > 普通说明次要灰（警示态已在上面处理完）。
            let color = if is_muted {
                Palette::TEXT_MUTED
            } else {
                Palette::TEXT_SUB
            };
            let _ = SetTextColor(hdc, COLORREF(color));
            // 返回卡片白画刷：说明文字全放在白色卡片里，返回页面底色会让
            // 文字周围露出灰块，看起来像贴了一条胶带。
            LRESULT(solid_brush(Palette::CARD).0 as isize)
        }
        // Edit（输入框）的背景色也由父窗口决定。
        //
        // 【为什么需要这一条】两个密码框已经去掉了 `WS_EX_CLIENTEDGE`，
        // 它们现在只剩"一块纯文字区域"。如果不管，系统会用默认的灰白底
        // （甚至白色 3D 底），和卡片白、以及我们在下面画的圆角外框都对不上。
        // 这里统一设成卡片白 + 正文色，输入框就和卡片彻底融成一体，
        // 只剩父窗口画的那圈圆角描边能看见。
        WM_CTLCOLOREDIT => {
            let hdc = HDC(wparam.0 as *mut _);
            let _ = SetBkColor(hdc, COLORREF(Palette::CARD));
            let _ = SetTextColor(hdc, COLORREF(Palette::TEXT));
            LRESULT(solid_brush(Palette::CARD).0 as isize)
        }
        // 无边框窗口的拖动 + 右上角自绘关闭按钮
        WM_LBUTTONDOWN => {
            // lParam 低 16 位是 x、高 16 位是 y（客户区坐标）。
            let x = (lparam.0 & 0xFFFF) as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);

            // ⓪ 顶栏最右侧的三个窗口按钮：最小化 / 最大化(还原) / 关闭。
            //
            // 【为什么放在入口判断之前】按钮区在最右边，入口在它左边，
            // 两块矩形本来不重叠；先判按钮纯粹是让"按下即生效"的语义更直白。
            // 和系统标题栏一致：这三个动作按下立即响应，不走"松开才触发"。
            let btns = window_btn_rects(rc.right);
            if point_in(btns[0], x, y) {
                // 最小化：收进任务栏（WS_EX_APPWINDOW 保证任务栏有按钮，点回即还原）。
                let _ = ShowWindow(hwnd, SW_MINIMIZE);
                return LRESULT(0);
            }
            if point_in(btns[1], x, y) {
                toggle_maximize(hwnd);
                return LRESULT(0);
            }
            if point_in(btns[2], x, y) {
                // 关闭：走统一的关窗流程（含"改了没保存"提醒）。
                request_close(hwnd);
                return LRESULT(0);
            }

            // ① 顶栏右侧的三个入口：GitHub / 作者 / 检查更新。
            //
            //    【按下只记账，不立即动作】标准按钮语义是"按下 → 松开时鼠标仍落在
            //    同一入口内 → 才触发"。SetCapture 把鼠标消息抓到本窗口：按住移到
            //    窗口外也能收到 WM_LBUTTONUP，才能在松开时判断"还在不在这个入口里"。
            let links = header_link_rects(rc.right);
            if point_in(links[1], x, y) {
                HEADER_PRESSED.with(|p| p.set(2)); // 2 = 作者入口
                let _ = SetCapture(hwnd);
                invalidate_header(hwnd); // 顶栏立刻重绘，画出按下态
                return LRESULT(0);
            }
            if point_in(links[0], x, y) {
                HEADER_PRESSED.with(|p| p.set(1)); // 1 = GitHub 入口
                let _ = SetCapture(hwnd);
                invalidate_header(hwnd);
                return LRESULT(0);
            }
            if point_in(links[2], x, y) {
                HEADER_PRESSED.with(|p| p.set(3)); // 3 = 检查更新入口
                let _ = SetCapture(hwnd);
                invalidate_header(hwnd);
                return LRESULT(0);
            }

            // ③ 只有顶部标题栏区域能拖动窗口。
            //    以前是"整个客户区都能拖"，结果在卡片空白处按住一划，
            //    整个窗口跟着跑，很容易误操作。
            if y < sc(84) {
                let _ = ReleaseCapture();
                let _ = SendMessageW(
                    hwnd,
                    WM_NCLBUTTONDOWN,
                    Some(WPARAM(HTCAPTION as usize)),
                    Some(LPARAM(0)),
                );
            }
            LRESULT(0)
        }
        // ---- 顶栏入口的"松开"处理：标准按钮语义的后半段 ----
        //
        // 按下（WM_LBUTTONDOWN）只记账，真正执行动作在这里：
        // 松开时鼠标**仍落在同一个入口内** → 才执行原动作；
        // 按住移出去再松开 = 取消，什么都不发生（和系统按钮一致）。
        WM_LBUTTONUP => {
            // 先解除鼠标捕获，把输入派发恢复正常（没按顶栏时调用它也无害）。
            let _ = ReleaseCapture();
            // 取出按下状态并立刻清零（Cell::replace = 读旧值 + 写新值一步完成）。
            let n = HEADER_PRESSED.with(|p| p.replace(0));
            if n != 0 {
                // 鼠标坐标：lparam 低 16 位 x、高 16 位 y（与 WM_LBUTTONDOWN 同一套解法；
                // 超出客户区时值会"绕回"成很大的数，落在入口矩形外即可——正好等于取消）。
                let x = (lparam.0 & 0xFFFF) as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let links = header_link_rects(rc.right);
                let inside = match n {
                    1 => point_in(links[0], x, y),
                    2 => point_in(links[1], x, y),
                    _ => point_in(links[2], x, y), // 3 = 检查更新
                };
                if inside {
                    // 动作和原来"按下时立即执行"的完全一致，只是搬到了松开这一步：
                    // n = 1 → 打开 GitHub 项目页；n = 2 → 作者卡片；
                    // n = 3 → 后台线程查 GitHub 最新 release（结果经 WM_UPDATE_RESULT 回来）。
                    match n {
                        1 => about::open_url(about::GITHUB_URL),
                        2 => about::open(hwnd),
                        _ => crate::update::spawn_check(hwnd),
                    }
                }
                // 不论最终有没有触发，都要重绘顶栏清掉按下态高亮（否则高亮会卡住）。
                invalidate_header(hwnd);
            }
            LRESULT(0)
        }
        // 自绘按钮 / 自绘复选框的绘制请求全在这里处理。
        //
        // 【原理】控件带 BS_OWNERDRAW 样式时，系统不再自己画，
        // 而是把「画什么、画在哪、什么状态」打包成 DRAWITEMSTRUCT 发给父窗口。
        // 返回 1 表示"我已经画好了"。
        WM_DRAWITEM => {
            let dis = lparam.0 as *const DRAWITEMSTRUCT;
            if !dis.is_null() {
                draw_owner_item(&*dis);
            }
            LRESULT(1)
        }
        WM_NCHITTEST => {
            // 默认返回 HTCLIENT，交由上面的 WM_LBUTTONDOWN 处理拖动。
            LRESULT(1) // HTCLIENT
        }
        // ---- 顶栏入口的悬停反馈 ----
        //
        // 顶栏没有子控件，"这里能点"这件事只能靠父窗口自己表达：
        // 鼠标进入入口时换品牌色 + 极淡胶囊底，离开时恢复。
        // 只让顶栏那一条区域失效重绘（而不是整窗），避免大面积闪烁。
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let links = header_link_rects(rc.right);
            let btns = window_btn_rects(rc.right);
            // 悬停编码：1=GitHub 2=作者 3=检查更新 4=最小化 5=最大化 6=关闭。
            let hover = if point_in(links[1], x, y) {
                2
            } else if point_in(links[0], x, y) {
                1
            } else if point_in(links[2], x, y) {
                3
            } else if point_in(btns[0], x, y) {
                4
            } else if point_in(btns[1], x, y) {
                5
            } else if point_in(btns[2], x, y) {
                6
            } else {
                0
            };
            if hover != HEADER_HOVER.with(|h| h.get()) {
                HEADER_HOVER.with(|h| h.set(hover));
                let header = RECT {
                    left: 0,
                    top: 0,
                    right: rc.right,
                    bottom: sc(84),
                };
                let _ = InvalidateRect(Some(hwnd), Some(&header), false);
            }
            // 登记"鼠标离开窗口"的通知：不登记的话，鼠标直接移出窗口时
            // 收不到 WM_MOUSELEAVE，高亮会一直留着，像卡死了一样。
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            LRESULT(0)
        }
        // 鼠标彻底离开窗口：清掉顶栏入口的高亮状态。
        x if x == WM_MOUSELEAVE => {
            if HEADER_HOVER.with(|h| h.get()) != 0 {
                HEADER_HOVER.with(|h| h.set(0));
                let mut rc = RECT::default();
                let _ = GetClientRect(hwnd, &mut rc);
                let header = RECT {
                    left: 0,
                    top: 0,
                    right: rc.right,
                    bottom: sc(84),
                };
                let _ = InvalidateRect(Some(hwnd), Some(&header), false);
            }
            LRESULT(0)
        }
        // 检查更新的后台线程把结果投回这里（lParam 是堆上 Outcome 的指针）。
        // 拆箱后按结果分发：Toast / 「发现新版本」卡片，见 update.rs。
        x if x == crate::update::WM_UPDATE_RESULT => {
            crate::update::handle_result(hwnd, lparam);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            let code = ((wparam.0 >> 16) & 0xFFFF) as u16;

            // ---- Edit 的焦点通知（不是"点击"，必须放在 code == 0 的判断之前）----
            //
            // EN_SETFOCUS  = 0x0100：输入框拿到键盘焦点
            // EN_KILLFOCUS = 0x0200：输入框失去键盘焦点
            //
            // 【为什么要监听】密码框的系统边框已经被去掉了，圆角外框是父窗口
            // 在 WM_PAINT 里画的，父窗口无法自己知道焦点在哪个框里。
            // 这里记录"当前聚焦的是哪个框"，然后让父窗口重绘——
            // 聚焦的框会画出品牌色 2px 描边（网页里的 focus ring），一眼可见。
            if id == IDC_PWD || id == IDC_PWD2 {
                if code == 0x0100 {
                    FOCUSED_PWD.with(|f| f.set(if id == IDC_PWD { 1 } else { 2 }));
                    // 第二个参数 false = 不擦背景：父窗口的 WM_PAINT 会铺满页面底色，
                    // 再擦一次反而会闪一下。
                    let _ = InvalidateRect(Some(hwnd), None, false);
                } else if code == 0x0200 {
                    FOCUSED_PWD.with(|f| f.set(0));
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                // EN_CHANGE = 0x0300：内容变了（每敲一个字符都会发来）。
                // ★ 实时刷新下方的引导文案——用户刚在「新密码」框输入时，
                //   提示行立刻变成"请在确认框再输一遍"，而不是等点「保存」才弹窗。
                //   （顺带覆盖了「显示密码」切换明文时的 WM_SETTEXT，它也会触发 EN_CHANGE。）
                else if code == 0x0300 {
                    update_pwd_status(hwnd);
                }
            }

            // ---- Esc / Enter（由消息循环里的 IsDialogMessageW 翻译而来）----
            // Esc → IDCANCEL：和点「取消」完全一样（改了没保存会先问一句）；
            // Enter → IDOK：和点「保存」完全一样（方案 D：密码框回车即保存）。
            // 用 .0 取底层 i32 再比较（IDOK/IDCANCEL 是 MESSAGEBOX_RESULT 类型）。
            if code == 0 && id == IDCANCEL.0 {
                request_close(hwnd);
                return LRESULT(0);
            }
            if code == 0 && id == IDOK.0 {
                // Enter → 和点「保存」完全一样，走统一的 begin_save：
                // 先立防连点门闩、按钮变「保存中…」，200ms 后由 WM_TIMER 真正保存。
                begin_save(hwnd);
                return LRESULT(0);
            }

            // 只处理按钮点击（BN_CLICKED = 0）
            if code == 0 {
                match id {
                    IDC_ADD => on_add_app(hwnd),
                    IDC_REMOVE => on_remove_selected(hwnd),
                    // 「保存」不直接调 on_save，先进 begin_save 走防连点流程
                    // （门闩 + 按钮禁用 + 200ms 定时器，详见 begin_save 的说明）。
                    IDC_SAVE => begin_save(hwnd),
                    // 「取消」也走统一的关窗前检查：改了没保存会先问一句
                    IDC_CANCEL => request_close(hwnd),
                    // 两个自绘复选框：必须手动翻转勾选状态（见函数内说明）
                    IDC_GUARD | IDC_AUTOSTART => toggle_owner_checkbox(hwnd, id),
                    // 「显示密码」：翻转状态 + 让两个密码框立即切换明/暗
                    IDC_SHOW_PWD => toggle_show_password(hwnd),
                    // 认证方式分段控件：点哪个片段就把选中项切到哪边
                    IDC_AUTH_SYS => set_auth_method(hwnd, false),
                    IDC_AUTH_PWD => set_auth_method(hwnd, true),
                    _ => {}
                }
            }
            LRESULT(0)
        }
        // ---- 「保存」防连点定时器（见 begin_save）----
        //
        // 到点后的顺序很重要：先恢复按钮、再执行保存——
        // - on_save 校验失败（密码不一致等）会弹警告并 return，此时按钮已经恢复
        //   可点，用户改完能立刻重试，不会卡死在「保存中…」；
        // - on_save 成功会 DestroyWindow：定时器在这之前已经 KillTimer，
        //   不会往已销毁的窗口再发消息，也没有定时器泄漏。
        WM_TIMER => {
            if wparam.0 == SAVE_TIMER_ID {
                let _ = KillTimer(Some(hwnd), SAVE_TIMER_ID);
                SAVING.with(|s| s.set(false));
                // 按钮恢复：重新启用 + 文案改回「保存」（与创建时的 save_text 一致）。
                if let Ok(btn) = GetDlgItem(Some(hwnd), IDC_SAVE) {
                    let _ = EnableWindow(btn, true);
                    let save_text = wide("保存");
                    let _ = SetWindowTextW(btn, PCWSTR(save_text.as_ptr()));
                    let _ = InvalidateRect(Some(btn), None, false);
                }
                // 真正执行保存（到这里门闩已放行，即使 on_save 内部再慢也没关系）。
                on_save(hwnd);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            // 点右上角 X / Alt+F4 都会先发 WM_CLOSE。
            // 在这里拦下做"未保存检查"，而不是让默认处理直接销毁窗口。
            request_close(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            // 退出消息循环
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ============================================================================
// 自绘控件
// ============================================================================
//
// 实色填充、圆角卡片、居中文字、圆角按钮、手绘对勾这些"画布基础件"
// 全部搬到了 crate::ui（解锁弹窗也用同一套，避免两份实现慢慢跑偏）。
// 这里只保留"主控台特有的摆位与配色组合"。

/// 自绘按钮 / 复选框（带 `BS_OWNERDRAW` 样式的控件不会自己画自己）。
///
/// # 为什么要自绘
/// 系统原生按钮是灰底 + 3D 边框，和这套浅色卡片界面放在一起非常违和。
/// 自绘之后：主按钮是品牌色实心块、次要按钮是白底细描边、
/// 复选框是圆角小方块 + 手绘对勾，整体视觉才统一。
unsafe fn draw_owner_item(dis: &DRAWITEMSTRUCT) {
    let hdc = dis.hDC;
    let rc = dis.rcItem;
    let _ = SetBkMode(hdc, TRANSPARENT);

    // ODS_SELECTED 表示"控件正被按住"，用它把按钮压深一号，给点击一个即时反馈。
    let pressed = (dis.itemState.0 & ODS_SELECTED.0) != 0;
    // ODS_FOCUS 表示"这个自绘控件当前持有键盘焦点"（Tab 轮到它 / 鼠标点它时由系统置位）。
    // 自绘控件系统不会替我们画焦点框，只能在函数末尾统一补一圈焦点环——
    // 否则纯键盘用户完全看不出焦点停在哪个控件上（网页里的 focus ring 概念）。
    let focused = (dis.itemState.0 & ODS_FOCUS.0) != 0;

    // 自绘时 DC 里的字体不一定是我们想要的，显式选一次（见 UI_FONT 的说明）。
    let font_raw = UI_FONT.with(|f| f.get());
    if font_raw != 0 {
        let font = HFONT(font_raw as *mut _);
        let _ = SelectObject(hdc, font.into());
    }

    // 焦点环的圆角半径必须跟着控件形状走，先给个默认值，进各分支时按控件改：
    // 复选框（小方块）→ 8；四类按钮/分段胶囊 → 控件圆角直径 20，环 = 20−2 = 18。
    // 取值思路 = 控件自身圆角 − 2px（环整体内缩 2px 后与控件轮廓平行）。
    let mut ring_radius = sc(10);
    match dis.CtlID as i32 {
        // ---------- 复选框 ----------
        IDC_GUARD | IDC_AUTOSTART | IDC_SHOW_PWD => {
            // 焦点环贴合 18px 勾选方框的小圆角
            ring_radius = sc(8);
            // 先用卡片白铺底，盖住父窗口可能留下的底色（自绘控件不会自动擦背景）。
            fill_solid(hdc, rc, Palette::CARD);

            // 注意：变量不能叫 `box`——它是 Rust 的保留字（旧的装箱语法），
            // 用了会报 "expected expression, found /"。这里改叫 box_size。
            let box_size = sc(18);
            let bx = rc.left + sc(2);
            let by = rc.top + (rc.bottom - rc.top - box_size) / 2;

            // 方框 + 对勾交给共用函数画（见 ui.rs::draw_check_box）。
            // 这样一来，主控台和解锁弹窗的勾选框形状、粗细、对勾角度完全一致。
            // 勾选状态从我们自己的记账里读（原因见 GUARD_CHECKED 的说明）。
            draw_check_box(hdc, bx, by, box_size, check_state(dis.CtlID as i32), sc(5));

            // 文字跟在方框右侧，垂直居中
            let color = if pressed { Palette::BRAND } else { Palette::TEXT };
            let text = window_text(dis.hwndItem);
            draw_text_left(
                hdc,
                &text,
                RECT {
                    left: bx + box_size + sc(10),
                    right: rc.right,
                    top: rc.top,
                    bottom: rc.bottom,
                },
                color,
            );
        }
        // ---------- 主按钮：保存（页面底色上，品牌色实心）----------
        IDC_SAVE => {
            // 保存按钮是 40 高的胶囊（圆角 20），环内缩 2px → 18
            ring_radius = sc(18);
            let (bg, fg) = if pressed {
                (Palette::BRAND_DARK, Palette::ON_BRAND)
            } else {
                (Palette::BRAND, Palette::ON_BRAND)
            };
            draw_button(
                hdc,
                rc,
                // 保存/取消按钮在两张卡片之外，底下是页面底色
                Palette::BG,
                bg,
                fg,
                &window_text(dis.hwndItem),
                None,
                // 圆角 20（RoundRect 的"椭圆直径"）：40 高的按钮得到明显的胶囊感，
                // 但四个角又不会像正圆那样吃掉文字附近的空间。
                sc(20),
            );
        }
        // ---------- 次要按钮：添加 / 移除（卡片内，白底细描边）----------
        IDC_ADD | IDC_REMOVE => {
            // 按钮圆角统一为直径 20（视觉半径 10，现代化批次），环内缩 2px → 18
            ring_radius = sc(18);
            // 「移除」的两段式确认态（方案 D · 危险操作）：
            // 处于待确认时画成淡红底 + 红字 + 红描边——整颗按钮变成
            // "这是危险操作"的信号，比灰色按钮上换一行字醒目得多。
            let armed =
                dis.CtlID as i32 == IDC_REMOVE && REMOVE_ARMED.with(|a| a.get());
            let (bg, fg, border) = if armed {
                (Palette::DANGER_BG, Palette::DANGER, Palette::DANGER)
            } else if pressed {
                (
                    Palette::HOVER,
                    Palette::TEXT,
                    Palette::BORDER_STRONG,
                )
            } else {
                (Palette::CARD, Palette::TEXT, Palette::BORDER_STRONG)
            };
            draw_button(
                hdc,
                rc,
                Palette::CARD,
                bg,
                fg,
                &window_text(dis.hwndItem),
                // BORDER_STRONG（比 BORDER 更实一点）：这两个按钮是"操作入口"，
                // 边界要看得清，否则在白卡片上会像两行普通文字。
                Some(border),
                sc(20),
            );
        }
        // ---------- 次要按钮：取消（页面底色上，白底细描边）----------
        IDC_CANCEL => {
            // 和保存按钮同尺寸（圆角 20 → 环 18），两个底部按钮的焦点环才对称
            ring_radius = sc(18);
            let bg = if pressed { Palette::HOVER } else { Palette::CARD };
            draw_button(
                hdc,
                rc,
                Palette::BG,
                bg,
                Palette::TEXT,
                &window_text(dis.hwndItem),
                Some(Palette::BORDER_STRONG),
                sc(20),
            );
        }
        // ---------- 认证方式分段控件（两个片段）----------
        IDC_AUTH_SYS | IDC_AUTH_PWD => {
            // 片段是贴合轨道的胶囊形；片段圆角直径 20，环半径 = 20 − 2 = 18
            ring_radius = sc(18);
            // 先用轨道色铺满整个片段矩形。
            //
            // 【为什么必须先铺一层】`draw_segment` 用圆角矩形填色，四个角
            // 是留空的；自绘控件又不会自动擦背景，不铺底的话那四个角会
            // 露出上一次绘制残留的像素（闪一下、或者出现杂点）。
            // 铺成 TRACK 之后：未选中的片段和底下的轨道同色 → 完全无缝；
            // 选中的片段是白色胶囊，四角露出一点轨道灰 → 正好是"浮在轨道上"的效果。
            fill_solid(hdc, rc, Palette::TRACK);

            let sel_password = AUTH_SEL.with(|s| s.get());
            let selected = if dis.CtlID as i32 == IDC_AUTH_PWD {
                sel_password
            } else {
                // "系统验证"片段：自设密码没选中时它才是选中态
                !sel_password
            };
            draw_segment(
                hdc,
                rc,
                selected,
                // hovered 固定传 false：这两个片段本来就在灰色轨道上，
                // 再叠一层悬停色反而会让人以为"已经选了"，得不偿失。
                false,
                &window_text(dis.hwndItem),
                sc(20),
            );
        }
        _ => {}
    }

    // ---- 焦点环：放在所有分支之后的统一收尾处 ----
    //
    // 【为什么放这里】各分支负责画控件本身，画环只关心"有没有焦点"和"圆角多大"
    // 两件事，且每个分支都没有提前 return，函数走到这里时环必然画在控件**之上**。
    // 分散写在每个分支里的话，以后新加一种自绘控件很容易忘记补环。
    if focused {
        // 内缩 2px 再描边：环浮在控件自身边框外面一圈，不会和边框重叠糊在一起
        //（等价网页里 outline 不占布局的 focus ring）。
        let ring_rc = RECT {
            left: rc.left + 2,
            top: rc.top + 2,
            right: rc.right - 2,
            bottom: rc.bottom - 2,
        };
        // IDC_SAVE 是品牌色实心按钮，品牌色环在上面根本看不见 → 改用白色环；
        // 其余控件底色都是白/浅灰，用品牌色环最醒目。
        let ring_color = if dis.CtlID as i32 == IDC_SAVE {
            Palette::ON_BRAND
        } else {
            Palette::BRAND
        };
        // 2px 线宽：再细在高 DPI 下会断，再粗就抢控件本身的视觉了。
        draw_round_border(hdc, ring_rc, ring_color, ring_radius, 2);
    }
}

/// 自绘复选框的点击处理：手动翻转勾选状态并请求重绘。
///
/// # 为什么必须手动翻转
/// `BS_AUTOCHECKBOX`（自动复选框）由系统负责切换勾选状态；
/// 换成 `BS_OWNERDRAW` 之后，系统只通知"被点了"（BN_CLICKED），
/// 不再替我们改状态。不手动翻转的话，用户怎么点勾选框都毫无反应。
unsafe fn toggle_owner_checkbox(hwnd: HWND, id: i32) {
    let Ok(btn) = GetDlgItem(Some(hwnd), id) else {
        return;
    };
    // 状态存我们自己的变量里（原因见 GUARD_CHECKED 的说明）
    set_check_state(id, !check_state(id));
    // 状态变了要重绘，否则界面上勾选框看起来还是原来的样子。
    let _ = InvalidateRect(Some(btn), None, true);
}

/// 切换两个密码框的「明文 / 圆点」显示。
///
/// # 为什么用 EM_SETPASSWORDCHAR 而不是重建控件
/// Edit 的 `ES_PASSWORD` 只是一个初始样式，运行时可以发 `EM_SETPASSWORDCHAR`
/// 指定"用哪个字符遮住原文"：
/// - 传 0 → 不遮了，直接显示明文，用户可以核对有没有打错；
/// - 传 `*`（0x2A）→ 恢复成 Windows 密码框默认的星号遮罩。
///
/// 整个过程不重建控件，所以已经输入的内容、光标位置都不会丢。
unsafe fn toggle_show_password(hwnd: HWND) {
    // EM_SETPASSWORDCHAR 的消息号（winuser.h 常量，windows crate 未单独封装）。
    const EM_SETPASSWORDCHAR: u32 = 0x00CC;
    // 0x2A = '*'，就是 Windows 密码框默认的遮罩字符（实测显示为星号），
    // 恢复时写回同一个字符，切换前后外观才一致。
    const MASK: usize = 0x2A;

    // 翻转勾选状态（自绘控件必须自己记账，见 SHOW_PWD_CHECKED 的说明）
    let shown = !check_state(IDC_SHOW_PWD);
    set_check_state(IDC_SHOW_PWD, shown);

    let mask = if shown { 0 } else { MASK };
    for id in [IDC_PWD, IDC_PWD2] {
        if let Ok(edit) = GetDlgItem(Some(hwnd), id) {
            if !edit.0.is_null() {
                let _ = SendMessageW(
                    edit,
                    EM_SETPASSWORDCHAR,
                    Some(WPARAM(mask)),
                    Some(LPARAM(0)),
                );
                // 立刻重绘：点下去马上能看到明文/圆点，不用等下一次按键。
                let _ = InvalidateRect(Some(edit), None, true);
            }
        }
    }

    // 勾选方块本身也要重画（否则界面上还是原来的样子）。
    if let Ok(btn) = GetDlgItem(Some(hwnd), IDC_SHOW_PWD) {
        let _ = InvalidateRect(Some(btn), None, true);
    }
}

/// 认证方式各选项的中文解释，显示在分段控件右侧。
///
/// 光看「系统验证」「自设密码」两个词判断不出实际解锁行为，必须把
/// 每种方式的验证流程讲清楚（尤其是密码模式会怎么"忘记密码"）。
fn auth_hint_text(index: usize) -> &'static str {
    match index {
        // ★ 这两句都刻意压到 22 个字以内：
        //   早前的文案太长，被控件右边界直接截断成了半句话（"…忘"），
        //   看起来像界面出了故障。现在保证在任何 DPI 下都完整显示。
        1 => "解锁：输入自设密码；忘记可用 Windows 身份找回",
        _ => "解锁：Windows Hello（指纹 / 人脸 / PIN）",
    }
}

/// 认证方式分段控件被点击后：切换选中项、刷新说明文字、重画。
///
/// # 参数
/// - `use_password`：true = 选「自设密码」，false = 选「系统验证」。
unsafe fn set_auth_method(hwnd: HWND, use_password: bool) {
    AUTH_SEL.with(|s| s.set(use_password));
    update_auth_hint(hwnd);

    // 两个片段的外观都变了（一个是"选中→未选中"、另一个相反），必须各自重绘。
    // 【为什么不能只刷父窗口】父窗口带 WS_CLIPCHILDREN，重绘时会**跳过**
    // 子控件占用的矩形；只刷父窗口的话，两个胶囊会保持旧样子。
    for id in [IDC_AUTH_SYS, IDC_AUTH_PWD] {
        if let Ok(btn) = GetDlgItem(Some(hwnd), id) {
            let _ = InvalidateRect(Some(btn), None, true);
        }
    }
    // 父窗口也刷一遍：说明文字位置、以及片段周围的轨道都在父窗口的图层上。
    let _ = InvalidateRect(Some(hwnd), None, true);
}

/// 按当前选中项刷新分段控件右侧那行说明文字。
///
/// 【为什么不直接用控件的勾选状态】分段控件是自绘按钮，没有"选中"语义，
/// 真正的选中项存在 AUTH_SEL 里（见该变量的说明）。
unsafe fn update_auth_hint(hwnd: HWND) {
    let Ok(hint) = GetDlgItem(Some(hwnd), IDC_AUTH_HINT) else {
        return;
    };
    if hint.0.is_null() {
        return;
    }
    let index = if AUTH_SEL.with(|s| s.get()) { 1 } else { 0 };
    let text = wide(auth_hint_text(index));
    let _ = SetWindowTextW(hint, PCWSTR(text.as_ptr()));
}

/// 当前认证方式是否选在「自设密码」。
///
/// 保存配置、保存前校验、以及关窗脏检查都用这一个判断，
/// 保证三处对"用户到底选了哪种方式"的理解完全一致。
fn auth_is_password() -> bool {
    AUTH_SEL.with(|s| s.get())
}

/// 创建主控台里的所有子控件，并用配置填充初始值。
unsafe fn build_controls(hwnd: HWND, cfg: &Config) {
    let hinstance = GetModuleHandleW(None).unwrap();
    // 三档字号，全部按 DPI 缩放。
    //
    // 【为什么用负数】
    // CreateFontW 的 nHeight 参数：正数表示「字符单元格高度」（含字体内部行距），
    // 负数表示「字符本身的高度」。用正数时实际字形只有设定值的 70% 左右，
    // 所以以前传 16 看起来像 12 号字，偏小。改成负数后视觉大小和数字对得上。
    //
    // font  ：控件正文（按钮、列表、输入框），16px 逻辑字号
    // small ：说明文字，14px，比正文小一号但依然清晰
    // （标题、小节标题的字体在 draw_console_background 里单独创建）
    //
    // 【为什么从 15/13 升到 16/14】"轻量现代化"批次的诊断：字号偏小是
    // 界面显旧的第一来源。16px 正文 + 14px 说明仍保持两级层次，
    // 但整屏的阅读压力明显下降；配套的行高 / 控件高度也同步加高。
    let font = make_font(-sc(16), FW_NORMAL.0 as i32);
    let small = make_font(-sc(14), FW_NORMAL.0 as i32);

    // 把正文字体句柄记到线程局部变量里：自绘按钮/复选框时要用它，
    // 否则那些控件会用系统默认字体，和列表、输入框的字号对不上。
    UI_FONT.with(|f| f.set(font.0 as isize));

    // ---------- 自绘应用列表（替代系统 ListView）----------
    //
    // 【为什么不再用 ListView】SysListView32 是 Windows 95 时代的设计：
    // 灰白 3D 表头、系统滚动条、白色直角边框。不管怎么设主题，
    // 它放在这套圆角卡片里都像一张"报表"。这里换成 applist.rs 里的自绘控件：
    // 没有表头、没有网格，行高 64（宽松），勾选框和界面其它勾选框完全一致。
    //
    // 参数说明：x/y/w/h 已经是物理像素，所以全部过一遍 sc()；
    // 最后两个是主字体（16px）和副字体（14px），保证列表文字与其它控件一致。
    //
    // 【视口高度 264 的来历】行高 64 × 4 行 + 上下各 4px 内边距 = 264，
    // 一眼能看到 4 行；早先压成 2 行（120px）是为了卡片紧凑，但实测"只露两行"
    // 让人意识不到下面还有内容。现在窗口随之加高，4 行更符合列表的浏览直觉，
    // 超过 4 行的内容仍由滚轮翻页，控件右侧会画一条细指示条提示"下面还有"。
    // 水平方向：卡片左边距 24 + 卡片内边距 24 = 48；
    // 宽 664 = 760（窗口）- 2×24（卡片外边距）- 2×24（卡片内边距）。
    let list = applist::create(
        hwnd,
        sc(48),
        sc(166),
        sc(664),
        sc(264),
        dpi(),
        font,
        small,
    );

    // 清空上一次打开窗口时残留的行，再按配置重新灌一遍。
    // 【为什么必须先 clear】行数据存在 applist.rs 的线程局部变量里，
    // 不清理的话，第二次打开主控台会看到两份重复的列表。
    applist::clear();
    for app in &cfg.apps {
        applist::add_row(&app.display_name, &app.file_name, app.enabled);
    }
    applist::refresh(list);

    // ---------- 按钮：添加 / 移除 ----------
    // 【样式改成 BS_OWNERDRAW】从这一版开始，按钮不再用系统那套灰底 3D 外观，
    // 而是由 draw_owner_item 画成"白底细描边 + 深色文字"的现代按钮。
    let btn_class = wide("Button");
    // 自绘按钮的统一窗口样式（下面每个按钮都复用它，避免抄四遍）
    let owner_btn_style = WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32);

    let add_text = wide("＋ 添加应用");
    // 注意：这里必须接收句柄，否则下面的统一设字体循环会漏掉它，
    // 按钮就会用系统默认小字，和其他控件格格不入。
    //
    // 位置：移到了卡片标题行右侧（对齐卡片内边距），和「受保护的应用」
    // 标题同一条水平线。这是 Stripe / Linear 这类控制台的常见做法：
    // 操作按钮跟着它的对象走，不再单独占一行——省下一整行高度，
    // 也让"这些按钮管的是上面这份列表"这层关系变得一目了然。
    // 卡片范围 (24,100)-(736,468)，标题行 112..152：
    // 按钮高 40（现代化批次：32→40，与保存/取消统一），
    // 右端贴内容区右缘 712：移除 (608,112,104,40)，添加在其左、间隔 12。
    let add_btn = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(btn_class.as_ptr()),
        PCWSTR(add_text.as_ptr()),
        owner_btn_style,
        sc(476),
        sc(112),
        sc(120),
        sc(40),
        Some(hwnd),
        Some(HMENU(IDC_ADD as *mut _)),
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();
    // 文案从「－ 移除选中行」简化成「移除」：长文案是"对话框味"的来源之一。
    // 「勾选框≠选中行」的语义没有丢——它由列表正下方的图注承担（见 list_hint），
    // 两段式确认逻辑（REMOVE_ARMED 记账）完全不变，只是按钮上不再啰嗦。
    // 宽度 104 按最宽的 armed 态文案「确认移除?」（16px 下约 96px）预留。
    let rm_text = wide("移除");
    let remove_btn = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(btn_class.as_ptr()),
        PCWSTR(rm_text.as_ptr()),
        owner_btn_style,
        sc(608),
        sc(112),
        sc(104),
        sc(40),
        Some(hwnd),
        Some(HMENU(IDC_REMOVE as *mut _)),
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();
    // 主控台是可以反复打开的，两段式删除的"待确认"状态必须每次开窗都清零——
    // 否则上次留下的红字态会跟这次新建的默认文字对不上（线程局部变量不会自动重置）。
    REMOVE_ARMED.with(|a| a.set(false));
    REMOVE_ARMED_SEL.with(|s| s.set(-1));

    // ---------- 总开关 / 开机自启 ----------
    // 说明文字和输入框都用系统的 Static / Edit，只是把它们的边框、背景
    // 交给自绘的父窗口处理（见 WM_CTLCOLORSTATIC / WM_CTLCOLOREDIT）。
    let static_class = wide("Static");

    // 按钮右侧的一句说明：把「勾选框」和「选中行」的区别写在界面上。
    // 这行字替代了过去的"点了移除没反应"弹窗——提前说清楚比事后报错好。
    // 位置 (48,438)-(712,458)：列表（底部 430）正下方 8px，当"图注"。
    // 原来它挤在按钮右边，两段文字各说各的；现在按钮上了标题行，
    // 这行说明贴着自己的列表，读起来才有"这段讲的是列表"的从属关系。
    let list_hint_text = wide("勾选框 = 启用 / 停用；点行选中后才能移除");
    let list_hint = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(static_class.as_ptr()),
        PCWSTR(list_hint_text.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        sc(48),
        sc(438),
        sc(664),
        sc(20),
        Some(hwnd),
        None,
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();
    SendMessageW(list_hint, WM_SETFONT, Some(WPARAM(small.0 as usize)), Some(LPARAM(1)));

    // 【为什么只留这一个开关】用户要的产品逻辑很简单：打开软件前验证一次。
    // 所以界面上不再有"免验证多久""切回来要不要重锁"这些遮罩时代的选项，
    // 只留一个勾选框——勾上就拦，取消就完全不拦（等效把锁关掉）。
    // 取消勾选并保存后，HelloGate 会删除注册表里的启动拦截登记（见 ifeo.rs），
    // 受保护程序双击即开，和没装 HelloGate 一模一样。
    //
    // 文案特意压短成「打开软件前验证」：原来那句 20 多个字的长句
    // 会把这一行撑得很挤，和右边的「开机自动启动」互相撞车。
    // 「取消后会发生什么」拆到右边的小字里说（见下面 guard_hint）。
    let guard_text = wide("打开软件前验证");
    // 同样是自绘复选框：方框由我们画，观感才和卡片、按钮统一。
    // 位置 (48,526)-(228,558)：卡片二「验证开关」分组第一行（小节标题下方）。
    // 行高 28→32：16px 文字 + 18px 方框需要更多垂直空间。
    let guard = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(btn_class.as_ptr()),
        PCWSTR(guard_text.as_ptr()),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
        sc(48),
        sc(526),
        sc(180),
        sc(32),
        Some(hwnd),
        Some(HMENU(IDC_GUARD as *mut _)),
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();
    // 初始化勾选状态：存到我们自己的变量里，同时界面首次绘制就会读它
    set_check_state(IDC_GUARD, cfg.guard_enabled);

    // 总开关右侧的淡化提示：说明"取消勾选"的实际后果。
    // 用三级灰（TEXT_MUTED）而不是次要灰，让它明显弱于左边的勾选框文字。
    let guard_hint_text = wide("取消勾选后，这些程序可直接打开");
    // 位置 (236,526)-(496,558)：紧贴 guard 右侧（14px 字下 15 字约 210px，给 260）。
    let guard_hint = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(static_class.as_ptr()),
        PCWSTR(guard_hint_text.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        sc(236),
        sc(526),
        sc(260),
        sc(32),
        Some(hwnd),
        // 给它一个 ID，WM_CTLCOLORSTATIC 才能认出它并单独设成浅灰
        Some(HMENU(IDC_GUARD_HINT as *mut _)),
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();
    SendMessageW(guard_hint, WM_SETFONT, Some(WPARAM(small.0 as usize)), Some(LPARAM(1)));

    let autostart_text = wide("开机自动启动");
    // 位置 (564,526)-(712,558)：右端贴内容区右缘 712，与 guard 同一行。
    let autostart = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(btn_class.as_ptr()),
        PCWSTR(autostart_text.as_ptr()),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
        sc(564),
        sc(526),
        sc(148),
        sc(32),
        Some(hwnd),
        Some(HMENU(IDC_AUTOSTART as *mut _)),
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();
    set_check_state(IDC_AUTOSTART, cfg.autostart);

    // ---------- 认证方式（系统验证 / 自设密码）----------
    //
    // 【为什么给选择】不是每台电脑都能用好 Windows Hello（没摄像头、
    // 提权进程偶尔弹不出来等），有的用户就想用自己定的密码；也有人偏爱
    // 系统验证的便利。两种方式都完整支持，解锁时按这里的配置路由。
    //
    // 【为什么从下拉框改成分段控件】系统 ComboBox 是 Win95 外观（凹陷白框
    // + 灰色小三角），外面怎么美化它都露怯；而且下拉框把"另一个选项"藏起来了，
    // 用户必须点开才知道有什么可选。两个并排胶囊既好看，又让两选项一眼可见。
    let auth_label_text = wide("认证方式");
    // 位置 (48,584)-(112,608)：和右边的分段轨道（576..616，中心 596）垂直居中。
    let auth_label = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(static_class.as_ptr()),
        PCWSTR(auth_label_text.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        sc(48),
        sc(584),
        sc(64),
        sc(24),
        Some(hwnd),
        None,
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();
    SendMessageW(auth_label, WM_SETFONT, Some(WPARAM(small.0 as usize)), Some(LPARAM(1)));

    // 分段控件的外框是一个"灰色轨道"（由父窗口在 WM_PAINT 里画），
    // 两个片段是盖在轨道上的自绘按钮——见 draw_console_background 里的
    // fill_round((120,576,188,40), TRACK, 24)。轨道高 36→40（现代化批次）。
    //
    // 选中项的初始值：配置里写的是 "password" 就用「自设密码」。
    // 这里写进 AUTH_SEL 线程局部变量（自绘按钮没有勾选语义，必须自己记账）。
    let auth_sel = cfg.auth_method == "password";
    AUTH_SEL.with(|s| s.set(auth_sel));

    // 左片段「系统验证」，位置 (122,578)-(214,614)：轨道内缩 2px。
    // 片段宽 86→92：16px 字下"系统验证"四字 64px + 左右各 14px 呼吸位。
    let sys_text = wide("系统验证");
    let auth_sys = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(btn_class.as_ptr()),
        PCWSTR(sys_text.as_ptr()),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
        sc(122),
        sc(578),
        sc(92),
        sc(36),
        Some(hwnd),
        Some(HMENU(IDC_AUTH_SYS as *mut _)),
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();

    // 右片段「自设密码」，位置 (214,578)-(306,614)：与左片段无缝相接，
    // 两片段合计 184 + 轨道两侧内缩 2×2 = 188 = 轨道宽。
    let pwd_mode_text = wide("自设密码");
    let auth_pwd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(btn_class.as_ptr()),
        PCWSTR(pwd_mode_text.as_ptr()),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
        sc(214),
        sc(578),
        sc(92),
        sc(36),
        Some(hwnd),
        Some(HMENU(IDC_AUTH_PWD as *mut _)),
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();

    // 认证方式的动态说明：初始文字按当前选中项写入，之后随分段控件切换
    // 由 set_auth_method → update_auth_hint 刷新。
    // 位置 (324,584)-(712,608)：紧跟在分段轨道（右缘 308）右侧 16px，一行放得下。
    let auth_hint0 = wide(auth_hint_text(if auth_sel { 1 } else { 0 }));
    let auth_hint = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(static_class.as_ptr()),
        PCWSTR(auth_hint0.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        sc(324),
        sc(584),
        sc(388),
        sc(24),
        Some(hwnd),
        Some(HMENU(IDC_AUTH_HINT as *mut _)),
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();
    SendMessageW(auth_hint, WM_SETFONT, Some(WPARAM(small.0 as usize)), Some(LPARAM(1)));

    // ---------- 主密码（自设密码模式的主验证 / Hello 不可用时的回退）----------
    //
    // 【为什么要这个设置】
    // 不是每台电脑都配了 Windows Hello。旧版遇到"没 Hello"直接放行——锁形同虚设。
    // 现在回退到主密码，所以必须给用户一个能设置它的地方。
    // 程序只保存密码的 PBKDF2 派生值（见 password.rs / config.rs），
    // 界面上也绝不回显旧密码——两个框永远从空白开始，留空就表示"不修改"。
    // 【长度自由】3 位、6 位、任意字符都可以——这是用户自己的锁，由用户
    // 自己权衡好记和安全（短密码有防暴力锁定兜底，见 password.rs）。
    // 两个密码框各自带一个小标题。
    //
    // 【为什么用两个标签而不是框内提示（cue banner）】
    // 一开始用的是 EM_SETCUEBANNER（输入框内淡灰色提示），但实测在这套环境里
    // 它总是不显示（该消息对控件生命周期、父窗口可见时机都很敏感，不稳定）。
    // 与其赌一个看不见的提示，不如在框子正上方各写一行小字：
    // 位置固定、任何系统版本都一定看得见，也直接解决了"两个框分不清哪个是哪个"。
    let pwd_label_text = wide("新密码（留空表示不修改）");
    // 位置 (48,624)-(374,646)：紧贴在左边输入框外框（654）的正上方，间距 8。
    let pwd_label = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(static_class.as_ptr()),
        PCWSTR(pwd_label_text.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        sc(48),
        sc(624),
        sc(326),
        sc(22),
        Some(hwnd),
        None,
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();
    SendMessageW(pwd_label, WM_SETFONT, Some(WPARAM(small.0 as usize)), Some(LPARAM(1)));

    // ★ 文案不再只写"确认新密码"四个字——用户实测反馈：在第一个框输入后，
    //   完全看不出旁边那个空框是留给谁的。"（再输一遍）"直接把动作说出来，
    //   再配合输入过程中的实时引导（EN_CHANGE → update_pwd_status），
    //   从"猜"变成"照着做"。
    let pwd2_label_text = wide("确认新密码（再输一遍）");
    // 位置 (386,624)-(606,646)：宽度只给文字够用的 220px（14px 字 11 字约 154px），
    // 右端空出来的位置留给后面的「显示密码」勾选框。
    let pwd2_label = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(static_class.as_ptr()),
        PCWSTR(pwd2_label_text.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        sc(386),
        sc(624),
        sc(220),
        sc(22),
        Some(hwnd),
        None,
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();
    SendMessageW(pwd2_label, WM_SETFONT, Some(WPARAM(small.0 as usize)), Some(LPARAM(1)));

    // 「显示密码」：勾上后两个密码框显示明文，用来核对有没有打错。
    // 【为什么放在标签行右侧】输入框本体只有 44 高，控件里再塞一个图标会挤；
    // 放在"确认新密码"标签同一行的右端，既属于这一组，又不占输入的位置。
    // 【不默认勾选】打开界面时密码框内容本来就是空的，默认隐藏更符合习惯；
    // 需要核对时再手动勾一下，用完可以再取消。
    let show_pwd_text = wide("显示密码");
    // 位置 (616,622)-(712,650)：与 pwd2 标签（624..646）同一行、垂直居中对齐。
    let show_pwd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(btn_class.as_ptr()),
        PCWSTR(show_pwd_text.as_ptr()),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
        sc(616),
        sc(622),
        sc(96),
        sc(28),
        Some(hwnd),
        Some(HMENU(IDC_SHOW_PWD as *mut _)),
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();
    // 每次打开主控台都从"隐藏"开始（线程局部变量会跨窗口保留，必须显式复位）。
    set_check_state(IDC_SHOW_PWD, false);
    SendMessageW(show_pwd, WM_SETFONT, Some(WPARAM(small.0 as usize)), Some(LPARAM(1)));

    // ---------- 两个密码输入框 ----------
    //
    // 【本版最关键的一处"去系统化"】
    // 以前用 `WS_EX_CLIENTEDGE`，Edit 会自己画一圈 Win95 风格的凹陷边框：
    // 深灰外线 + 白色高光内线，放在白色圆角卡片上像一块贴错位置的胶布。
    // 现在把边框完全去掉，Edit 只是一块"会显示文字的白底区域"；
    // 真正的圆角外框由父窗口在 WM_PAINT 里画（见 draw_console_background 中的
    // draw_input_state），这样聚焦时还能像网页一样换成品牌色 focus ring。
    //
    // 【组件化（方案 B1）】以前这里各写各的几何值：
    // 创建控件用 (34,449,312,24)，画外框用 (32,442,316,40)，两边靠人工推导
    // 保持一致——改一处忘了改另一处就是"文字和框错位"。现在**外框矩形是
    // 唯一的事实来源**：ui::create_input 拿它推算 Edit 位置（含垂直居中、
    // 12px 左右内边距、ES_PASSWORD/ES_AUTOHSCROLL 样式），
    // 父窗口画框也用同一个矩形调 ui::draw_input_state，几何再也分不了叉。
    //
    // 【垂直居中】单行 Edit 的文字永远贴着控件顶部画（Win32 没有垂直居中
    // 样式），所以组件把 Edit 缩到一行字高（24px）、在外框里上下居中，
    // 文字自然落在圆角框正中——修掉了"文字下面一块空的"的老问题。
    let pwd_frame = RECT {
        left: sc(48),
        top: sc(654),
        right: sc(48) + sc(326),
        bottom: sc(654) + sc(44),
    };
    let pwd2_frame = RECT {
        left: sc(386),
        top: sc(654),
        right: sc(386) + sc(326),
        bottom: sc(654) + sc(44),
    };
    // 最后一个参数 0 = 右端不预留（本页没有外挂按钮）；
    // dpi() 是主控台自己的缩放基准（密码弹窗传的则是窗口 DPI）。
    let pwd = create_input(hwnd, pwd_frame, dpi(), IDC_PWD, true, font, 0);
    let pwd2 = create_input(hwnd, pwd2_frame, dpi(), IDC_PWD2, true, font, 0);

    // 当前状态提示：让用户一眼知道主密码到底设没设。
    //
    // 【三种状态，三种文案，但一律不显示密码值】
    // 1. 空密码 → 提醒去设；
    // 2. 还是出厂默认状态 → 醒目提示"尚未设置"（琥珀色）；
    // 3. 已设置自己的密码 → 只说明忘记时怎么找回。
    //
    // 【为什么不写具体的默认密码】把初始密码印在设置界面上毫无意义：
    // 能坐到这台电脑前、能看到这个界面的人，自然也就能用那串密码解锁——
    // 等于把钥匙插在锁孔上还挂着牌子。这里只陈述"状态"，
    // 至于初始值，只在安装文档里出现，用户设置完就作废。
    let (pwd_status_str, is_default_pwd) = if cfg.master_password_hash.is_empty()
        || cfg.master_password_salt.is_empty()
    {
        (
            "当前状态：尚未设置主密码 —— 请设置一个自己的密码".to_string(),
            true,
        )
    } else if crate::config::is_default_password(
        &cfg.master_password_hash,
        &cfg.master_password_salt,
    ) {
        (
            "当前状态：尚未设置主密码（出厂初始状态）—— 设置后才会真正拦截".to_string(),
            true,
        )
    } else {
        (
            "当前状态：已设置主密码（忘记时可用 Windows 人脸 / 指纹或账户密码找回）"
                .to_string(),
            false,
        )
    };
    // 把 banner 语气记下来（WM_CTLCOLORSTATIC 与父窗口画 banner 都取用它）：
    // 尚未设置密码 → 琥珀警示语气；已设置 → 普通语气。
    let init_tone = if is_default_pwd { TONE_WARN } else { TONE_NORMAL };
    PWD_STATUS_TONE.with(|t| t.set(init_tone));
    // 兜底语气同时存一份：用户打字时语气会临时变（见 update_pwd_status），
    // 两个框清空后靠它还原。
    PWD_STATUS_BASE_TONE.with(|t| t.set(init_tone));
    // 把这段兜底文案也存一份：用户开始在密码框打字时，这一行会临时切换成
    // 实时引导（见 update_pwd_status / EN_CHANGE），两个框清空后又靠它还原。
    PWD_STATUS_BASE.with(|b| *b.borrow_mut() = pwd_status_str.clone());

    let pwd_status_text = wide(&pwd_status_str);
    // 【A2 banner】Static 内缩在 banner 矩形里：左留 8px 给左侧竖条和呼吸位、
    // 上下各留 6px（banner 高 32 = 20 文字行 + 2×6）。它自己刷的底色
    // （见 WM_CTLCOLORSTATIC）与父窗口画的 banner 同色，无缝拼接成一条横幅。
    let banner = pwd_status_banner_rect();
    let pwd_status = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(static_class.as_ptr()),
        PCWSTR(pwd_status_text.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        banner.left + sc(8),
        banner.top + sc(6),
        (banner.right - banner.left) - sc(16),
        sc(20),
        Some(hwnd),
        // 给这一行一个 ID：窗口过程要靠它认出"这是需要变色的状态行"
        Some(HMENU(IDC_PWD_STATUS as *mut _)),
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();
    SendMessageW(pwd_status, WM_SETFONT, Some(WPARAM(small.0 as usize)), Some(LPARAM(1)));

    // ---------- 保存 / 取消 ----------
    // 保存 = 品牌色实心主按钮；取消 = 白底描边次要按钮（样式在 draw_owner_item 里区分）
    // 位置：卡片二底部（744）往下 24px 起（现代化批次：12→24，底部留白更足），
    // 右端与卡片右边缘（736）对齐；两钮间距 12→16；
    // 宽 108 按 16px 字重算："保存"/"取消" 32px + 左右各 16px 内边距 = 64，
    // armed 态"正在保存…" 64px + 32 = 96 < 108，不会截字。
    let save_text = wide("保存");
    let save = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(btn_class.as_ptr()),
        PCWSTR(save_text.as_ptr()),
        owner_btn_style,
        sc(504),
        sc(768),
        sc(108),
        sc(40),
        Some(hwnd),
        Some(HMENU(IDC_SAVE as *mut _)),
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();
    let cancel_text = wide("取消");
    let cancel = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(btn_class.as_ptr()),
        PCWSTR(cancel_text.as_ptr()),
        owner_btn_style,
        sc(628),
        sc(768),
        sc(108),
        sc(40),
        Some(hwnd),
        Some(HMENU(IDC_CANCEL as *mut _)),
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();

    // 统一设置字体（默认是难看的 System 字体）。
    //
    // 【分两组是有意的】
    // 第一组是"正文控件"（按钮、分段控件、输入框）：用 16px，看得清、点得准；
    // 第二组是"说明文字"（各种提示行）：用 14px，弱化存在感并给主内容让位。
    // 之前把所有控件塞进同一个循环，导致说明文字也被拉到 17px，
    // 那句"解锁：自设密码…"就是被撑宽后撞上控件边界、显示成半句话的。
    for ctrl in [
        add_btn, remove_btn, guard, autostart, auth_sys, auth_pwd, pwd, pwd2, save, cancel,
    ] {
        SendMessageW(ctrl, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
    }
    for ctrl in [list_hint, guard_hint, auth_label, auth_hint, pwd_status] {
        SendMessageW(ctrl, WM_SETFONT, Some(WPARAM(small.0 as usize)), Some(LPARAM(1)));
    }

    // 缓存控件句柄
    CONTROLS.with(|c| {
        *c.borrow_mut() = Some(ControlHandles { list, pwd, pwd2 });
    });
}

// 说明：这里原来还有 add_list_column / append_list_row 两个 ListView 辅助函数。
// 自绘列表接管之后，行数据直接进 applist.rs 的 ROWS 里，不再需要
// "插列、插行、设状态图像"这一整套 LVM_* 消息，两个函数已整体删除。

/// 创建字体。
unsafe fn make_font(height: i32, weight: i32) -> windows::Win32::Graphics::Gdi::HFONT {
    let face = wide("Microsoft YaHei UI");
    CreateFontW(
        height, 0, 0, 0, weight, 0, 0, 0,
        DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
        FF_DONTCARE.0.into(), PCWSTR(face.as_ptr()),
    )
}

/// 绘制主控台背景：品牌色标题栏 + 两张白色圆角卡片 + 小节标题。
///
/// # 这个函数负责"画布"，不负责控件
/// 按钮、列表、输入框都是独立子窗口，由系统自己画（我们把它们设成自绘后，
/// 改由 WM_DRAWITEM 画）。这里只画**控件之间的空地和装饰**：
/// 页面底色、顶栏、卡片、卡片里的小节标题、关闭按钮。
unsafe fn draw_console_background(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let _ = SetBkMode(hdc, TRANSPARENT);

    let mut rect = RECT::default();
    let _ = GetClientRect(hwnd, &mut rect);

    // ---- 1. 页面底色（浅灰蓝 #F3F5FA）----
    // 卡片是靠"白色铺在浅灰上"形成层次的，所以这一步不能省。
    fill_solid(hdc, rect, Palette::BG);

    // ---- 2. 白色顶栏 + 1px 分割线 ----
    //
    // 【为什么从"整块靛蓝"改成"白色顶栏"】
    // 整片高饱和蓝是"移动端 App"的做法：颜色太重，会把注意力全拉走，
    // 也会让下面的白卡片显脏。Stripe / Linear 这类工具的做法是
    // "白底 + 一条极细分割线 + 一点品牌色当 Logo"，克制反而显贵。
    let header = RECT {
        left: 0,
        top: 0,
        right: rect.right,
        bottom: sc(84), // 顶栏高 76→84（现代化批次：顶部更从容）
    };
    fill_solid(hdc, header, Palette::CARD);
    // 1px 分割线：白色顶栏和浅灰页面之间需要一条边界，否则会糊成一片。
    // 用 BORDER_SOFT（最浅的一档灰）——分割线只是"提示"，不该是"边界"。
    fill_solid(
        hdc,
        RECT {
            left: 0,
            top: sc(84) - 1,
            right: rect.right,
            bottom: sc(84),
        },
        Palette::BORDER_SOFT,
    );

    // ---- 3. 左上角 Logo：36x36 手绘「渐变底板 + 白盾 + H 镂空」----
    //
    // 【为什么手绘而不是贴位图】
    // draw_logo（ui.rs）与 make_icon.ps1 生成的 .ico 同构：同一套几何比例，
    // 任何 DPI 下都锐利、零资源解码开销，而且"界面 Logo"和"exe 图标"永远一致。
    let logo = RECT {
        left: sc(28),
        top: sc(24),
        right: sc(28) + sc(36), // Logo 圆块 32→36，垂直居中于 84 高顶栏：(84-36)/2=24
        bottom: sc(24) + sc(36),
    };
    draw_logo(hdc, logo);

    // ---- 4. 标题与副标题 ----
    // 主标题 19px 半粗（设计令牌：顶栏主标题 →19px），
    // 文字块 18..68 在 84 高顶栏里上下各留 ~17，整体垂直居中。
    let title_font = make_font(-sc(19), FW_SEMIBOLD.0 as i32);
    SelectObject(hdc, title_font.into());
    draw_text_left(
        hdc,
        "HelloGate",
        RECT {
            left: sc(76), // Logo 右缘 64 + 12 间距
            top: sc(18),
            right: sc(330),
            bottom: sc(46),
        },
        Palette::TEXT,
    );

    // 副标题 (76,48)-(430,68)：13px 次要灰（12→13），一句话交代产品是干什么的。
    let sub_font = make_font(-sc(13), FW_NORMAL.0 as i32);
    SelectObject(hdc, sub_font.into());
    draw_text_left(
        hdc,
        "应用锁 · 打开前验证，通过才启动",
        RECT {
            left: sc(76),
            top: sc(48),
            right: sc(430),
            bottom: sc(68),
        },
        Palette::TEXT_SUB,
    );

    // ---- 4.5 顶栏右侧的两个入口：GitHub 与作者 ----
    //
    // 【为什么放在顶栏】这是"关于软件"和"关于作者"这类信息的常规位置：
    // 用户不会天天点，但想找的时候知道往右上角看。
    // 两个入口都用"幽灵按钮"的画法（平时只有文字，悬停才浮出浅底），
    // 不跟主操作（保存）抢注意力。
    {
        let links = header_link_rects(rect.right);
        let hover = HEADER_HOVER.with(|h| h.get());
        let pressed = HEADER_PRESSED.with(|p| p.get());
        // 按下态 = "正按着 且鼠标此刻还在这个入口上"（&& hover）。
        //
        // 【为什么带上 hover 条件】松开时是否真正触发，看的正是"鼠标还在不在
        // 同一入口内"（见 WM_LBUTTONUP）。高亮条件与触发条件保持一致：
        // 用户看到高亮 = 松开真的会生效；按住移到窗口外（WM_MOUSELEAVE 把 hover
        // 清零）高亮就熄灭，也符合系统按钮"移出即不按下"的习惯。
        let pressed_github = pressed == 1 && hover == 1;
        let pressed_author = pressed == 2 && hover == 2;
        let pressed_update = pressed == 3 && hover == 3;
        let link_font = make_font(-sc(14), FW_NORMAL.0 as i32);
        // 记住"换进来之前"的字体，画完要换回去，否则下面 DeleteObject
        // 会删掉一个仍被 DC 选中的字体——GDI 里这属于未定义行为。
        let old_link_font = SelectObject(hdc, link_font.into());

        // ① GitHub：悬停 = 品牌色浅底 + 品牌色字（幽灵按钮浮起）；
        //    按下 = 品牌色**实心胶囊 + 白字**（明确"按下去了"，等价主按钮按下态）。
        if pressed_github {
            fill_round(hdc, links[0], Palette::BRAND, sc(14));
        } else if hover == 1 {
            fill_round(hdc, links[0], Palette::BRAND_SOFT, sc(14));
        }
        draw_text_center(
            hdc,
            "GitHub",
            links[0],
            if pressed_github {
                Palette::ON_BRAND
            } else if hover == 1 {
                Palette::BRAND
            } else {
                Palette::TEXT_SUB
            },
        );

        // ② 作者：20px 圆形头像（品牌色底 + 白色"艺"）+ 名字。
        //    头像比纯文字更容易被认成"人"，也更像现代产品的账号入口。
        if pressed_author {
            fill_round(hdc, links[1], Palette::BRAND, sc(14));
        } else if hover == 2 {
            fill_round(hdc, links[1], Palette::BRAND_SOFT, sc(14));
        }
        let avatar = RECT {
            left: links[1].left + sc(10),
            top: sc(32), // 20px 头像在入口胶囊（26..58）里垂直居中：上下各 6
            right: links[1].left + sc(10) + sc(20),
            bottom: sc(32) + sc(20),
        };
        // 按下时头像**反色**（圆底变白、"艺"字变品牌色）：胶囊底已经是品牌色实心，
        // 头像再用品牌色会糊成一团，反色才能在按下态里保住头像的圆形轮廓。
        fill_round(
            hdc,
            avatar,
            if pressed_author {
                Palette::ON_BRAND
            } else {
                Palette::BRAND
            },
            sc(20),
        );
        draw_text_center(
            hdc,
            "艺",
            avatar,
            if pressed_author {
                Palette::BRAND
            } else {
                Palette::ON_BRAND
            },
        );
        draw_text_left(
            hdc,
            // 顶栏入口统一显示角色名「作者」而不是笔名「艺杯羹」：
            // 新用户一眼就知道这个按钮是干嘛的（点开设作者主页），
            // 笔名本身留到「关于」页里再展示，那里才有介绍作者的语境。
            "作者",
            RECT {
                left: links[1].left + sc(36),
                right: links[1].right - sc(6),
                top: sc(26), // 与入口胶囊矩形（header_link_rects）一致，文字垂直居中
                bottom: sc(58),
            },
            if pressed_author {
                Palette::ON_BRAND
            } else if hover == 2 {
                Palette::BRAND
            } else {
                Palette::TEXT
            },
        );

        // ③ 检查更新（v1.0.0）：幽灵按钮画法与前两个入口完全一致。
        //    点击后后台线程查 GitHub，这里不显示进度（CHECKING 门闩防连点，
        //    结果由 Toast / 「发现新版本」卡片反馈，见 update.rs）。
        if pressed_update {
            fill_round(hdc, links[2], Palette::BRAND, sc(14));
        } else if hover == 3 {
            fill_round(hdc, links[2], Palette::BRAND_SOFT, sc(14));
        }
        draw_text_center(
            hdc,
            "检查更新",
            links[2],
            if pressed_update {
                Palette::ON_BRAND
            } else if hover == 3 {
                Palette::BRAND
            } else {
                Palette::TEXT_SUB
            },
        );

        SelectObject(hdc, old_link_font);
        let _ = DeleteObject(link_font.into());
    }

    // ---- 5. 右上角三个窗口按钮：最小化 / 最大化(还原) / 关闭 ----
    //
    // 【为什么全部纯几何绘制、不用字符】"─ □ ✕"这类符号要靠字体提供字形，
    // "Microsoft YaHei UI" 里未必齐全，缺字形会显示成"豆腐块"。
    // 线条和方框是纯几何绘制，任何 Windows 版本、任何字体环境下长得一模一样。
    //
    // 【悬停反馈对齐系统习惯】最小化/最大化悬停 = 浅灰圆角底；
    // 关闭悬停 = 红底白叉（Windows 自己的标题栏就是红色高亮）。
    // 点击判定在 WM_LBUTTONDOWN 里（window_btn_rects 的三个矩形）。
    {
        let btns = window_btn_rects(rect.right);
        let hover = HEADER_HOVER.with(|h| h.get());
        let maximized = MAXIMIZED.load(Ordering::SeqCst);

        // 悬停底色（先画底再画图形，图形颜色才能压在底色上）。
        for (i, code) in [(0usize, 4u8), (1, 5), (2, 6)] {
            if hover == code {
                let r = btns[i];
                // 底色不贴满整条顶栏：上下各留 14px、左右各留 3px 的圆角块，
                // 视觉上更轻，也不会盖住顶栏底边的分隔线。
                let pad = RECT {
                    left: r.left + sc(3),
                    top: sc(14),
                    right: r.right - sc(3),
                    bottom: sc(70),
                };
                fill_round(
                    hdc,
                    pad,
                    if code == 6 { Palette::DANGER } else { Palette::HOVER },
                    sc(8),
                );
            }
        }

        // ① 最小化：一条 14px 的水平短线（垂直居中略偏下，和系统一致）。
        {
            let cx = (btns[0].left + btns[0].right) / 2;
            let cy = sc(44);
            fill_solid(
                hdc,
                RECT {
                    left: cx - sc(7),
                    top: cy - sc(1),
                    right: cx + sc(7),
                    bottom: cy + sc(1),
                },
                if hover == 4 { Palette::TEXT } else { Palette::TEXT_SUB },
            );
        }

        // ② 最大化 / 还原：未最大化画一个 □；已最大化画"两个叠放的方框"
        //    （系统还原图标就是错位双框，一眼区分两种状态）。
        {
            let cx = (btns[1].left + btns[1].right) / 2;
            let cy = sc(42);
            let color = if hover == 5 { Palette::TEXT } else { Palette::TEXT_SUB };
            if !maximized {
                draw_round_border(
                    hdc,
                    RECT {
                        left: cx - sc(6),
                        top: cy - sc(6),
                        right: cx + sc(6),
                        bottom: cy + sc(6),
                    },
                    color,
                    sc(2),
                    sc(2),
                );
            } else {
                // 后层方框（右上偏移 4px）：只描边。
                draw_round_border(
                    hdc,
                    RECT {
                        left: cx - sc(3),
                        top: cy - sc(9),
                        right: cx + sc(9),
                        bottom: cy + sc(3),
                    },
                    color,
                    sc(2),
                    sc(2),
                );
                // 前层方框：先用顶栏白把后层边框压掉一截，再描自己的边——
                // 叠放关系立刻清楚，也不用做真正的"线条断开"计算。
                fill_solid(
                    hdc,
                    RECT {
                        left: cx - sc(9),
                        top: cy - sc(3),
                        right: cx + sc(4),
                        bottom: cy + sc(9),
                    },
                    if hover == 5 { Palette::HOVER } else { Palette::CARD },
                );
                draw_round_border(
                    hdc,
                    RECT {
                        left: cx - sc(9),
                        top: cy - sc(3),
                        right: cx + sc(3),
                        bottom: cy + sc(9),
                    },
                    color,
                    sc(2),
                    sc(2),
                );
            }
        }

        // ③ 关闭 ✕：悬停红底时换成白色叉（红底上灰叉看不清）。
        {
            let cx = (btns[2].left + btns[2].right) / 2;
            let cy = sc(42);
            draw_close_glyph(
                hdc,
                cx,
                cy,
                sc(7),
                sc(2),
                if hover == 6 { Palette::ON_BRAND } else { Palette::TEXT_SUB },
            );
        }
    }

    let _ = DeleteObject(title_font.into());
    let _ = DeleteObject(sub_font.into());

    // ---- 6. 两张白色圆角卡片（带柔和投影）----
    // 卡片一：受保护应用列表（含添加/移除按钮）
    // 卡片二：总开关 + 认证方式 + 主密码
    //
    // 坐标已经是物理像素，所以全部先过 sc()；
    // 不要再让 draw_card_soft 自己换算，否则会被缩放两次。
    //
    // 【现代化批次几何】左右外边距 16→24（卡片 x 24..736）；
    // draw_card_soft 的最后一个参数是 RoundRect 椭圆**直径**：
    // 旧值 sc(24)（视觉半径 12）→ sc(28)（视觉半径 14）。
    // 纵向分配：顶栏 84 + 16 → 卡片一 100..468；+16 → 卡片二 484..744；
    // +24 → 保存行 768..808；底部余 12。
    draw_card_soft(
        hdc,
        RECT {
            left: sc(24),
            top: sc(100),
            right: sc(736),
            bottom: sc(468),
        },
        sc(28),
    );
    draw_card_soft(
        hdc,
        RECT {
            left: sc(24),
            top: sc(484),
            right: sc(736),
            bottom: sc(744),
        },
        sc(28),
    );

    // ---- 7. 两个小节标题 + 分组分隔线（C1 卡片二分组）----
    // 直接画在卡片上，而不是另外创建 Static 控件：
    // 少一个控件就少一份字体/背景/颜色要维护，且绝不会出现"灰色胶带"底。
    // 小节标题 15→16px 半粗（设计令牌），文字基线与同行控件垂直居中。
    let section_font = make_font(-sc(16), FW_SEMIBOLD.0 as i32);
    let old_section = SelectObject(hdc, section_font.into());
    // 卡片一的小节标题：与添加/移除按钮（112..152，中心 132）居中对齐。
    draw_text_left(
        hdc,
        "受保护的应用",
        RECT {
            left: sc(48),
            top: sc(118),
            right: sc(400),
            bottom: sc(146),
        },
        Palette::TEXT,
    );
    // 卡片二组 1 的小节标题「验证开关」：宣告下面两行（打开软件前验证 /
    // 开机自动启动）属于"什么时候锁"这一组。卡片二顶 484，上内边距 8。
    draw_text_left(
        hdc,
        "验证开关",
        RECT {
            left: sc(48),
            top: sc(492),
            right: sc(400),
            bottom: sc(518),
        },
        Palette::TEXT,
    );
    SelectObject(hdc, old_section);
    let _ = DeleteObject(section_font.into());
    // 组 1 / 组 2 之间的 1px 分隔线（方案 C1 指定用 BORDER_SOFT）：
    // 只靠标题分组还不够，一条极浅的横线把"什么时候锁"和"怎么解锁"
    // 物理隔开，扫读时视线不会串组。
    // 位置：开关行（底 558）下方 9px、分段轨道（顶 576）上方 8px；
    // 左右与内容区对齐（48..712）。
    fill_solid(
        hdc,
        RECT {
            left: sc(48),
            top: sc(567),
            right: sc(712),
            bottom: sc(568),
        },
        Palette::BORDER_SOFT,
    );

    // ---- 8. 认证方式分段控件的"轨道" ----
    //
    // 【为什么要父窗口画轨道】两个片段是自绘按钮，它们只画自己那一格；
    // 轨道（那块浅灰底）是它们共同的家，必须由父窗口先铺好。
    // 两个片段盖上去之后，轨道只在四周露出一圈 2px 的灰边——
    // 这就是 iOS / macOS 分段控件那种"白色滑块浮在灰槽里"的观感。
    // 轨道 (120,576)-(308,616)：高 36→40，宽 188 = 2×92 片段 + 2×2 内缩；
    // 左缘 120 = 标签右缘 112 + 8。圆角直径 20→24（视觉半径 12）。
    fill_round(
        hdc,
        RECT {
            left: sc(120),
            top: sc(576),
            right: sc(308),
            bottom: sc(616),
        },
        Palette::TRACK,
        sc(24),
    );

    // ---- 9. 两个密码输入框的圆角外框 ----
    //
    // Edit 控件已经把 WS_EX_CLIENTEDGE 去掉了（不再自己画凹陷边框），
    // 所以"框"要由我们画：先铺一层卡片白，再描一圈圆角边。
    // 聚焦的框用品牌色描边（focus ring），另一个保持浅灰——
    // 用户不用找光标，一眼就知道现在在往哪个框里打字。
    //
    // 【组件化（B1）】状态到样式的映射集中在这里，具体"长什么样"收敛到
    // ui::draw_input_state 一处实现。以后密码错时想给输入框上红色危险态，
    // 只需在这里把 Normal 换成 Danger，不用动绘制代码。
    let focus = FOCUSED_PWD.with(|f| f.get());
    // 外框矩形必须和 ui::create_input 建控件时传的**完全一致**（同一个 sc 换算），
    // 这样框和文字才严丝合缝。
    let pwd_frame = RECT {
        left: sc(48),
        top: sc(654),
        right: sc(48) + sc(326),
        bottom: sc(654) + sc(44),
    };
    let pwd2_frame = RECT {
        left: sc(386),
        top: sc(654),
        right: sc(386) + sc(326),
        bottom: sc(654) + sc(44),
    };
    draw_input_state(
        hdc,
        pwd_frame,
        if focus == 1 {
            InputState::Focused
        } else {
            InputState::Normal
        },
    );
    draw_input_state(
        hdc,
        pwd2_frame,
        if focus == 2 {
            InputState::Focused
        } else {
            InputState::Normal
        },
    );

    // ---- 10. 主密码状态行 banner（A2）----
    //
    // 【分工】圆角底 + 左侧竖条由父窗口画；文字 Static 只把自己的矩形刷成
    // 同色底再画字（见 WM_CTLCOLORSTATIC 的 is_status 分支）。
    // 两边颜色都来自同一个语气旗标 PWD_STATUS_TONE，所以永远一致。
    // 普通语气下底色就是卡片白（画了也看不出来）、竖条不画——
    // 只有"尚未设置 / 输入不一致 / 两次一致"才需要被看见。
    {
        let tone = PWD_STATUS_TONE.with(|t| t.get());
        let banner = pwd_status_banner_rect();
        let bg = match tone {
            TONE_WARN => Palette::WARN_BG,
            TONE_SUCCESS => Palette::SUCCESS_BG,
            _ => Palette::CARD,
        };
        fill_round(hdc, banner, bg, sc(8));
        if tone != TONE_NORMAL {
            let bar_color = if tone == TONE_WARN {
                Palette::WARN
            } else {
                Palette::SUCCESS
            };
            // 竖条 3px 宽；上下各缩进 8px 正好避开圆角区（半径 8），
            // 形成一条悬浮的小胶囊，比贴边直条柔和，也不会被圆角切掉一角。
            fill_round(
                hdc,
                RECT {
                    left: banner.left + sc(2),
                    top: banner.top + sc(8),
                    right: banner.left + sc(5),
                    bottom: banner.bottom - sc(8),
                },
                bar_color,
                sc(2),
            );
        }
    }

    let _ = EndPaint(hwnd, &ps);
}

/// 处理「添加应用」：弹出**系统标准「打开文件」对话框**让用户直接挑选 exe。
///
/// # 为什么从「手动输入文件名」改成「浏览选择」
/// 普通用户根本说不清 Chrome 的进程叫 `chrome.exe`、微信叫 `WeChat.exe`，
/// 让用户手输等于让用户猜——猜错了锁不住，还完全不知道哪里出了问题。
/// `GetOpenFileNameW` 就是大家在任何软件里都见过的原生文件选择框，
/// 浏览到安装目录点一下 exe，文件名自动就填进列表了，零学习成本。
unsafe fn on_add_app(hwnd: HWND) {
    use windows::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OPENFILENAMEW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY,
        OFN_PATHMUSTEXIST,
    };

    // 路径缓冲区：对话框会把用户选中的完整路径写回这里（1024 字符足够任何安装路径）。
    let mut buf = [0u16; 1024];

    // 文件过滤器：默认显示 .exe 和 .lnk 快捷方式，可切换「所有文件」。
    // 格式是固定的：「描述\0通配符\0」成对出现，结尾要两个 \0。
    // 之所以把 .lnk 也放进来：用户最习惯从桌面快捷方式找软件，
    // 选中快捷方式后会在下面自动解析成真实 exe（见 resolve_shortcut_target）。
    let filter: Vec<u16> = "应用程序 / 快捷方式 (*.exe;*.lnk)\0*.exe;*.lnk\0所有文件 (*.*)\0*.*\0"
        .encode_utf16()
        .collect();
    let title = wide("选择要加锁的应用");

    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        // 指定主控台为「拥有者」窗口：对话框会保证出现在主控台上面、
        // 并随它一起置于前台——这正是之前"弹窗躲在主界面下面"问题的根治手段。
        hwndOwner: hwnd,
        hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
        lpstrFilter: PCWSTR(filter.as_ptr()),
        lpstrFile: windows::core::PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        lpstrTitle: PCWSTR(title.as_ptr()),
        // EXPLORER=现代资源管理器外观；FILEMUSTEXIST/PATHMUSTEXIST=不允许输不存在的路径
        Flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY,
        ..Default::default()
    };

    // 用户点「取消」或直接关掉对话框 → 返回 FALSE，什么都不做。
    if !GetOpenFileNameW(&mut ofn).as_bool() {
        return;
    }

    // 把缓冲区里的 UTF-16 路径读到结束符为止。
    let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
    let path = String::from_utf16_lossy(&buf[..len]);

    // 用户选的如果是桌面快捷方式（.lnk），先解析出它真正指向的 exe。
    // 解析不出来（比如指向网页、商店应用）就明确告知，绝不"静默加一条无效记录"。
    let path = if path.to_lowercase().ends_with(".lnk") {
        match resolve_shortcut_target(&path) {
            Some(target) => target,
            None => {
                // 自绘对话框（dialog.rs）：调用方直接给 &str，
                // 不用再手搓 wide() + PCWSTR 包一层。
                crate::dialog::info(
                    Some(hwnd),
                    "无法识别这个快捷方式",
                    "这个快捷方式解析不出对应的程序文件（它可能指向网页或商店应用）。\n\n\
                     请改用「所有文件」视图，直接选中程序安装目录里的 .exe 文件。",
                );
                return;
            }
        }
    } else {
        path
    };

    // 从完整路径里取出文件名（如 chrome.exe），统一转小写——
    // 后面进程识别是按小写文件名比对的，这里先规范化好。
    let file_name = path.rsplit(['\\', '/']).next().unwrap_or("").to_lowercase();
    if file_name.is_empty() {
        return;
    }
    // 显示名用去掉 .exe 后缀的部分（如 "chrome"），列表里更易读。
    let display = file_name.trim_end_matches(".exe").to_string();

    // 查重：同一个 exe 重复添加没有意义。
    // 不拦的话用户选完发现列表"毫无变化"，又会以为按钮坏了。
    // contains_file 内部按"大小写无关"比对文件名（见 applist.rs），
    // 和我们上面刚做的小写规范化正好对上。
    if applist::contains_file(&file_name) {
        // 无需决策的短反馈 → Toast（方案 A1 的判断标准）：
        // 右下角浮出 3 秒自动消失，不打断"继续添加下一个"的节奏，
        // 不再要求用户把鼠标挪过去点一下「确定」。
        crate::toast::show(Some(hwnd), "这个应用已经在列表里了，不需要重复添加。");
        return;
    }

    // 追加一行，默认勾选（= 启用）。
    // 【为什么新行默认启用】用户点「添加应用」的意图显然是"我要锁住它"，
    // 让用户加完再补勾一下属于多余步骤。
    applist::add_row(&display, &file_name, true);

    // 通知自绘列表重绘：行数变了，滚动范围和"空列表"状态都需要重算。
    // （applist::refresh 内部就是按新的行数刷新并 InvalidateRect）
    if let Some(handles) = CONTROLS.with(|c| *c.borrow()) {
        applist::refresh(handles.list);
    }
}

/// 解析 `.lnk` 桌面快捷方式，返回它真正指向的目标程序路径。
///
/// # 为什么必须要这一步
/// 用户找软件时最自然的路径就是桌面的快捷方式，很可能在文件框里选中 `Word 快捷方式.lnk`。
/// 但加锁的判定依据是**进程的可执行文件名**（WINWORD.EXE）——快捷方式本身只是
/// 一个存有"目标路径"的小索引文件，永远不会变成进程。
/// 如果把 `word.lnk` 记进列表，它和任何进程都对不上，锁就**无声无息地失效**了。
/// 所以这里借 Windows 自带的 IShellLink 组件问一句"这个快捷方式指向哪"，
/// 把背后真正的 exe 路径拿回来继续处理。
///
/// # 返回
/// - `Some(目标路径)`：解析成功（绝大多数普通软件的快捷方式都能解析）。
/// - `None`：快捷方式指向网页、Microsoft Store 应用等没有 exe 目标的东西。
unsafe fn resolve_shortcut_target(path: &str) -> Option<String> {
    use windows::core::{Interface, GUID};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED, STGM_READ,
    };
    use windows::Win32::UI::Shell::IShellLinkW;

    // Shell 快捷方式 COM 对象的类标识（CLSID_ShellLink）。
    // 这是 Windows SDK 头文件 shobjidl.h 里的固定注册值，
    // windows 0.62 这个 crate 没有帮我们生成常量，只能自己写。
    const CLSID_SHELL_LINK: GUID = GUID::from_u128(0x0002_1401_0000_0000_C000_0000_0000_0046);

    // IShellLink 是 COM 组件，用之前必须初始化 COM。
    // 同一线程重复调用无害（只会返回"已经初始化过"的状态码），
    // 主控台线程与进程同生命周期，所以不必再配对 Uninitialize。
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

    // 1. 创建"快捷方式"对象（此时它还是空的，没关联任何 .lnk 文件）
    let link: IShellLinkW =
        CoCreateInstance(&CLSID_SHELL_LINK, None, CLSCTX_INPROC_SERVER).ok()?;

    // 2. 换一张"能从文件读数据"的接口面孔（IPersistFile），
    //    COM 对象的惯用套路：同一个对象，cast 到不同接口调用不同能力。
    let persist: IPersistFile = link.cast().ok()?;

    // 3. 把用户选中的 .lnk 文件真正读进来
    let wpath = wide(path);
    persist.Load(PCWSTR(wpath.as_ptr()), STGM_READ).ok()?;

    // 4. 问它：目标程序在哪？
    //    第二个参数传 null 表示不需要附带的时间戳/大小等信息；
    //    第三个参数 0 = 返回完整长路径（而不是 8.3 短路径）。
    let mut buf = [0u16; 1024];
    link.GetPath(&mut buf, core::ptr::null_mut(), 0).ok()?;

    let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
    if len == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// 把「移除」按钮切换到/退出"待确认"外观（两段式删除，方案 D · 危险操作）。
///
/// 只改记账 + 按钮文字：按钮是自绘的（BS_OWNERDRAW），文字换掉后
/// 重绘时 draw_owner_item 会按 REMOVE_ARMED 旗标画成红字淡红底。
unsafe fn set_remove_armed(hwnd: HWND, armed: bool) {
    REMOVE_ARMED.with(|a| a.set(armed));
    if !armed {
        REMOVE_ARMED_SEL.with(|s| s.set(-1));
    }
    // 常态文案与建控件时一致（"移除"）；armed 态"确认移除？"原样保留——
    // 两段式确认逻辑（REMOVE_ARMED / REMOVE_ARMED_SEL 记账）完全不动。
    let text = if armed { "确认移除？" } else { "移除" };
    if let Ok(btn) = GetDlgItem(Some(hwnd), IDC_REMOVE) {
        if !btn.0.is_null() {
            let _ = SetWindowTextW(btn, PCWSTR(wide(text).as_ptr()));
            // 第三个参数 true = 顺带擦背景，让红字态立刻可见，
            // 不用等下一次别的原因触发重绘。
            let _ = InvalidateRect(Some(btn), None, true);
        }
    }
}

/// 处理「移除选中」：两段式删除（第一次点只预备，第二次点才真删）。
///
/// 【为什么不再用 MessageBox 确认】确认弹窗把"删一行"这种小操作变成
/// 一次打断：弹窗、读字、点按钮，三步。行内确认（按钮自己变红字
/// 「确认移除？」）同样防误触，但视线不用离开列表（方案 D · 危险操作）。
unsafe fn on_remove_selected(hwnd: HWND) {
    let Some(handles) = CONTROLS.with(|c| *c.borrow()) else {
        return;
    };

    // 选中行一变就视为重新"预备"：避免"红字还亮着，选中的却是另一行"，
    // 一下删掉没被确认过的行。
    let sel = applist::selected_index();
    if sel < 0 {
        // 没有选中行：如果按钮正处于待确认态，先复位（没有行可确认了），
        // 再给出和从前一样的明确提示——这个提示解决的是最常见误解
        // （点了勾选框以为那就是"选中"），必须保留。
        if REMOVE_ARMED.with(|a| a.get()) {
            set_remove_armed(hwnd, false);
        }
        let text = "请先在列表里点击要移除的那一行（点行本身，让整行变蓝）。\n\n\
             注意：行首的勾选框是「启用 / 停用」开关，不代表选中该行。";
        crate::dialog::info(Some(hwnd), "还没有选中任何行", text);
        return;
    }

    let want_confirm = REMOVE_ARMED.with(|a| a.get()) && REMOVE_ARMED_SEL.with(|s| s.get()) == sel;
    if !want_confirm {
        // 第一下：只进入"待确认"，按钮变红字「确认移除？」，一行都还没删。
        REMOVE_ARMED_SEL.with(|s| s.set(sel));
        set_remove_armed(hwnd, true);
        return;
    }

    // 第二下（且选中行没变）：真删，然后把按钮还原成常态。
    set_remove_armed(hwnd, false);
    // 上面已确认 sel >= 0，remove_selected 必然成功；返回值不再需要分支。
    let _ = applist::remove_selected(handles.list);
}

/// 主密码输入框的当前状态（保存校验与关窗脏检查共用一份判定，避免两处理解不一致）。
enum PwdState {
    /// 两个框都空：用户不想改密码。
    Unchanged,
    /// 输入了新密码且两次一致（携带明文，只用于现场算哈希，用完即弃）。
    New(String),
    /// 两次输入不一致，或只填了其中一个框。
    Mismatch,
}

/// 读取两个主密码输入框，归成三态之一。
unsafe fn pwd_state(handles: ControlHandles) -> PwdState {
    let p1 = window_text(handles.pwd);
    let p2 = window_text(handles.pwd2);
    if p1.is_empty() && p2.is_empty() {
        PwdState::Unchanged
    } else if p1 == p2 {
        PwdState::New(p1)
    } else {
        PwdState::Mismatch
    }
}

/// 根据两个密码框的**实时内容**，刷新下方那一行提示文字（EN_CHANGE 时调用）。
///
/// 【为什么做这个】用户实测反馈：在「新密码」框输入后，右边/下面的
/// 「确认新密码」框还是空的，用户完全不知道那个空框是留给谁的，
/// 以为界面做坏了。根因是"确认密码"这个动作没有任何过程引导——
/// 只有等点了「保存」才弹"两次不一致"。现在把引导提前到打字的每一步：
///
/// - 两个框都空        → 恢复兜底文案「当前状态：…」（用户没在改密码）；
/// - 只填了「新密码」  → 明确告诉用户"还差一步：请在确认框里再输一遍"（警示色）；
/// - 只填了「确认框」  → 提示"上面的新密码框还是空的"（警示色）；
/// - 两个都有但不一致  → 琥珀 banner"两次输入不一致"；
/// - 两个都有且一致    → 绿色 banner"两次输入一致"（正向反馈，放心保存）。
///
/// 【为什么复用 IDC_PWD_STATUS 这一行】它是"离密码框最近的说明位"，
/// 再加一行新控件会挤占卡片空间；而且它的变色机制（PWD_STATUS_TONE →
/// WM_CTLCOLORSTATIC + 父窗口画 banner）是现成的，直接借用。
///
/// 【颜色怎么变】SetWindowTextW 会自动让 Static 重绘，重绘时父窗口的
/// WM_CTLCOLORSTATIC 会按**当前**的 PWD_STATUS_TONE 选颜色——所以只要
/// 先更新旗标、再改文字，颜色和文字就同步换好了。
unsafe fn update_pwd_status(hwnd: HWND) {
    // 这个 windows 版本里 GetDlgItem 返回 Result<HWND>，统一用 let-else 解包。
    let Ok(status) = GetDlgItem(Some(hwnd), IDC_PWD_STATUS) else {
        return;
    };
    let Ok(p1_h) = GetDlgItem(Some(hwnd), IDC_PWD) else {
        return;
    };
    let Ok(p2_h) = GetDlgItem(Some(hwnd), IDC_PWD2) else {
        return;
    };
    let p1 = window_text(p1_h);
    let p2 = window_text(p2_h);

    // 先按"兜底文案 + 兜底语气"准备，再按实际输入覆盖。
    let base_tone = PWD_STATUS_BASE_TONE.with(|t| t.get());
    let (text, tone) = PWD_STATUS_BASE.with(|b| (b.borrow().clone(), base_tone));
    let (text, tone) = if p1.is_empty() && p2.is_empty() {
        // 两个框都空：用户没有在改密码，回到"当前状态：…"（含它的语气）。
        (text, tone)
    } else if p1.is_empty() {
        // 只填了确认框：顺序反了，先去填上面的新密码框。
        (
            "请先在左边「新密码」框里输入，再到「确认新密码」框重复一遍".to_string(),
            TONE_WARN,
        )
    } else if p2.is_empty() {
        // ★ 用户抱怨的核心场景：新密码已填、确认框还是空的。
        // 必须直说"那个空框是干嘛的、下一步做什么"，不能让用户猜。
        (
            "还差一步：请在「确认新密码」框里把新密码再输一遍".to_string(),
            TONE_WARN,
        )
    } else if p1 == p2 {
        // 两次一致：绿色 banner 给一个明确的"可以保存了"信号（正向反馈）。
        (
            "两次输入一致 —— 点「保存」后新密码生效".to_string(),
            TONE_SUCCESS,
        )
    } else {
        // 不一致：琥珀 banner 提醒，用户不用等到点保存才发现。
        (
            "两次输入不一致 —— 请检查后重新输入".to_string(),
            TONE_WARN,
        )
    };

    // 顺序不能反：先定语气旗标，再改文字（改文字会触发重绘，
    // 重绘时 WM_CTLCOLORSTATIC 读到的就是新旗标）。
    PWD_STATUS_TONE.with(|t| t.set(tone));
    let _ = SetWindowTextW(status, PCWSTR(wide(&text).as_ptr()));
    // 文字变了只有 Static 自己重绘，但 banner 的圆角底和竖条画在**父窗口**上，
    // 必须另外把父窗口那一条区域标脏，否则竖条/底色会停留在上一次的语气。
    let banner = pwd_status_banner_rect();
    let _ = InvalidateRect(Some(hwnd), Some(&banner), false);
    // 极端情况：文字没变而语气变了时 SetWindowTextW 不触发重绘，
    // 这里把 Static 也标脏补一刀（erase=true 让它用新画刷重刷底色）。
    let _ = InvalidateRect(Some(status), None, true);
}

/// 从界面读取当前所有值，组装成 Config（**只读取，不落盘**）。
///
/// 「保存」按钮和「关窗前的未保存检查」都用它，保证两处对"当前状态"的
/// 理解完全一致——否则检查说有改动、保存却写出不同结果，会更混乱。
unsafe fn collect_config(handles: ControlHandles) -> Config {
    // 1. 从自绘列表读取所有行
    //
    // snapshot() 返回 `Vec<(显示名, 文件名, 是否勾选)>`，顺序就是界面上的顺序，
    // 我们原样往配置里写——这样保存前后行序不会变，用户不会看到列表"自己重排"。
    let mut apps = Vec::new();
    for (display, file_name, enabled) in applist::snapshot() {
        if file_name.is_empty() {
            continue;
        }
        apps.push(ProtectedApp {
            file_name,
            display_name: if display.is_empty() {
                "未命名".to_string()
            } else {
                display
            },
            // full_path 一直是空的：加锁只按进程文件名比对，不看安装路径
            // （程序换了安装目录也要能锁住），这里保持原有行为。
            full_path: String::new(),
            enabled,
        });
    }

    // 2. 读取复选框：总开关 / 开机自启
    //
    // 总开关是本次改版的核心：勾上 = 打开受保护软件前先验证（写 IFEO 登记），
    // 取消 = 完全不拦（删除全部登记）。语义见 ifeo.rs::sync。
    let guard_enabled = check_state(IDC_GUARD);
    let autostart = check_state(IDC_AUTOSTART);

    // 3. 遮罩时代的遗留字段（宽限期模式/时长、锁屏重锁）：
    //    界面已经不再提供这些选项，统一沿用窗口打开时的原值，
    //    保证老配置文件不会因为"界面上看不到了"就被悄悄改掉。
    let base = ORIGINAL_CONFIG
        .with(|o| o.borrow().as_ref().cloned())
        .unwrap_or_default();

    // 5. 主密码：两框都空 = 保持原派生值不变；
    //    两次一致的新密码 → 重新生成随机盐 + PBKDF2 派生（见 DESIGN_AND_ROADMAP.md §3.2，
    //    明文绝不落盘；每次更换密码都换一把新盐）。
    //    不一致的半成品输入交给 on_save 弹窗拦截，这里先按"不修改"处理，
    //    保证关窗脏检查不会因为没填完的密码框而误判成"有改动"。
    let (base_hash, base_salt) = ORIGINAL_CONFIG.with(|o| {
        o.borrow()
            .as_ref()
            .map(|c| {
                (
                    c.master_password_hash.clone(),
                    c.master_password_salt.clone(),
                )
            })
            .unwrap_or_default()
    });
    let (master_password_hash, master_password_salt) = match pwd_state(handles) {
        PwdState::New(p) => {
            let salt = crate::password::generate_salt_hex();
            (crate::password::derive_password_hex(&p, &salt), salt)
        }
        _ => (base_hash, base_salt),
    };

    // 6. 认证方式：分段控件的选中项决定写哪一串关键字。
    //    "password" = 自设密码，"system" = 系统验证（Windows Hello）。
    //    注：旧版本这里写的是 "hello"，auth.rs 只认 "password" 这一个特例，
    //    其余一律走系统验证，所以改叫 "system" 后行为完全不变，只是名字更准确。
    let auth_method = if auth_is_password() {
        "password".to_string()
    } else {
        "system".to_string()
    };

    // 7. 组装配置。
    //
    // 注意：这里不再有「恢复码」——忘记密码改为用 Windows 身份验证找回
    // （见 auth::verify_windows_identity），用户不需要再抄写任何东西。
    Config {
        version: 1,
        apps,
        // 以下三项是遮罩遗留字段，原样沿用（界面已不提供设置）
        grace_mode: base.grace_mode,
        grace_seconds: base.grace_seconds,
        relock_on_session_change: base.relock_on_session_change,
        autostart,
        master_password_hash,
        master_password_salt,
        auth_method,
        guard_enabled,
        // 运行中遮罩：主控台不暴露，沿用原值（默认关闭）
        overlay_enabled: base.overlay_enabled,
        // 授权激活状态：主控台不提供开关，原样保留（激活只在启动时做一次）。
        license_activated: base.license_activated,
    }
}

/// 「保存」的统一入口：先立门闩、把按钮切成「保存中…」，200ms 后再真正执行 on_save。
///
/// # 为什么要有这 200ms
/// on_save 要做主密码校验、PBKDF2 派生、DPAPI 存盘、IFEO 注册表同步一整套流程，
/// 用户手快连点（或按住 Enter 不放）会让第二次、第三次点击叠在还没跑完的第一次上：
/// 白做功不说，还可能弹出叠着的警告框。标准做法就是网页按钮的 "submitting…" 状态：
/// 1. 点击瞬间**禁用按钮** + 改文案「保存中…」，给即时反馈；
/// 2. 用 200ms 定时器把真正的保存推迟到下一个消息循环 tick——这期间所有点击
///    落在**已禁用**的按钮上，根本发不出 BN_CLICKED；
/// 3. `SAVING` 门闩做双保险：就算消息绕过了按钮禁用（Enter 走 IDOK、
///    Ctrl+S 转发 WM_COMMAND，这两条路不经过按钮），也会在这里被挡住。
unsafe fn begin_save(hwnd: HWND) {
    // 门闩去重：防连点的核心。已经在等定时器/保存中 → 直接忽略这次触发。
    if SAVING.with(|s| s.get()) {
        return;
    }
    SAVING.with(|s| s.set(true));

    // 禁用按钮 + 换文案：让用户看到"系统收到了，正在处理"，而不是毫无反应。
    if let Ok(btn) = GetDlgItem(Some(hwnd), IDC_SAVE) {
        let _ = EnableWindow(btn, false);
        let saving_text = wide("保存中…");
        let _ = SetWindowTextW(btn, PCWSTR(saving_text.as_ptr()));
        // 不擦背景（false）：draw_button 会把整颗按钮重画一遍，擦了只会白闪一下。
        let _ = InvalidateRect(Some(btn), None, false);
    }

    // 200ms 后 WM_TIMER 回来恢复按钮并执行真正的保存（见窗口过程的 WM_TIMER 分支）。
    // SetTimer 返回新定时器的 ID（失败返回 0）；ID 是我们自己指定的，返回值不用管。
    let _ = SetTimer(Some(hwnd), SAVE_TIMER_ID, 200, None);
}

/// 处理「保存」：从界面读取所有值，构造新配置，存盘并关闭窗口。
unsafe fn on_save(hwnd: HWND) {
    let Some(handles) = CONTROLS.with(|c| *c.borrow()) else {
        return;
    };

    // ★ 主密码校验先行：有问题就阻止保存、也阻止关窗，绝不"悄悄没改成功"。
    match pwd_state(handles) {
        PwdState::Mismatch => {
            crate::dialog::warn(
                Some(hwnd),
                "主密码不一致",
                "两次输入的主密码不一致，请检查后重新输入。\n\n\
                 不想修改密码的话，让两个框都留空即可。",
            );
            return;
        }
        PwdState::New(p) => {
            // 派生失败（CNG / 随机源异常）时必须拦下：若继续保存，会把主密码写成
            // "未设置"——用户以为设好了，实际回退锁失效。
            let salt = crate::password::generate_salt_hex();
            if salt.is_empty() || crate::password::derive_password_hex(&p, &salt).is_empty() {
                crate::dialog::warn(
                    Some(hwnd),
                    "主密码保存失败",
                    "主密码加密失败（系统加密服务异常），本次未能保存主密码。",
                );
                return;
            }
        }
        PwdState::Unchanged => {}
    }

    // ★ 认证方式校验：选「自设密码」就必须真的有密码可用。
    // 新输入的、或窗口打开时已设置过的，都算；两者都没有 → 阻止保存。
    // 【为什么拦】配置成密码模式却没有密码，解锁会降级走 Hello——
    // 与用户"我就想用密码"的预期不符，必须现在讲清楚。
    // 判定只看分段控件的选中项（auth_is_password），不再去读取控件状态。
    if auth_is_password()
        && matches!(pwd_state(handles), PwdState::Unchanged)
        && ORIGINAL_CONFIG.with(|o| {
            o.borrow()
                .as_ref()
                .map(|c| c.master_password_hash.is_empty() || c.master_password_salt.is_empty())
                .unwrap_or(true)
        })
    {
        crate::dialog::warn(
            Some(hwnd),
            "请先设置密码",
            "「自设密码」模式需要先设置密码：\n\n\
             请在下方输入两次一致的密码（3 位、6 位等任意长度均可）。\n\
             如果想继续用 Windows Hello，请把认证方式选回「系统验证」。",
        );
        return;
    }

    let new_cfg = collect_config(handles);

    // 5. 存盘（DPAPI 加密）
    if let Err(e) = crate::config::save(&new_cfg) {
        // ★ 保存失败必须中止整个流程，不能只记一条日志继续往下走。
        //
        // 【为什么】下面紧接着要把配置应用到内存并关窗。如果磁盘写失败却照常
        // 关窗，用户会以为设置成功，重启后全部丢失（尤其是刚设的主密码）。
        // 正确做法是明确报错、留在窗口里让用户重试。
        logln!("[console] 保存配置失败: {e}");
        crate::dialog::warn(
            Some(hwnd),
            "保存失败",
            &format!(
                "配置保存失败，本次改动未生效。\n\n原因：{e}\n\n\
                 请检查磁盘空间/权限后重试；主密码也未生效。"
            ),
        );
        return;
    }
    // 6. 同步自启设置到注册表
    if let Err(e) = crate::autostart::set_enabled(new_cfg.autostart) {
        logln!("[console] 设置开机自启失败: {e}");
    }
    // 6.5 同步 IFEO 启动拦截登记：新增的应用立刻获得"先验证后运行"，
    //     被移除/禁用的应用自动清理，下次开机不再被拦。
    if let Err(e) = crate::ifeo::sync(&new_cfg) {
        logln!("[console] 同步启动拦截登记失败: {e}");
    }

    // 7. 记录结果并关闭窗口
    SAVED_CONFIG.with(|c| *c.borrow_mut() = Some(new_cfg));
    let _ = DestroyWindow(hwnd);
}

/// 统一的「想关窗」入口：先检查有没有未保存的修改，有就询问。
///
/// 【为什么需要】经典陷阱：用户删了一行、改了设置，然后直接点 X 或「取消」
/// 关窗——修改**静默丢失**，下次打开一切复原，用户会以为程序坏了
/// （"我明明移除了，怎么又回来了！"）。弹窗确认彻底消灭这个困惑。
///
/// 判定方式：把界面当前状态组装成 Config，和窗口打开时记下的快照对比。
unsafe fn request_close(hwnd: HWND) {
    let Some(handles) = CONTROLS.with(|c| *c.borrow()) else {
        let _ = DestroyWindow(hwnd);
        return;
    };
    // 脏检查：把界面当前状态原样组装出来对比即可
    let mut current = collect_config(handles);

    // full_path 不参与保存（保存时恒为空串），对比前先抹平，
    // 否则旧配置里带路径的条目会被误判成"有改动"。
    for a in &mut current.apps {
        a.full_path = String::new();
    }
    let dirty = match ORIGINAL_CONFIG.with(|o| o.borrow().clone()) {
        Some(mut o) => {
            for a in &mut o.apps {
                a.full_path = String::new();
            }
            o != current
        }
        None => true, // 拿不到基线（理论上不会发生），保守起见当作有改动
    };

    if !dirty {
        // 什么都没改，直接关，不打扰用户
        let _ = DestroyWindow(hwnd);
        return;
    }

    // 二选一确认 → dialog::confirm（自绘的是/否按钮，见 dialog.rs）。
    // 「保存并关闭」是品牌色主按钮（用户 99% 的正确选择应当最显眼），
    // 「不保存」是白底描边次按钮——降低手滑丢改动的概率。
    // Esc / 点 ✕ 都等同「不保存」（返回 false），与原 MB_YESNO 行为一致。
    let save_and_close = crate::dialog::confirm(
        Some(hwnd),
        "未保存的修改",
        "还有未保存的修改。\n\n现在保存吗？（选\"不保存\"将丢弃本次所有改动）",
        "保存并关闭",
        "不保存",
    );
    if save_and_close {
        on_save(hwnd); // 保存并关窗（on_save 内部会 DestroyWindow）
    } else {
        let _ = DestroyWindow(hwnd);
    }
}

// 说明：这里原来还有 list_item_text / current_list 两个 ListView 辅助函数
// （分别用于"读第 N 行第 M 列文本"和"取缓存的 ListView 句柄"）。
// 现在行数据通过 applist::snapshot() 一次性取出，句柄则从 CONTROLS 里直接取，
// 两个函数已整体删除。

