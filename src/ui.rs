//! 共用的界面绘制基元（主控台 `console.rs` 与解锁弹窗 `password.rs` 都使用）。
//!
//! # 为什么要单独抽一个模块
//! 主控台和解锁弹窗是两扇"门"，但用户眼里它们是同一个软件。如果两边各写一套
//! 按钮、卡片、文字的绘制代码，配色和圆角迟早会漂移成两种样子。
//! 这里把"用什么颜色、多大圆角、怎么画对勾"集中成一份，两边只负责摆位置。
//!
//! # 约定：这里的函数全部接收**已经换算好的物理像素**
//! DPI 换算（96 DPI 下的逻辑值 → 实际像素）留给调用方，原因有两个：
//! - 主控台用的是一份进程级的缩放缓存（启动时算一次，全程复用）；
//! - 解锁弹窗用 `GetDpiForWindow` 逐窗计算（窗口可能出现在另一块不同 DPI 的屏幕上）。
//!
//! 把这个差异挡在模块外面，这里的代码才能保持无状态、任何窗口都能直接用。
//!
//! # 视觉规范（SaaS 极简，参考 Stripe Dashboard）
//! - 页面底色浅灰白，内容放在**纯白圆角卡片**上，靠层次而不是边框分区；
//! - 主操作 = 品牌色实心按钮；次操作 = 白底 1px 细描边；破坏/旁路 = 纯文字；
//! - 圆角克制：卡片 12px、按钮胶囊、勾选框 5px；
//! - 卡片下方叠两层极淡的灰色投影制造层次，其余一律不做渐变。

use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::Graphics::Gdi::{
    CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, GetStockObject, LineTo, MoveToEx,
    Rectangle, RoundRect, SelectObject, SetBkMode, SetTextColor, DT_CENTER, DT_LEFT,
    DT_SINGLELINE, DT_VCENTER, HBRUSH, HDC, HOLLOW_BRUSH, NULL_PEN, PS_SOLID, TRANSPARENT,
};
// windows 0.62 把 HICON 归在 WindowsAndMessaging（图标是"用户界面对象"，不是 GDI 绘图对象）。
use windows::Win32::UI::WindowsAndMessaging::HICON;
use windows::Win32::UI::WindowsAndMessaging::GetWindowTextW;

// ============================================================================
// 缩放换算（全项目唯一的 96 DPI → 物理像素公式）
// ============================================================================

/// 96 DPI 下的逻辑像素 → 实际物理像素（全项目唯一换算公式，决策 D2）。
///
/// 【为什么单独抽出来】console/applist/password 三个窗口各自算过一遍同样的公式，
/// 改一次缩放策略要同步改三处，容易漏。现在只留这一个实现。
/// 【为什么 DPI 来源不合并】console::sc() 在 CreateWindowExW 之前就要用（窗口还不存在，
/// GetDpiForWindow 取不到），且 detect_dpi() 含"窗口不得超出工作区"的钳制逻辑——
/// 这是结构性差异，只收敛公式、不强改来源（规格决策 D2）。
pub fn scale(logical: i32, dpi: i32) -> i32 {
    (logical as f32 * dpi as f32 / 96.0).round() as i32
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
// 把所有颜色集中在这里：换主题（比如改成墨绿、深蓝）只改这一处，
// 不用在几百行绘图代码里到处找颜色数字。
pub struct Palette;

impl Palette {
    /// 品牌主色 #4F46E5（靛蓝）：标题栏、主按钮、勾选框选中态。
    pub const BRAND: u32 = 0x00E5464F;
    /// 品牌深色 #4338CA：主按钮被按下时压深一号，给点击一个明确反馈。
    pub const BRAND_DARK: u32 = 0x00CA3843;
    /// 页面底色 #F3F5FA。
    // 【为什么是这个灰度】纯白卡片 + 柔和投影要"浮"起来，底色就必须比卡片
    // 深一档；但也不能深到发灰发脏。#F3F5FA 是带一点冷调的浅灰，
    // 既能把白色卡片托起来，又不会让界面显得旧。
    pub const BG: u32 = 0x00FAF5F3;
    /// 卡片白 #FFFFFF。
    pub const CARD: u32 = 0x00FFFFFF;
    /// 卡片描边 / 控件边框 #E5E7EB。
    pub const BORDER: u32 = 0x00EBE7E5;
    /// 更浅的描边 #ECEEF4：表格分隔线、分段控件外框等"只需轻微提示"的位置。
    pub const BORDER_SOFT: u32 = 0x00F4EEEC;
    /// 更实的描边 #DDE0EA：输入框、次级按钮需要一眼看出的边界。
    pub const BORDER_STRONG: u32 = 0x00EAE0DD;
    /// 卡片投影第一层 #ECEEF6（贴卡片，稍深）。
    pub const SHADOW_1: u32 = 0x00F6EEEC;
    /// 卡片投影第二层 #F1F3F9（往外扩散，更淡）。
    pub const SHADOW_2: u32 = 0x00F9F3F1;
    /// 品牌色浅底 #EEF2FF：选中态、分段控件高亮、标签底色。
    pub const BRAND_SOFT: u32 = 0x00FFF2EE;
    /// 分段控件/输入框的灰色轨道 #F2F3F8。
    pub const TRACK: u32 = 0x00F8F3F2;
    /// 列表行悬停底色 #F7F8FC。
    pub const ROW_HOVER: u32 = 0x00FCF8F7;
    /// 列表行选中底色 #F4F3FF（极淡的品牌色）。
    pub const ROW_SEL: u32 = 0x00FFF3F4;
    /// 更弱的三级文字色 #94A3B8：进程文件名、辅助说明。
    pub const TEXT_MUTED: u32 = 0x00B8A394;
    /// 正文色 #111827：接近黑但不刺眼。
    pub const TEXT: u32 = 0x00271811;
    /// 次要文字色 #6B7280：说明、提示类文字。
    pub const TEXT_SUB: u32 = 0x0080726B;
    /// 品牌色上的文字（纯白）。
    pub const ON_BRAND: u32 = 0x00FFFFFF;
    /// 悬停/按下时的浅灰底 #F3F4F6。
    pub const HOVER: u32 = 0x00F6F4F3;
    /// 警示色 #B45309（琥珀）：用于「还没设置自己的密码」这类必须被看见的提示。
    pub const WARN: u32 = 0x000953B4;
    /// 危险色 #DC2626（红）：解锁失败、防暴力锁定这类错误信息。
    pub const DANGER: u32 = 0x000626DC;
    /// 危险底色 #FEF2F2（淡红）：密码错误时输入框的填充色。
    pub const DANGER_BG: u32 = 0x00F2F2FE;
    // ---- 以下三个是「状态行 banner」的语义色（UI 优化方案 · 附录）----
    // 【COLORREF 换算】#RRGGBB → 0x00BBGGRR（红蓝互换）：
    // #059669 → 0x00699605，#ECFDF5 → 0x00F5FDEC，#FFF7ED → 0x00EDF7FF。
    /// 成功色 #059669（绿）：「两次输入一致」这类让人心安的正向反馈。
    pub const SUCCESS: u32 = 0x00699605;
    /// 成功底色 #ECFDF5（淡绿）：状态行 banner 的绿色底。
    pub const SUCCESS_BG: u32 = 0x00F5FDEC;
    /// 警示底色 #FFF7ED（淡琥珀）：状态行 banner 的琥珀底
    /// （「尚未设置主密码」「两次输入不一致」时显示）。
    pub const WARN_BG: u32 = 0x00EDF7FF;
}

// ============================================================================
// 基础绘制
// ============================================================================

/// 把无边框窗口的四角切成圆角（Windows 11 生效；Windows 10 静默失败）。
///
/// # 为什么需要这一句
/// 主控台和解锁弹窗都是 `WS_POPUP` 自绘窗口，没有系统标题栏，默认是四个直角。
/// Windows 11 上几乎所有现代软件都是圆角窗口，直角会显得"像老式工具"。
/// DWM 提供了一个标准属性：`DWMWA_WINDOW_CORNER_PREFERENCE`（值 33），
/// 把它设成 `DWMWCP_ROUND`（值 2）即可。Win10 不认识这个属性，调用失败后
/// 窗口保持直角——不影响任何功能，所以返回值直接忽略。
pub unsafe fn round_window(hwnd: HWND) {
    let pref: i32 = 2;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWINDOWATTRIBUTE(33),
        &pref as *const _ as *const std::ffi::c_void,
        std::mem::size_of::<i32>() as u32,
    );
}

