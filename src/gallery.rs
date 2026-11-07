//! 组件画廊：调试用的静态展示窗（规格决策 D3 · Task 8）。
//!
//! # 它是干什么的
//! 把 `ui.rs` 里所有绘制原语（按钮、输入框、状态 banner、勾选框、分段控件、
//! 色板）的**各个状态**排开画在一张大卡片上，方便开发时**一眼目视核对**
//! 整套视觉体系有没有走样（比如换主题、调 DPI 缩放后逐个状态过一遍）。
//!
//! # 关键设计：纯绘制、无真实控件
//! 窗口里没有任何子控件（没有 Edit、没有按钮控件），全部内容都在 `WM_PAINT`
//! 里用 `ui.rs` 的原语直接画出来——这是一张"静态照片"，不能点、不能输入，
//! 只负责**看**。因此它也天然不会引入焦点、Tab 序、消息路由这些复杂度。
//!
//! # 两条"会出事故"的红线（改动本文件前必读）
//! 1. **非模态**：`open()` 里绝不允许自建 `GetMessage` 消息循环。本窗口挂在
//!    托盘既有的主循环上派发，谁建循环谁把主线程卡死。
//! 2. **`WM_DESTROY` 绝不能调 `PostQuitMessage`**：它会投递 `WM_QUIT`，
//!    托盘主循环收到后 `GetMessage` 返回 0 直接退出——整个程序跟着画廊一起关掉。
//!    详见下方 `WM_DESTROY` 分支的醒目注释。

use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, DeleteObject, EndPaint, InvalidateRect, SelectObject, CLEARTYPE_QUALITY,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, FF_DONTCARE, FW_NORMAL, FW_SEMIBOLD, HFONT,
    OUT_DEFAULT_PRECIS, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
// DPI 在窗口创建之后才能取（GetDpiForWindow 要求窗口已存在），参照 password.rs 的做法。
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowW, GetClientRect, GetSystemMetrics,
    LoadCursorW, RegisterClassW, SetForegroundWindow, SetWindowPos, CS_HREDRAW, CS_VREDRAW,
    IDC_ARROW, SM_CXSCREEN, SM_CYSCREEN, SWP_SHOWWINDOW, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_PAINT, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
};

use crate::ui::{
    draw_button, draw_card_soft, draw_check_box, draw_close_glyph, draw_input_state,
    draw_round_border, draw_segment, draw_text_center, draw_text_left, fill_round, fill_solid,
    round_window, scale, InputState, Palette,
};

use std::cell::Cell;

/// 画廊窗口类名。防重复打开时用 `FindWindowW` 按它查找。
const GALLERY_CLASS: &str = "HelloGateGalleryClass";

/// 窗口逻辑尺寸（96 DPI 下的 640×540，创建后按实际 DPI 换算成物理像素）。
const WIN_W: i32 = 640;
const WIN_H: i32 = 540;

thread_local! {
    /// 本窗口所在屏幕的 DPI 缓存（96 = 100%）。
    ///
    /// 【为什么缓存而不是每帧调 GetDpiForWindow】WM_PAINT 每次重绘都要换算几十个
    /// 坐标，而窗口一旦创建就不会"换屏跳 DPI"（画廊是固定尺寸的调试窗），
    /// 创建时取一次存起来即可，省掉每帧的系统调用。默认 96 保证即使在
    /// 极端时序下 paint 先于 open 跑，坐标也只是偏小、不会错乱。
    static GALLERY_DPI: Cell<i32> = const { Cell::new(96) };
}

/// 逻辑像素 → 本窗口物理像素（统一走 ui::scale 这唯一换算公式，决策 D2）。
fn sc(v: i32) -> i32 {
    scale(v, GALLERY_DPI.with(|c| c.get()))
}

// ============================================================================
// 对外入口
// ============================================================================