/// 用纯色填满一个矩形（先选"空画笔"，保证不留边框）。
pub unsafe fn fill_solid(hdc: HDC, rc: RECT, color: u32) {
    let brush = CreateSolidBrush(COLORREF(color));
    let old_brush = SelectObject(hdc, brush.into());
    // NULL_PEN = "画线时不落笔"，只填色不描边
    let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN));
    // 注意这里的 +1：GDI 的 Rectangle 不包含右边和下边那一列像素
    // （那是给 1px 边框留的位置）。我们故意不画边框，如果不 +1，
    // 控件最外侧就会留一条 1px 的底色缝——在白色卡片上表现为一条灰线。
    let _ = Rectangle(hdc, rc.left, rc.top, rc.right + 1, rc.bottom + 1);
    SelectObject(hdc, old_pen);
    SelectObject(hdc, old_brush);
    let _ = DeleteObject(brush.into());
}

/// 用指定颜色填充一个**圆角矩形**（不画边框）。
///
/// 这是画投影层、分段控件底槽、标签底色等"纯色块"的通用原子。
pub unsafe fn fill_round(hdc: HDC, rc: RECT, color: u32, radius: i32) {
    let brush = CreateSolidBrush(COLORREF(color));
    // 画笔颜色与填充同色 = 看不见边框，又满足 RoundRect 必须选一支笔的要求。
    let pen = CreatePen(PS_SOLID, 1, COLORREF(color));
    let old_brush = SelectObject(hdc, brush.into());
    let old_pen = SelectObject(hdc, pen.into());
    let _ = RoundRect(hdc, rc.left, rc.top, rc.right, rc.bottom, radius, radius);
    SelectObject(hdc, old_pen);
    SelectObject(hdc, old_brush);
    let _ = DeleteObject(brush.into());
    let _ = DeleteObject(pen.into());
}

/// 画一张带**柔和投影**的白色圆角卡片 —— 这是"现代感"最关键的一笔。
///
/// # 和 draw_card 的区别
/// `draw_card` 只有 1px 描边，那种卡片是"印"在背景上的，扁平但偏硬；
/// 这里在卡片下方叠两层非常淡的灰色圆角块，视觉上像卡片浮在桌面上，
/// 层次立刻就出来了（Stripe / Linear 这类界面的核心手法）。
///
/// # 为什么不用半透明（AlphaBlend）
/// 半透明要用 32 位 DIB 或 GDI+，代码量和内存都翻倍。截图中真正起作用的
/// 是"边缘比底色深一点点"的错觉，两个实色灰阶就能骗过眼睛，
/// 而且零额外依赖、零内存分配。
///
/// # 参数
/// - `rc`：卡片本体矩形。
/// - `radius`：RoundRect 的圆角椭圆直径（传 24 左右视觉半径约 12px）。
pub unsafe fn draw_card_soft(hdc: HDC, rc: RECT, radius: i32) {
    draw_card_soft_on(hdc, rc, radius, Palette::BG);
}

/// 同上，但允许指定"卡片外面的底色"。
///
/// # 什么时候需要这个参数
/// 主控台平时铺的是页面底色 `Palette::BG`，而「关于作者」浮层外面盖了一层
/// 更深的遮罩。如果投影函数仍然拿 BG 去铺外圈，就会在遮罩上留下一个
/// 比周围更亮的方块——看起来像卡片外面贴了张白纸。
/// 所以把"外圈底色"变成参数，浮层传自己的遮罩色即可。
pub unsafe fn draw_card_soft_on(hdc: HDC, rc: RECT, radius: i32, page_bg: u32) {
    // 先把卡片外扩一圈的页面底色铺掉，确保投影不会被上一次绘制残留污染。
    fill_solid(
        hdc,
        RECT {
            left: rc.left - 8,
            top: rc.top - 4,
            right: rc.right + 8,
            bottom: rc.bottom + 12,
        },
        page_bg,
    );

    // 第 1 层：最外圈、最淡，整体向下偏移，形成"光源在上方"的错觉。
    fill_round(
        hdc,
        RECT {
            left: rc.left - 4,
            top: rc.top - 1,
            right: rc.right + 4,
            bottom: rc.bottom + 6,
        },
        Palette::SHADOW_2,
        radius + 8,
    );
    // 第 2 层：贴近卡片，稍深一点，让边缘有"浓→淡"的过渡。
    fill_round(
        hdc,
        RECT {
            left: rc.left - 2,
            top: rc.top,
            right: rc.right + 2,
            bottom: rc.bottom + 4,
        },
        Palette::SHADOW_1,
        radius + 4,
    );

    // 卡片本体 + 一圈极浅描边（描边只负责收边，不强调存在感）。
    fill_round(hdc, rc, Palette::CARD, radius);
    draw_round_border(hdc, rc, Palette::BORDER, radius, 1);
}

/// 画一个"输入框外框"：白底 + 圆角描边，聚焦时换成品牌色。
///
/// # 为什么输入框要这样画
/// Edit 是系统控件，它自己画的是**直角矩形 + 系统凹陷边框**，这是界面里
/// 最显旧的元素之一。做法是：Edit 去掉 `WS_EX_CLIENTEDGE`，父窗口在它
/// 正下方画这个圆角框，并把白底铺满 —— Edit 的白底和框内白底同色，
/// 两者无缝拼接，视觉上就是一个圆角输入框（不需要裁剪区域）。
///
/// # 参数
/// - `focused`：为真时描边换成品牌色并加粗到 2px，相当于网页里的 focus ring。
pub unsafe fn draw_input_frame(hdc: HDC, rc: RECT, focused: bool) {
    fill_round(hdc, rc, Palette::CARD, 20);
    draw_round_border(
        hdc,
        rc,
        if focused {
            Palette::BRAND
        } else {
            Palette::BORDER_STRONG
        },
        20,
        if focused { 2 } else { 1 },
    );
}

/// 输入框的三档视觉状态。
///
/// # 为什么要把状态收成一个枚举
/// 在组件化之前，"输入框该画成什么样"这件事散在两个窗口里各写各的：
/// 主控台只区分"聚焦 / 没聚焦"，解锁弹窗则自己拼了一个
/// 「淡红底 + 红边」的错误分支。结果就是同一类控件的画法有三套，
/// 改一处（比如想把聚焦描边加粗到 2px）必须记得去三个地方同步。
///
/// 收成 `InputState` 之后：调用方只负责**判断当前是什么状态**，
/// "这个状态长什么样"永远只有 `draw_input_state` 一处实现。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum InputState {
    /// 普通：白底 + 浅灰 1px 描边。
    Normal,
    /// 聚焦：白底 + 品牌色 2px 描边（网页里的 focus ring）。
    Focused,
    /// 错误：淡红底 + 红色 2px 描边（密码错 / 校验不通过）。
    Danger,
}

/// 按状态画输入框外框（`console.rs` 与 `password.rs` 的唯一入口）。
///
/// # 参数
/// - `rc`：外框矩形（**物理像素**，调用方自己完成 DPI 换算）。
/// - `state`：三档状态之一，见 [`InputState`]。
pub unsafe fn draw_input_state(hdc: HDC, rc: RECT, state: InputState) {
    match state {
        InputState::Normal => draw_input_frame(hdc, rc, false),
        InputState::Focused => draw_input_frame(hdc, rc, true),
        InputState::Danger => {
            // 错误态不能只换描边：淡红填充才是"这里出错了"的第一眼信号，
            // 光靠红边在白卡片上太弱，用户会以为只是聚焦框变了个颜色。
            fill_round(hdc, rc, Palette::DANGER_BG, 20);
            draw_round_border(hdc, rc, Palette::DANGER, 20, 2);
        }
    }
}