/// 打开组件画廊窗口（非模态：创建完就返回，消息交给托盘主循环派发）。
pub unsafe fn open() {
    let class = wide(GALLERY_CLASS);

    // ---- 防重复打开：已存在就把已有的那扇推到前台，不新建 ----
    // 【为什么用 FindWindowW 按类名查】类名是全局唯一的"窗口种类标识"，
    // 同一进程（乃至全系统）里同名类的顶层窗口最多一个，比自己维护一个
    // "窗口句柄 + 是否还活着"的状态变量可靠得多——窗口被用户关掉后句柄
    // 就失效了，自己记账还要处理悬空句柄，FindWindowW 天然没有这个问题。
    if let Ok(existing) = FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null()) {
        if !existing.0.is_null() {
            // 已经开着 → 只把它推到最前，让用户看见，直接返回。
            let _ = SetForegroundWindow(existing);
            return;
        }
    }

    ensure_class_registered();

    let hinst = GetModuleHandleW(None).unwrap_or_default();
    // 先按 96 DPI 的逻辑尺寸在 (0,0) 建出来；窗口可见后立刻能拿到它所在
    // 屏幕的 DPI，再用 SetWindowPos 换算成物理尺寸并居中（password.rs 同款流程）。
    let Ok(hwnd) = CreateWindowExW(
        // WS_EX_TOOLWINDOW：不占任务栏按钮、不进 Alt+Tab——
        // 它是调试窗，不该和主控台/解锁弹窗这些"正式窗口"抢位置。
        WS_EX_TOOLWINDOW,
        PCWSTR(class.as_ptr()),
        PCWSTR::null(),
        // WS_POPUP：无边框自绘窗口，样式与主控台/密码窗一致。
        WS_POPUP | WS_VISIBLE,
        0,
        0,
        WIN_W,
        WIN_H,
        None,
        None,
        Some(hinst.into()),
        None,
    ) else {
        return; // 窗口类没注册成功等异常：安静放弃，画廊只是调试工具
    };

    // Win11 圆角，和其它窗口保持一致（Win10 上静默失败，保持直角）。
    round_window(hwnd);

    // 创建后再取 DPI：此时窗口已落在某块屏幕上，GetDpiForWindow 能拿到
    // 那块屏幕的真实缩放比（多屏异 DPI 场景下必须逐窗取）。
    let dpi = GetDpiForWindow(hwnd).max(96) as i32;
    GALLERY_DPI.with(|c| c.set(dpi));
    let w = sc(WIN_W);
    let h = sc(WIN_H);
    // 屏幕居中；高缩放比小屏上负偏移钳到 0，免得窗口左上角被推出屏外。
    let sw = GetSystemMetrics(SM_CXSCREEN);
    let sh = GetSystemMetrics(SM_CYSCREEN);
    let _ = SetWindowPos(
        hwnd,
        None,
        ((sw - w) / 2).max(0),
        ((sh - h) / 2).max(0),
        w,
        h,
        SWP_SHOWWINDOW, // 顺带完成 ShowWindow(SW_SHOW)，不必再调一次
    );

    // 抢前台 + 焦点：不给焦点的话 WM_KEYDOWN（Esc 关窗）根本收不到，
    // 因为键盘消息只会发给"当前焦点窗口"。
    let _ = SetForegroundWindow(hwnd);
    let _ = SetFocus(Some(hwnd));

    // 让窗口立刻重绘一次。**注意：这里绝不建消息循环**——
    // 窗口创建后就交给托盘既有的主循环去泵消息（见文件头红线 1）。
    let _ = InvalidateRect(Some(hwnd), None, false);
}

// ============================================================================
// 窗口类与窗口过程
// ============================================================================

fn ensure_class_registered() {
    // 只注册一次：RegisterClassW 对同名类第二次调用会失败，静默忽略即可
    // （about.rs 同款写法）。
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    if REGISTERED.load(Ordering::Relaxed) {
        return;
    }
    unsafe {
        let class = wide(GALLERY_CLASS);
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(gallery_wnd_proc),
            hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            // 品牌图标（exe 内嵌 ID=1 资源；WNDCLASSW 无 hIconSm，小图标回退 hIcon）
            hIcon: crate::ui::load_app_icon(),
            // 不设 hbrBackground（保持默认 null）：背景完全由 WM_PAINT 自己画，
            // 配合下面 WM_ERASEBKGND 直接返回 1，系统一次都不插手擦背景，
            // 也就没有任何闪烁。
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
    }
    REGISTERED.store(true, Ordering::Relaxed);
}