/// 创建一个「圆角外框 + 垂直居中 Edit」的完整输入框，返回 Edit 句柄。
///
/// # 为什么要有这个组件（B1：输入框组件化）
/// 输入框 = 系统 Edit（画文字）+ 父窗口画的圆角外框，两边必须严丝合缝。
/// 组件化之前，两边的几何值分别硬编码在"创建控件"和"绘制外框"两处，
/// 主控台写的是 `(34,449,312,24)`、解锁弹窗写的是 `(38,175,352,24)`——
/// 数字各算各的，任何一边调整（改高度、加按钮让位）都会让文字和外框错位。
/// 实测"输入框文字贴顶"就是这么来的。
///
/// 现在**外框矩形是唯一的事实来源**：Edit 的位置由它推导出来，
/// 创建和绘制用的是同一个 `rc`，几何关系想错位都难。
///
/// # 参数
/// - `parent`：父窗口（主控台 / 密码弹窗）。
/// - `rc_frame`：输入框**外框**矩形，物理像素（父窗口画框也用它）。
/// - `dpi`：换算基准（96 = 100%）。主控台传进程级的 `console::dpi()`，
///   密码弹窗传 `GetDpiForWindow`，两边各自用自己窗口的缩放，互不干扰。
/// - `id`：控件 ID（父窗口靠它找句柄）。
/// - `password`：true = 输入内容打圆点（ES_PASSWORD）。
/// - `font`：正文字体句柄，创建后立即套上，避免控件用系统默认宋体。
/// - `reserve_right`：右端预留宽度（物理像素）。密码弹窗要在右端放
///   「显示 / 隐藏」小按钮，就传按钮宽度 + 间隙；没有就传 0（占满外框内区）。
///
/// # 内部几何（和旧版手工算的完全一致，只是收敛到一处）
/// ```text
/// 外框 rc_frame（高 40）
///   └ 内区 = 四周各内缩 2px（高度 34，用来露外框那圈描边）
///       └ Edit 高 24（一行字的高度），在内区里上下居中：
///         top = 内区top + (34 - 24) / 2 = 外框top + 2 + 5
/// ```
/// 【为什么 Edit 只有 24 高】Win32 单行 Edit 没有垂直居中样式，文字永远
/// 贴控件顶部画。唯一可靠的做法是把控件压到"一行字"的高度，再在外框里
/// 上下留同宽的空隙——文字自然就落在框的正中间。
pub unsafe fn create_input(
    parent: HWND,
    rc_frame: RECT,
    dpi: i32,
    id: i32,
    password: bool,
    font: windows::Win32::Graphics::Gdi::HFONT,
    reserve_right: i32,
) -> HWND {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, SendMessageW, HMENU, WINDOW_EX_STYLE, WINDOW_STYLE, ES_AUTOHSCROLL,
        ES_PASSWORD, WM_SETFONT, WS_CHILD, WS_TABSTOP, WS_VISIBLE,
    };

    // 本窗口的逻辑像素 → 物理像素（和调用方换算外框用的是同一个系数，
    // 所以这里算出来的 Edit 必然和外框对齐，不存在两套缩放打架的问题）。
    let px = |v: i32| (v as f32 * dpi as f32 / 96.0).round() as i32;

    let edit_h = px(24); // 一行字的高度
    let inset = px(2); // 内区四周缩进，用来露出外框描边
    // 上下居中：内区高度 - Edit 高度，剩下的平分两半。
    let inner_top = rc_frame.top + inset;
    let inner_h = (rc_frame.bottom - rc_frame.top) - inset * 2;
    let edit_top = inner_top + ((inner_h - edit_h) / 2).max(0);

    let edit_left = rc_frame.left + inset;
    let edit_right = rc_frame.right - inset - reserve_right.max(0);

    let mut style = ES_AUTOHSCROLL;
    if password {
        style |= ES_PASSWORD;
    }
    let edit_class = wide_ui("Edit");
    let Ok(hinstance) = GetModuleHandleW(None) else {
        return HWND::default();
    };
    let Ok(edit) = CreateWindowExW(
        // 扩展样式传 0 = 不要 WS_EX_CLIENTEDGE：系统凹陷边框由父窗口的
        // 圆角外框取代（这正是输入框"去 Win95 化"的关键开关）。
        WINDOW_EX_STYLE(0),
        windows::core::PCWSTR(edit_class.as_ptr()),
        windows::core::PCWSTR::null(),
        // WS_TABSTOP：按 Tab 能跳进来（否则这种"无边框"窗口里没法用键盘走位）。
        // ES_* 在 windows crate 里是 i32 常量，转回 WINDOW_STYLE 才能和 WS_* 组合。
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(style as u32),
        edit_left,
        edit_top,
        (edit_right - edit_left).max(px(20)),
        edit_h,
        Some(parent),
        Some(HMENU(id as *mut _)),
        Some(hinstance.into()),
        None,
    ) else {
        return HWND::default();
    };

    // 字体：不设的话 Edit 会用系统默认宋体，和旁边自绘按钮的雅黑对不上。
    SendMessageW(edit, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));

    // 左右各 12px 内边距：第一个字符不贴圆角，光标也别贴在两端。
    // EM_SETMARGINS(0x00D1)：lParam 低 16 位 = 左边距，高 16 位 = 右边距；
    // wParam 传 3 = EC_LEFTMARGIN(1) | EC_RIGHTMARGIN(2)，两个都要设。
    const EM_SETMARGINS: u32 = 0x00D1;
    const EC_LEFT_AND_RIGHT: usize = 3;
    let margin = px(12) as isize;
    SendMessageW(
        edit,
        EM_SETMARGINS,
        Some(WPARAM(EC_LEFT_AND_RIGHT)),
        Some(LPARAM((margin << 16) | margin)),
    );

    edit
}

/// 模块内部用的宽字符转换（`create_input` 创建控件类名时用）。
fn wide_ui(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 画认证方式"分段控件"里的一个片段。
///
/// # 为什么不再用下拉框
/// 系统 ComboBox 是 Windows 95 时代的外观：凹陷白框 + 灰色小三角，
/// 无论外面怎么美化，它一出现就把整个界面拉回 2008 年。
/// 这里把两个选项画成并排的两个胶囊：选中项白底 + 品牌色文字，
/// 未选中项透明 + 灰色文字，外观完全由我们掌控，也少一次点击（直接可见）。
///
/// # 参数
/// - `selected`：当前片段是否为选中项。
/// - `hovered`：鼠标是否悬停在这个片段上（悬停给一点浅灰反馈）。
pub unsafe fn draw_segment(
    hdc: HDC,
    rc: RECT,
    selected: bool,
    hovered: bool,
    text: &str,
    radius: i32,
) {
    let bg = if selected {
        Palette::CARD
    } else if hovered {
        Palette::ROW_HOVER
    } else {
        Palette::TRACK
    };
    fill_round(hdc, rc, bg, radius);
    if selected {
        // 选中片段描一圈品牌浅色边，和未选中的灰底拉开层次。
        draw_round_border(hdc, rc, Palette::BORDER, radius, 1);
    }
    draw_text_center(
        hdc,
        text,
        rc,
        if selected {
            Palette::BRAND
        } else {
            Palette::TEXT_SUB
        },
    );
}

/// 只画一圈 1px 圆角描边（不填充），用于自绘输入框的外框。
///
/// 【为什么要单独一个函数】输入框本体是系统 Edit 控件，它自己画的是直角矩形；
/// 我们把带圆角的描边画在它**紧贴的外圈**，Edit 的白底和白色卡片融合，
/// 只留下这圈圆角细线——视觉上就是一个圆角输入框，不需要裁剪区域。
pub unsafe fn draw_round_border(hdc: HDC, rc: RECT, color: u32, radius: i32, width: i32) {
    // HOLLOW_BRUSH = "空画刷"：只描边、不填充内部。
    // 这里千万不能用实色画刷——那会把 Edit 控件底下的白色涂成别的颜色，
    // 文字周围就会出现一块色差。空画刷才是"只画线"的正确做法。
    let old_brush = SelectObject(hdc, GetStockObject(HOLLOW_BRUSH));
    let pen = CreatePen(PS_SOLID, width.max(1), COLORREF(color));
    let old_pen = SelectObject(hdc, pen.into());
    let _ = RoundRect(hdc, rc.left, rc.top, rc.right, rc.bottom, radius, radius);
    SelectObject(hdc, old_pen);
    SelectObject(hdc, old_brush);
    let _ = DeleteObject(pen.into());
}

/// 在矩形内水平、垂直居中画一行文字。
pub unsafe fn draw_text_center(hdc: HDC, text: &str, rc: RECT, color: u32) {
    let mut rect = rc;
    let mut buf: Vec<u16> = text.encode_utf16().collect();
    // ★ 必须先设成"透明背景模式"，否则 DrawTextW 会用 DC 默认的白色背景
    // 把文字所在的整个矩形涂白——在品牌色顶栏上就是一块刺眼的白补丁，
    // 在实心按钮上就是文字周围一圈白框（实测截图里非常明显）。
    // TRANSPARENT = 只画字形，不碰背景。
    let _ = SetBkMode(hdc, TRANSPARENT);
    let _ = SetTextColor(hdc, COLORREF(color));
    // DrawTextW 的第三个参数是"文字长度"，不需要结尾的 \0
    let _ = DrawTextW(
        hdc,
        &mut buf,
        &mut rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
}

/// 在指定横向范围内、垂直居中、左对齐地画一行文字。
pub unsafe fn draw_text_left(hdc: HDC, text: &str, rc: RECT, color: u32) {
    let mut rect = rc;
    let mut buf: Vec<u16> = text.encode_utf16().collect();
    // 同样要透明背景：否则在品牌色顶栏上会画出一条白底"胶带"。
    let _ = SetBkMode(hdc, TRANSPARENT);
    let _ = SetTextColor(hdc, COLORREF(color));
    let _ = DrawTextW(hdc, &mut buf, &mut rect, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
}

/// 画一个圆角按钮（填充色 + 可选描边 + 居中文字）。
///
/// # 参数
/// - `rc`：按钮矩形（物理像素）。
/// - `under`：按钮所在容器的底色。**必须先铺这一层**：
///   自绘按钮的四个圆角之外会露出系统擦背景时留下的颜色，
///   只有先用容器底色填满整个矩形，圆角才不会出现灰边/白边。
/// - `border`：描边颜色，`None` 表示不要描边（主按钮就用这种）。
/// - `radius`：圆角大小（物理像素）。
pub unsafe fn draw_button(
    hdc: HDC,
    rc: RECT,
    under: u32,
    bg: u32,
    fg: u32,
    text: &str,
    border: Option<u32>,
    radius: i32,
) {
    fill_solid(hdc, rc, under);
    let brush = CreateSolidBrush(COLORREF(bg));
    // 无描边时用"和填充同色"的画笔——RoundRect 必须先选一支笔，
    // 传同色等于看不见边框，既简单又不会出现一圈突兀的黑线。
    let pen = CreatePen(PS_SOLID, 1, COLORREF(border.unwrap_or(bg)));
    let old_brush = SelectObject(hdc, brush.into());
    let old_pen = SelectObject(hdc, pen.into());
    let _ = RoundRect(hdc, rc.left, rc.top, rc.right, rc.bottom, radius, radius);
    SelectObject(hdc, old_pen);
    SelectObject(hdc, old_brush);
    let _ = DeleteObject(brush.into());
    let _ = DeleteObject(pen.into());
    draw_text_center(hdc, text, rc, fg);
}

/// 画一个自绘复选框的方框（含选中时的对勾），返回方框右边界以便调用方接文字。
///
/// # 参数
/// - `x` / `y`：方框左上角。
/// - `size`：方框边长。
/// - `checked`：是否显示为选中（品牌色实心 + 白色对勾）。
pub unsafe fn draw_check_box(hdc: HDC, x: i32, y: i32, size: i32, checked: bool, radius: i32) {
    // 外框：勾选 = 品牌色实心；未勾选 = 白底浅灰描边。
    let pen = CreatePen(
        PS_SOLID,
        1,
        COLORREF(if checked { Palette::BRAND } else { Palette::BORDER }),
    );
    let brush = CreateSolidBrush(COLORREF(if checked {
        Palette::BRAND
    } else {
        Palette::CARD
    }));
    let old_pen = SelectObject(hdc, pen.into());
    let old_brush = SelectObject(hdc, brush.into());
    let _ = RoundRect(hdc, x, y, x + size, y + size, radius, radius);
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(brush.into());
    let _ = DeleteObject(pen.into());

    if !checked {
        return;
    }

    // 手绘对勾：两段直线，比字体里的 ✓ 更可控——
    // 字体的勾在不同字号下粗细会乱跳，画线则能严格跟着 DPI 走。
    // 坐标按方框边长的比例取，这样任何 DPI 下形状都一致。
    let white_pen = CreatePen(PS_SOLID, (size / 9).max(1), COLORREF(Palette::ON_BRAND));
    let old = SelectObject(hdc, white_pen.into());
    let p1 = (x + size * 22 / 100, y + size * 52 / 100);
    let p2 = (x + size * 40 / 100, y + size * 72 / 100);
    let p3 = (x + size * 78 / 100, y + size * 24 / 100);
    let _ = MoveToEx(hdc, p1.0, p1.1, None);
    let _ = LineTo(hdc, p2.0, p2.1);
    let _ = LineTo(hdc, p3.0, p3.1);
    SelectObject(hdc, old);
    let _ = DeleteObject(white_pen.into());
}

/// 手绘一个 ✕（两条交叉线段）。
///
/// 【为什么不用字符】`✕`、`×` 这些符号要靠字体提供字形，
/// "Microsoft YaHei UI" 里未必都有；缺字形时会出现一个"豆腐块"。
/// 两条线是纯几何绘制，任何 Windows 版本、任何字体环境下都长得一模一样。
pub unsafe fn draw_close_glyph(hdc: HDC, cx: i32, cy: i32, r: i32, thickness: i32, color: u32) {
    let pen = CreatePen(PS_SOLID, thickness.max(1), COLORREF(color));
    let old_pen = SelectObject(hdc, pen.into());
    let _ = MoveToEx(hdc, cx - r, cy - r, None);
    let _ = LineTo(hdc, cx + r, cy + r);
    let _ = MoveToEx(hdc, cx + r, cy - r, None);
    let _ = LineTo(hdc, cx - r, cy + r);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(pen.into());
}

/// 读取控件上的文字（按钮的自绘文字就来自控件本身，不用另外维护一份）。
pub unsafe fn window_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = GetWindowTextW(hwnd, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..len])
}

/// 返回一个纯色画刷（缓存复用）。
///
/// 【为什么要缓存】GDI 对象数量和句柄一样是有限资源。如果每次重绘都新建一个画刷，
/// 长时间反复刷新会把 GDI 句柄耗尽。这里用 `OnceLock` 只创建一次，全生命周期复用。
///
/// 【为什么要存裸句柄值】`HBRUSH` 含裸指针，不满足 `Send`/`Sync`，放进静态变量会编译不过。
/// 句柄本身只是一个数值，跨线程读是安全的。
pub fn solid_brush(color: u32) -> HBRUSH {
    // 用线程局部的小表缓存：每个线程最多也就用到 2~3 种颜色（白、淡红、页面灰），
    // 用 Vec 线性查找比哈希表更简单，而且完全不加锁——重绘路径上不能有锁开销。
    use std::cell::RefCell;

    thread_local! {
        static CACHE: RefCell<Vec<(u32, isize)>> = const { RefCell::new(Vec::new()) };
    }

    CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        if let Some((_, raw)) = cache.iter().find(|(c, _)| *c == color) {
            return HBRUSH(*raw as *mut _);
        }
        let raw = unsafe { CreateSolidBrush(COLORREF(color)).0 as isize };
        cache.push((color, raw));
        HBRUSH(raw as *mut _)
    })
}