extern "system" fn gallery_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            // 整块区域都由 WM_PAINT 自己画满，告诉系统"别再擦一遍背景"，
            // 否则每次重绘都会先闪一下（applist.rs / about.rs 同款处理）。
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_KEYDOWN => {
                // Esc 关窗。wparam 里是虚拟键码，VK_ESCAPE = 0x1B。
                // （open() 里已 SetFocus 到本窗口，键盘消息能落到这。）
                if wparam.0 == VK_ESCAPE.0 as usize {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let x = (lparam.0 & 0xFFFF) as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);
                // 点在右上角 ✕ 热区（48×48，与画的 ✕ 位置对应）→ 关窗。
                if point_in(close_rect(card_rect(client)), x, y) {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                // ★★★ 绝对不能在这里调用 PostQuitMessage！★★★
                //
                // PostQuitMessage(0) 会向线程队列投递 WM_QUIT。本窗口不跑自己的
                // 消息循环，所有消息都由**托盘的主循环**派发——主循环的 GetMessage
                // 一旦收到 WM_QUIT 就返回 0，循环退出，托盘、主控台、锁屏监听
                // 全部停摆，**整个程序直接结束**。也就是说：用户点一下画廊的 ✕，
                // HelloGate 整个就退出了——这是本文件最容易写错、后果最严重的一行。
                //
                // 正确做法：销毁只做销毁，什么都不发（LRESULT(0) 收工）。
                // 画廊是"随开随用、随关随走"的普通顶层窗口，关掉后不需要通知任何人。
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

// ============================================================================
// 布局（逻辑坐标，96 DPI 基准；全部经 sc() 换算成物理像素）
// ============================================================================
//
// 【为什么所有内容都写成"卡片左上角 + 偏移"】窗口一缩放、DPI 一变，卡片位置
// 会整体平移；内容全部相对卡片定位，就永远跟着卡片走，不会出现"窗口变大了
// 文字还钉在旧位置"的错位（about.rs 同款手法）。

/// 客户区内那张大卡片（四周留 16px 露出页面底色，形成"卡片浮在灰底上"的层次）。
fn card_rect(client: RECT) -> RECT {
    RECT {
        left: sc(16),
        top: sc(16),
        right: client.right - sc(16),
        bottom: client.bottom - sc(16),
    }
}

/// 内容区左右边界（卡片两侧各内缩 24px 呼吸位）。
fn content_x(card: RECT) -> (i32, i32) {
    (card.left + sc(24), card.right - sc(24))
}

/// 用"相对卡片左上角的偏移"构造矩形（所有 y 分段见 paint 顶部注释）。
fn at(card: RECT, l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT {
        left: card.left + sc(l),
        top: card.top + sc(t),
        right: card.left + sc(r),
        bottom: card.top + sc(b),
    }
}

/// ✕ 的点击热区：卡片右上角 48×48（比画出来的 ✕ 大一圈，好点中）。
fn close_rect(card: RECT) -> RECT {
    RECT {
        left: card.right - sc(48),
        top: card.top,
        right: card.right,
        bottom: card.top + sc(48),
    }
}

/// 判断点是否在矩形内（点击判定用）。
fn point_in(rc: RECT, x: i32, y: i32) -> bool {
    x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom
}

// ============================================================================
// 绘制：五区纯静态展示
// ============================================================================

/// WM_PAINT 主绘制。
///
/// 【五区 y 分段（相对卡片顶部，逻辑 px）】
/// ```text
///   +6  ~ +34   标题「组件画廊（调试）」+ 右上角 ✕
///   +42 ~ +112  区1 按钮：区名 / 六个按钮 / 每钮状态名
///   +122~ +200  区2 输入框：区名 / 三态外框 / 状态名
///   +210~ +284  区3 状态 banner：区名 / 三态横幅 / 状态名
///   +290~ +360  区4 勾选框 + 分段控件：区名 / 内容 / 说明
///   +368~ +498  区5 色板：区名 / 两排色块（每块下标色名）
/// ```
unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    let card = card_rect(client);
    let (cl, cr) = content_x(card); // 内容区左/右边界

    // ---- 底：页面浅灰 + 一张带柔和投影的白卡片（和主控台同一套层次语言）----
    fill_solid(hdc, client, Palette::BG);
    draw_card_soft(hdc, card, sc(16));

    // ---- 字体：一次创建，画完统一选回并释放（about.rs 同款流程）----
    // 16px 半粗 = 标题；14px = 控件内文字；13px = 区名/状态名/色名等小字。
    let f_title = make_font(-sc(16), FW_SEMIBOLD.0 as i32);
    let f_body = make_font(-sc(14), FW_NORMAL.0 as i32);
    let f_small = make_font(-sc(13), FW_NORMAL.0 as i32);
    let old_font = SelectObject(hdc, f_title.into());

    // ---- 标题 + ✕ ----
    draw_text_left(hdc, "组件画廊（调试）", at(card, 24, 6, 300, 34), Palette::TEXT);
    draw_close_glyph(
        hdc,
        card.right - sc(24),
        card.top + sc(20),
        sc(7),
        sc(2),
        Palette::TEXT_SUB,
    );

    // ========================================================================
    // 区 1 · 按钮：主/次 × 正常/按下/禁用（六个）
    // ========================================================================
    //
    // 【draw_button 没有状态参数怎么办】它的签名是 (rc, under, bg, fg, text,
    // border, radius)——"状态"全靠调用方传什么颜色表达。这正是本组件的设计：
    // 按钮本体不认识状态，状态是**上下文**（是否按下、是否可用）决定的。
    // 所以三态在这里分别用不同的 (bg, fg, border) 组合画出可区分的效果。
    SelectObject(hdc, f_small.into());
    draw_text_left(hdc, "1 · 按钮（主 / 次 × 正常 / 按下 / 禁用）", at(card, 24, 42, 580, 58), Palette::TEXT_MUTED);

    let btn_w = 86;
    let btn_gap = 8;
    // (列号, 文字, 底色, 文字色, 描边)——列号只用来算 x，方便整体对齐。
    // 禁用态的画法：底色压成浅灰、文字降到三级灰（不给品牌色，也就不像"可点"）。
    // 按下态的画法：主按钮底色压深一号（BRAND_DARK），次按钮底色换成悬停灰。
    let buttons: [(i32, &str, u32, u32, Option<u32>); 6] = [
        (0, "保存", Palette::BRAND, Palette::ON_BRAND, None),
        (1, "保存", Palette::BRAND_DARK, Palette::ON_BRAND, None),
        (2, "保存", Palette::BORDER, Palette::TEXT_MUTED, None),
        (3, "取消", Palette::CARD, Palette::TEXT_SUB, Some(Palette::BORDER_STRONG)),
        (4, "取消", Palette::HOVER, Palette::TEXT, Some(Palette::BORDER_STRONG)),
        (5, "取消", Palette::CARD, Palette::TEXT_MUTED, Some(Palette::BORDER)),
    ];
    let btn_names = ["主·正常", "主·按下", "主·禁用", "次·正常", "次·按下", "次·禁用"];
    for (i, (_, text, bg, fg, border)) in buttons.iter().enumerate() {
        let x = cl + (btn_w + btn_gap) * i as i32;
        let rc = RECT {
            left: x,
            top: card.top + sc(62),
            right: x + sc(btn_w),
            bottom: card.top + sc(94),
        };
        // under 必须传按钮所在容器的底色（白卡片），先铺满再画圆角按钮，
        // 否则按钮四角会露出上一次绘制残留的颜色。
        SelectObject(hdc, f_body.into());
        draw_button(hdc, rc, Palette::CARD, *bg, *fg, text, *border, sc(8));
        // 每个按钮下方标注它是哪种状态（13px 小字，居中对齐按钮列）。
        SelectObject(hdc, f_small.into());
        draw_text_center(
            hdc,
            btn_names[i],
            RECT {
                left: x,
                top: card.top + sc(96),
                right: x + sc(btn_w),
                bottom: card.top + sc(112),
            },
            Palette::TEXT_SUB,
        );
    }

    // ========================================================================
    // 区 2 · 输入框：InputState 枚举的全部成员（Normal / Focused / Danger）
    // ========================================================================
    SelectObject(hdc, f_small.into());
    draw_text_left(hdc, "2 · 输入框（InputState 三态）", at(card, 24, 122, 580, 138), Palette::TEXT_MUTED);

    // 三个输入框并排，每列宽 180、间隔 10（3×180 + 2×10 = 560 = 内容区宽度）。
    let input_w = 180;
    let states = [
        (InputState::Normal, "正常"),
        (InputState::Focused, "聚焦"),
        (InputState::Danger, "错误"),
    ];
    for (i, (state, name)) in states.iter().enumerate() {
        let x = cl + (input_w + 10) * i as i32;
        let rc = RECT {
            left: x,
            top: card.top + sc(142),
            right: x + sc(input_w),
            bottom: card.top + sc(182),
        };
        // 先画外框状态，再在里面补一行示例文字——
        // 只有框没有字的话，"文字和框的比例"就核对不了。
        draw_input_state(hdc, rc, *state);
        SelectObject(hdc, f_body.into());
        draw_text_left(hdc, "示例输入", RECT { left: rc.left + sc(12), top: rc.top, right: rc.right - sc(12), bottom: rc.bottom }, Palette::TEXT_SUB);
        // 框下方标注状态名。
        SelectObject(hdc, f_small.into());
        draw_text_center(
            hdc,
            name,
            RECT {
                left: x,
                top: card.top + sc(184),
                right: x + sc(input_w),
                bottom: card.top + sc(200),
            },
            Palette::TEXT_SUB,
        );
    }

    // ========================================================================
    // 区 3 · 状态 banner 三态（照 console.rs 的 A2 banner 画法：
    //        fill_round 圆角底 + 左侧 3px 竖条 + 文字）
    // ========================================================================
    SelectObject(hdc, f_small.into());
    draw_text_left(hdc, "3 · 状态 banner（普通 / 警示 / 成功）", at(card, 24, 210, 580, 226), Palette::TEXT_MUTED);

    // (底色, 文字色, 竖条色 Option, 文案, 状态名)——
    // 竖条色 None = 普通态不画竖条（与 console 的 TONE_NORMAL 一致）。
    // 普通态在主控台上靠"卡片同色"隐身，但画廊要核对它的几何边界，
    // 所以这里额外补一圈浅描边，让"框在哪"看得见（仅画廊如此）。
    let banners: [(u32, u32, Option<u32>, &str, &str); 3] = [
        (Palette::CARD, Palette::TEXT_SUB, None, "普通文案（无竖条）", "普通"),
        (Palette::WARN_BG, Palette::WARN, Some(Palette::WARN), "警示文案", "警示"),
        (Palette::SUCCESS_BG, Palette::SUCCESS, Some(Palette::SUCCESS), "成功文案", "成功"),
    ];
    for (i, (bg, fg, bar, text, name)) in banners.iter().enumerate() {
        let x = cl + (input_w + 10) * i as i32;
        let rc = RECT {
            left: x,
            top: card.top + sc(230),
            right: x + sc(input_w),
            bottom: card.top + sc(266),
        };
        fill_round(hdc, rc, *bg, sc(8));
        if let Some(bar_color) = bar {
            // 竖条 3px 宽、上下各缩进 8px：避开圆角区，形成一条悬浮小胶囊
            // （console.rs draw_console_background 同款几何）。
            fill_round(
                hdc,
                RECT {
                    left: rc.left + sc(2),
                    top: rc.top + sc(8),
                    right: rc.left + sc(5),
                    bottom: rc.bottom - sc(8),
                },
                *bar_color,
                sc(2),
            );
        } else {
            draw_round_border(hdc, rc, Palette::BORDER, sc(8), 1);
        }
        // 文字从竖条右侧留出呼吸位开始（左边距 14 = 竖条 5 + 9 的空隙）。
        SelectObject(hdc, f_body.into());
        draw_text_left(
            hdc,
            text,
            RECT { left: rc.left + sc(14), top: rc.top, right: rc.right - sc(6), bottom: rc.bottom },
            *fg,
        );
        SelectObject(hdc, f_small.into());
        draw_text_center(
            hdc,
            name,
            RECT {
                left: x,
                top: card.top + sc(268),
                right: x + sc(input_w),
                bottom: card.top + sc(284),
            },
            Palette::TEXT_SUB,
        );
    }

    // ========================================================================
    // 区 4 · 勾选框两态 + 分段控件两态
    // ========================================================================
    SelectObject(hdc, f_small.into());
    draw_text_left(hdc, "4 · 勾选框（两态） / 分段控件（两态）", at(card, 24, 290, 580, 306), Palette::TEXT_MUTED);

    // --- 勾选框：勾选（品牌色实心 + 白勾）与未勾选（白底灰框）并排 ---
    let box_size = sc(18);
    let box_y = card.top + sc(317); // 在 310~342 的内容行里垂直居中
    draw_check_box(hdc, cl, box_y, box_size, true, sc(5));
    SelectObject(hdc, f_small.into());
    draw_text_left(hdc, "已勾选", RECT { left: cl + sc(26), top: card.top + sc(310), right: cl + sc(100), bottom: card.top + sc(342) }, Palette::TEXT_SUB);
    draw_check_box(hdc, cl + sc(110), box_y, box_size, false, sc(5));
    draw_text_left(hdc, "未勾选", RECT { left: cl + sc(136), top: card.top + sc(310), right: cl + sc(210), bottom: card.top + sc(342) }, Palette::TEXT_SUB);

    // --- 分段控件：一条灰色轨道里放两个胶囊，左选中、右未选中 ---
    // 轨道先铺底，再把两个片段画进去：未选中片段的底色本身就是轨道灰（TRACK），
    // 二者无缝融合；选中片段是白底 + 品牌字 + 浅描边，两种状态一眼可辨。
    let track = RECT {
        left: cl + sc(260),
        top: card.top + sc(310),
        right: cr,
        bottom: card.top + sc(342),
    };
    fill_round(hdc, track, Palette::TRACK, sc(16));
    let seg_gap = sc(4);
    let seg_h = 24;
    let seg_w = (track.right - track.left - seg_gap * 3) / 2;
    let seg_top = track.top + sc(4);
    let seg1 = RECT { left: track.left + seg_gap, top: seg_top, right: track.left + seg_gap + seg_w, bottom: seg_top + sc(seg_h) };
    let seg2 = RECT { left: seg1.right + seg_gap, top: seg_top, right: track.right - seg_gap, bottom: seg_top + sc(seg_h) };
    SelectObject(hdc, f_body.into());
    draw_segment(hdc, seg1, true, false, "系统验证", sc(12)); // 选中态
    draw_segment(hdc, seg2, false, false, "自设密码", sc(12)); // 未选中态
    // 控件下方一行说明，交代这两种画法分别对应什么状态。
    SelectObject(hdc, f_small.into());
    draw_text_left(hdc, "分段：左「系统验证」选中 · 右「自设密码」未选中", at(card, 24, 344, 580, 360), Palette::TEXT_SUB);

    // ========================================================================
    // 区 5 · Palette 关键色板（两排，每块下方标色名）
    // ========================================================================
    SelectObject(hdc, f_small.into());
    draw_text_left(hdc, "5 · Palette 色板", at(card, 24, 368, 580, 384), Palette::TEXT_MUTED);

    // 两排、每排 5 块：块宽 90、间隔 18（5×90 + 4×18 = 522 ≤ 560 内容区宽）。
    // 色名用常量名本身（BRAND / TEXT_MUTED…），换主题时能直接对上代码。
    let swatches: [(&str, u32); 10] = [
        ("BRAND", Palette::BRAND),
        ("BRAND_SOFT", Palette::BRAND_SOFT),
        ("CARD", Palette::CARD),
        ("TEXT", Palette::TEXT),
        ("TEXT_MUTED", Palette::TEXT_MUTED),
        ("ON_BRAND", Palette::ON_BRAND),
        ("WARN", Palette::WARN),
        ("WARN_BG", Palette::WARN_BG),
        ("SUCCESS", Palette::SUCCESS),
        ("SUCCESS_BG", Palette::SUCCESS_BG),
    ];
    let (sw_w, sw_gap, sw_h) = (90, 18, 36);
    for (i, (name, color)) in swatches.iter().enumerate() {
        let row = i as i32 / 5; // 0 = 上排，1 = 下排
        let col = i as i32 % 5;
        let x = cl + (sw_w + sw_gap) * col;
        let top = card.top + sc(388 + row * 57);
        let rc = RECT { left: x, top, right: x + sc(sw_w), bottom: top + sc(sw_h) };
        fill_round(hdc, rc, *color, sc(8));
        // 白色系色块（CARD / ON_BRAND）在白卡片上会"消失"，
        // 补一圈浅描边把边界勾出来，否则看起来像少画了一块。
        if *color == Palette::CARD || *color == Palette::ON_BRAND {
            draw_round_border(hdc, rc, Palette::BORDER, sc(8), 1);
        }
        // 色名（13px 小字）画在色块正下方。
        draw_text_center(
            hdc,
            name,
            RECT { left: x - sc(6), top: top + sc(sw_h + 1), right: x + sc(sw_w + 6), bottom: top + sc(sw_h + 16) },
            Palette::TEXT_SUB,
        );
    }

    // ---- 收尾：把字体换回去再删除（GDI 对象不删会泄漏）----
    SelectObject(hdc, old_font);
    let _ = DeleteObject(f_title.into());
    let _ = DeleteObject(f_body.into());
    let _ = DeleteObject(f_small.into());

    let _ = EndPaint(hwnd, &ps);
}

/// 创建字体（和 about.rs / 主控台同一字体族与渲染方式，保证观感一致）。
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

/// Rust 字符串 → NUL 结尾的 UTF-16（Win32 API 只认这个）。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