// ============================================================================
// 品牌图标（exe 资源加载 + 界面手绘，两处图形同构）
// ============================================================================

/// 加载编进 exe 的品牌图标（build.rs 用 winres 登记的 MAINICON 组，资源 ID = 1）。
///
/// 【为什么能按 ID=1 拿到】winres 的 `set_icon` 把 .ico 写成
/// RT_GROUP_ICON + RT_ICON 资源，组名固定为 IDMAINICON（数值 1）。
/// 资源管理器/桌面/任务栏显示的 exe 图标，和这里 LoadIconW 拿到的，是同一份数据——
/// 所以托盘、任务栏、Alt-Tab、窗口标题栏的图标永远一致。
///
/// 【MAKEINTRESOURCE 惯用法】Win32 里"资源名"参数其实是指针：
/// 指针数值 ≤ 0xFFFF 时，系统不当它是字符串地址，而是当**资源 ID**。
/// C 宏 MAKEINTRESOURCEW(1) 就是 `(LPWSTR)1`；Rust 里等价写法是
/// `PCWSTR::from_raw(1usize as *const u16)`——"把数字 1 伪装成字符串指针"。
///
/// 【失败回退】万一资源缺失（比如非 winres 构建的产物），
/// 回退系统默认应用图标并写日志，保证任何窗口/托盘都不会"裸奔"没图标。
///
/// 【句柄生命周期】LoadIconW 对同一资源返回的是系统缓存的共享句柄，
/// 重复调用拿到同一个值，**不要** DeleteObject / DestroyIcon。
pub unsafe fn load_app_icon() -> HICON {
    use windows::core::PCWSTR;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{IDI_APPLICATION, LoadIconW};

    let own = GetModuleHandleW(None)
        .ok()
        // hmod 是 HMODULE，窗口类/图标 API 要 HINSTANCE——两者是同一个数值的
        // 不同"马甲"类型，windows crate 提供了 From 转换，.into() 即可。
        .and_then(|hmod| LoadIconW(Some(hmod.into()), PCWSTR::from_raw(1usize as *const u16)).ok());

    match own {
        Some(h) => h,
        None => {
            crate::logln!("[ui] 自有图标(ID=1)加载失败，回退系统默认应用图标");
            LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
        }
    }
}

/// 手绘品牌 Logo —— 与 `tools\make_icon.ps1` 生成的 .ico **同构**：
/// 渐变圆角底板 + 白色盾牌 + H 镂空。界面三处（主控台 / 弹窗 / 密码框）统一调用它，
/// 保证"界面里的 Logo"和"exe 图标"永远是同一个图形，改规格只需改这一个函数 + 脚本。
///
/// # 几何（全部按底板边长 s 的比例换算，与 .ico 完全一致）
/// - 底板：圆角半径 0.22s，垂直渐变 #6366F1（上）→ #4F46E5（下）；
/// - 盾牌：宽 0.52s、高 0.58s，盾顶在 0.21s 处（光学居中偏上）；
///   顶边两角圆角 0.08s，两侧垂直下探到盾高 55%，再二次贝塞尔收向底部中点尖；
/// - H：宽/高 0.30s，笔画粗 0.055s，用渐变底色 #4F46E5 画（等于"镂空"）；
///   水平居中，垂直中心在盾顶 + 0.525×盾高（即盾高 30%~75% 区间的中点）。
///
/// # 参数
/// - `rc`：底板矩形（物理像素，调用方自己过 DPI 换算；**默认正方形**，宽高相等）。
///
/// # 为什么盾牌用折线近似而不是 GDI 路径 API
/// GDI 的 Polygon 一次点阵就能填出封闭图形；圆角拆成 8 段小折线、
/// 贝塞尔按 16 步采样，在 32 逻辑像素的绘制尺寸下误差远小于半像素，
/// 肉眼与矢量版无差别——比 PathBeginPath/PathPaint 那套少一半代码，
/// 也少一半 API 兼容性风险。
pub unsafe fn draw_logo(hdc: HDC, rc: RECT) {
    use windows::Win32::Graphics::Gdi::{
        CreateRectRgn, CreateRoundRectRgn, GetClipRgn, GradientFill, Polygon, SelectClipRgn,
        GRADIENT_FILL_RECT_V, GRADIENT_RECT, TRIVERTEX,
    };

    let s = (rc.right - rc.left) as f32;
    if s <= 0.0 {
        return;
    }
    // 浮点几何 → 整数像素的统一收口（每个点只 round 一次，避免累积误差）。
    let f = |v: f32| v.round() as i32;

    // ---- ① 底板：垂直渐变圆角方块 ----
    // GradientFill 只会填"矩形"，所以先把 DC 裁剪到圆角区域、再整块铺渐变：
    // 圆角外的四个角被裁掉、露出底下的顶栏白——圆角是"裁"出来的，不会被切坏。
    let d = (0.44 * s).round().max(1.0) as i32; // 圆角直径 = 2 × 半径(0.22s)
    let rounded = CreateRoundRectRgn(rc.left, rc.top, rc.right + 1, rc.bottom + 1, d, d);
    // 先保存调用方现有的裁剪区（WM_PAINT 的更新区域），画完原样还回去，
    // 绝不污染 DC 状态——这是公共绘制函数的基本礼貌。
    let saved = CreateRectRgn(0, 0, 0, 0);
    let had = GetClipRgn(hdc, saved);
    let _ = SelectClipRgn(hdc, Some(rounded));
    // ★ 这里最容易踩的坑：TRIVERTEX 用的是**正常 R,G,B**（16 位、高字节有效），
    // 不是 COLORREF 的 0x00BBGGRR 红蓝互换！CreateSolidBrush(COLORREF(#4F46E5))
    // 要写成 0x00E5464F，而 GradientFill 的顶点直接写 (0x4F, 0x46, 0xE5) 即可。
    // 所以 #6366F1 → Red=0x63、Green=0x66、Blue=0xF1（各左移 8 位补足 16 位）。
    let verts = [
        TRIVERTEX {
            x: rc.left,
            y: rc.top,
            Red: 0x63 << 8,
            Green: 0x66 << 8,
            Blue: 0xF1 << 8,
            Alpha: 0xFF00,
        },
        TRIVERTEX {
            x: rc.right,
            y: rc.bottom,
            Red: 0x4F << 8,
            Green: 0x46 << 8,
            Blue: 0xE5 << 8,
            Alpha: 0xFF00,
        },
    ];
    let grect = GRADIENT_RECT {
        UpperLeft: 0,
        LowerRight: 1,
    };
    // windows 0.62 把"顶点数组 + 数量"两个参数合并成一个切片 &[TRIVERTEX]。
    let _ = GradientFill(
        hdc,
        &verts,
        &grect as *const GRADIENT_RECT as *const std::ffi::c_void,
        1,
        GRADIENT_FILL_RECT_V,
    );
    // 还原裁剪区：had == 1 表示调用方本来有裁剪区，套回去；
    // 0 表示本来没有（整个 DC 不裁），恢复成"无裁剪"。
    if had == 1 {
        let _ = SelectClipRgn(hdc, Some(saved));
    } else {
        let _ = SelectClipRgn(hdc, None);
    }
    let _ = DeleteObject(rounded.into());
    let _ = DeleteObject(saved.into());

    // ---- ② 白色盾牌（轮廓点阵 + Polygon 实心填充）----
    let sw = 0.52 * s; // 盾宽
    let sh = 0.58 * s; // 盾高
    let sl = rc.left as f32 + (s - sw) / 2.0; // 盾左缘（水平居中）
    let sr = sl + sw; // 盾右缘
    let st = rc.top as f32 + 0.21 * s; // 盾顶（光学居中偏上）
    let sb = st + sh; // 盾底尖
    let rt = 0.08 * s; // 顶边两角圆角半径
    let ys = st + 0.55 * sh; // 垂直段终点：从这里开始收尖
    let cx = rc.left as f32 + s / 2.0; // 画布水平中线

    let mut pts: Vec<POINT> = Vec::new();
    // 顶边（左圆角终点 → 右圆角起点）
    pts.push(POINT { x: f(sl + rt), y: f(st) });
    pts.push(POINT { x: f(sr - rt), y: f(st) });
    // 右上角圆弧：圆心 (sr-rt, st+rt)，从 -90°（正上方）顺时针扫 90° 到 0°（正右方）
    for k in 1..=8 {
        let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::FRAC_PI_2 * (k as f32 / 8.0);
        pts.push(POINT { x: f(sr - rt + rt * a.cos()), y: f(st + rt + rt * a.sin()) });
    }
    // 右侧垂直下探到 55% 盾高
    pts.push(POINT { x: f(sr), y: f(ys) });
    // 右半收尖：二次贝塞尔 P0=(sr,ys) 控制点 Q=(sr,sb) P2=(cx,sb)。
    // 控制点取"右下角"：曲线先贴侧边继续向下、再平滑拐向尖端（与 .ico 同款）。
    for k in 1..=16 {
        let t = k as f32 / 16.0;
        let mt = 1.0 - t;
        // 二次贝塞尔采样公式：B(t) = (1-t)²P0 + 2(1-t)tQ + t²P2
        pts.push(POINT {
            x: f(mt * mt * sr + 2.0 * mt * t * sr + t * t * cx),
            y: f(mt * mt * ys + 2.0 * mt * t * sb + t * t * sb),
        });
    }
    // 左半收尖（镜像）：P0=(cx,sb) Q=(sl,sb) P2=(sl,ys)
    for k in 1..=16 {
        let t = k as f32 / 16.0;
        let mt = 1.0 - t;
        pts.push(POINT {
            x: f(mt * mt * cx + 2.0 * mt * t * sl + t * t * sl),
            y: f(mt * mt * sb + 2.0 * mt * t * sb + t * t * ys),
        });
    }
    // 左侧垂直上探到左圆角起点
    pts.push(POINT { x: f(sl), y: f(st + rt) });
    // 左上角圆弧：圆心 (sl+rt, st+rt)，从 180°（正左方）顺时针扫 90° 到 270°（正上方）
    for k in 1..=8 {
        let a = std::f32::consts::PI + std::f32::consts::FRAC_PI_2 * (k as f32 / 8.0);
        pts.push(POINT { x: f(sl + rt + rt * a.cos()), y: f(st + rt + rt * a.sin()) });
    }
    // Polygon 会自动封闭回第一个点，顶边左段不用重复画。

    let old_brush = SelectObject(hdc, solid_brush(Palette::CARD).into());
    // windows 0.62 的 Polygon 同样把"点数组 + 数量"合并成 &[POINT]。
    let _ = Polygon(hdc, &pts);
    SelectObject(hdc, old_brush);

    // ---- ③ H 镂空：三条 #4F46E5 矩形（两竖=门柱、一横=门梁）----
    // 颜色取渐变**下端**的 #4F46E5（= Palette::BRAND，COLORREF 已红蓝互换）。
    // H 垂直中心在盾顶 + 0.525×盾高（盾高 30%~75% 区间的中点），与 .ico 一致。
    let hw = 0.30 * s; // H 宽
    let hh = 0.30 * s; // H 高
    let th = (0.055 * s).max(1.0); // 笔画粗（竖=横），至少 1px 保证小尺寸可见
    let hcy = st + 0.525 * sh; // H 垂直中心
    let bl = f(cx - hw / 2.0); // H 左缘
    let br = f(cx + hw / 2.0); // H 右缘
    let ht = f(hcy - hh / 2.0); // H 顶
    let hb = f(hcy + hh / 2.0); // H 底
    let tb = th.round() as i32;
    // fill_solid 的 RECT 右/下边是"包含"语义（内部 +1），这里按含头不含尾算好。
    // 左竖（门柱）
    fill_solid(hdc, RECT { left: bl, top: ht, right: bl + tb - 1, bottom: hb }, Palette::BRAND);
    // 右竖（门柱）
    fill_solid(hdc, RECT { left: br - tb, top: ht, right: br - 1, bottom: hb }, Palette::BRAND);
    // 横（门梁）：垂直居中于 H，横跨整个 H 宽
    fill_solid(
        hdc,
        RECT { left: bl, top: f(hcy - th / 2.0), right: br - 1, bottom: f(hcy + th / 2.0) },
        Palette::BRAND,
    );
}
