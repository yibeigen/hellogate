//! 「关于作者」浮层：主控台顶栏点「艺杯羹」后弹出的说明卡片。
//!
//! # 为什么是"主控台拥有的弹出窗口"（不是子窗口）
//! 最初实现用的是 `WS_CHILD` 子窗口，结果实测踩坑：它虽然盖住了主控台自己画的
//! 背景，却盖不住**兄弟子窗口**（按钮、输入框、复选框中转出来的静态文本）——
//! 那些控件比浮层晚创建，Z 序天然在浮层之上，界面就碎成一团。
//! `SetWindowPos(panel, HWND_TOP)` 在这个场景下也不可靠（见 open 里的注释）。
//!
//! 现在改成 `WS_POPUP` + `hwndParent = 主控台`（不带 `WS_CHILD` 时，父句柄表示
//! **拥有者 owner**）。Windows 保证 owned 窗口永远排在 owner 之上，这是窗口
//! 管理器层面的规则，不会被任何兄弟控件插队：
//! 1. owner 销毁 → 浮层自动销毁，不用手动同步生命周期；
//! 2. owner 最小化 → 浮层自动隐藏，还原时自动恢复；
//! 3. 浮层铺满主控台客户区，鼠标点不到背后的「保存」「添加应用」——天然模态；
//! 4. 缩放系数、圆角窗口、调色板全部和主控台共用，视觉上就是同一页内容。
//!
//! # 二维码图片从哪来、怎么显示
//! 公众号与赞赏码两张图以 PNG 字节的形式**编进 exe**（`include_bytes!`）：
//! - 用户拷走一个 exe 就能看到完整内容，不会丢资源文件；
//! - PNG 是压缩格式，两张图只有几十 KB，不会把程序撑大。
//!
//! 解码交给 Windows 自带的 **WIC**（Windows Imaging Component，从 Vista 起
//! 就是系统组件），所以我们没有引入任何第三方库，也没多出需要分发的 DLL。

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, EndPaint,
    SelectObject, SetStretchBltMode, StretchBlt, BI_RGB, BITMAPINFO, BITMAPINFOHEADER,
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS, FF_DONTCARE, FW_NORMAL,
    FW_SEMIBOLD, HALFTONE, HBITMAP, HDC, HFONT, OUT_DEFAULT_PRECIS, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::Graphics::Imaging::{
    IWICImagingFactory, IWICPalette, WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom,
    WICDecodeMetadataCacheOnDemand, CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA,
};
// WIC 是基于 COM 的组件；这两个调用负责"把 COM 叫醒"和"创建一个解码工厂对象"。
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
// SetFocus 在键盘与鼠标模块里，不在 WindowsAndMessaging——这是 windows crate
// 按 API 原始头文件分区导致的，写代码时容易找错地方。
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE};
use windows::Win32::UI::Shell::{ShellExecuteW, SHCreateMemStream};
// ClientToScreen 虽然名字看起来属于窗口管理，但 windows crate 按原始头文件
// 分区把它放在 Graphics::Gdi 里（winuser.h 中的 GetDC 系列归 GDI 模块）。
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetParent,
    LoadCursorW, RegisterClassW, SetCursor, CS_HREDRAW, CS_VREDRAW, IDC_ARROW, IDC_HAND,
    SW_SHOWNORMAL, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_PAINT, WNDCLASSW,
    // WS_POPUP：无边框顶层窗口（正好用来画"整块遮罩 + 一张卡片"）；
    // WS_EX_TOOLWINDOW：不占任务栏按钮、不进 Alt+Tab 列表，看起来就是主控台的一部分。
    WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
};

use crate::console::sc;
use crate::ui::{
    draw_card_soft_on, draw_close_glyph, draw_text_center, draw_text_left, fill_round, fill_solid,
    Palette,
};

/// 浮层窗口类名。
const PANEL_CLASS: &str = "HelloGateAboutPanelClass";
/// `WM_MOUSELEAVE`（windows crate 未导出，见 console.rs 里的同名常量说明）。
const WM_MOUSELEAVE: u32 = 0x02A3;

// ----------------------------------------------------------------------------
// 作者信息（改这里就能换人、换号、换链接）
// ----------------------------------------------------------------------------
/// HelloGate 的 GitHub **项目仓库**地址。
///
/// 【说明】顶栏 GitHub 入口、关于卡片、检查更新都读这里——
/// 只写 GitHub 一个入口（用户要求），Gitee / GitCode 作为镜像
/// 出现在「关于作者」卡片和「发现新版本」提示里。
pub(crate) const GITHUB_URL: &str = "https://github.com/yibeigen/hellogate";
/// Gitee 镜像仓库（国内网络下载更快）。
pub(crate) const GITEE_URL: &str = "https://gitee.com/yibeigen/hellogate";
/// GitCode 镜像仓库（国内网络下载更快）。
pub(crate) const GITCODE_URL: &str = "https://gitcode.com/yibeigen/hellogate";
/// GitHub Release 最新发布页（检查更新拿到新版本号后引导用户到这里下载）。
pub(crate) const GITHUB_RELEASES_URL: &str = "https://github.com/yibeigen/hellogate/releases/latest";
/// Gitee 发行版列表页（国内网络下载更快，v1.0.0 起已上传 exe 附件）。
pub(crate) const GITEE_RELEASES_URL: &str = "https://gitee.com/yibeigen/hellogate/releases";
/// GitCode 发行版列表页（国内网络下载更快，v1.0.0 起已上传 exe 附件）。
pub(crate) const GITCODE_RELEASES_URL: &str = "https://gitcode.com/yibeigen/hellogate/releases";
/// 微信号。
const WECHAT_ID: &str = "peace-83";
/// QQ 号。
const QQ_ID: &str = "3057454077";

/// 卡片尺寸（96 DPI 下的逻辑像素）。
///
/// 【v1.0.0 加高】联系方式从 3 行扩到 5 行（新增 Gitee / GitCode 两个镜像入口，
/// 用户要求"作者介绍里可以把三个都写着"），下方二维码区与版本行整体下移 64px。
const CARD_W: i32 = 600;
const CARD_H: i32 = 526;

/// 卡片外面的遮罩色 #E6EAF0。
///
/// 【为什么不用半透明】GDI 画半透明要么上 AlphaBlend + 32 位位图，要么上 GDI+，
/// 代价都不小。这里换成"比页面底色再深一档的纯色"，视觉上同样能把背后的内容
/// 压下去、让白色卡片浮出来，而且一次 fill_solid 就够。
const SCRIM: u32 = 0x00F0EAE6;

thread_local! {
    /// 两张二维码解码后的位图（第一次绘制时才解码，之后一直复用）。
    static IMAGES: RefCell<Images> = const { RefCell::new(Images::new()) };
    /// 鼠标悬停目标：0 = 无，1 = 关闭按钮，2 = GitHub 行，3 = Gitee 行，4 = GitCode 行。
    static HOVER: Cell<u8> = const { Cell::new(0) };
}

/// 解码后的位图。`hbmp` 由 GDI 持有，进程结束时随进程释放。
/// pub(crate)：授权码激活窗口（license.rs）要复用同一张公众号二维码。
pub(crate) struct QrImage {
    hbmp: HBITMAP,
    w: i32,
    h: i32,
}

/// 图片缓存。
struct Images {
    loaded: bool,
    /// 公众号（横版：二维码 + "微信搜一搜"）。
    official: Option<QrImage>,
    /// 赞赏码（竖版：插画 + 二维码）。
    reward: Option<QrImage>,
}

impl Images {
    const fn new() -> Self {
        Self {
            loaded: false,
            official: None,
            reward: None,
        }
    }
}

// ============================================================================
// 对外入口
// ============================================================================

/// 打开「关于作者」浮层。
///
/// 参数 `parent` 是主控台窗口句柄。浮层铺满它的客户区，所以调用方不需要
/// 关心位置和尺寸。
pub unsafe fn open(parent: HWND) {
    ensure_class_registered();

    let mut client = RECT::default();
    let _ = GetClientRect(parent, &mut client);

    // 把客户区左上角从"窗口内坐标 (0,0)"换算成屏幕坐标。
    // 弹出窗口（WS_POPUP）不吃父窗口的坐标系，位置必须自己算准；
    // 主控台不可缩放、浮层打开后又盖住了标题栏，所以算一次就够。
    let mut origin = POINT { x: 0, y: 0 };
    let _ = ClientToScreen(parent, &mut origin);

    let class = wide(PANEL_CLASS);
    let hinst = GetModuleHandleW(None).unwrap_or_default();
    // 这个版本的 windows crate 把 CreateWindowExW 包成了 Result：
    // 失败时（例如窗口类没注册成功）会返回 Err，这里直接放弃显示浮层。
    //
    // 【关键】这里刻意不写 WS_CHILD：父句柄传的是**拥有者（owner）**。
    // Windows 会保证 owned 窗口始终排在 owner 之上，而子窗口的 Z 序还要和
    // 一堆兄弟控件打架（曾经就是这里出问题，按钮/输入框盖住了浮层）。
    let Ok(panel) = CreateWindowExW(
        WS_EX_TOOLWINDOW,
        PCWSTR(class.as_ptr()),
        PCWSTR::null(),
        WS_POPUP | WS_VISIBLE,
        origin.x,
        origin.y,
        client.right,
        client.bottom,
        Some(parent),
        None,
        Some(hinst.into()),
        None,
    ) else {
        return;
    };

    // 拿焦点，保证 Esc 之类将来要加的键盘操作不会落到背后的输入框里。
    let _ = SetFocus(Some(panel));
}

/// 用系统默认浏览器打开网址。
///
/// 顶栏的 GitHub 入口和卡片里的 GitHub 行都走这里，地址只在一个地方定义。
pub(crate) unsafe fn open_url(url: &str) {
    let verb = wide("open");
    let target = wide(url);
    // 返回值大于 32 才算成功；这里不弹错误框——连浏览器都打不开的环境，
    // 弹窗也帮不上忙，只会更烦人。
    let _ = ShellExecuteW(
        None,
        PCWSTR(verb.as_ptr()),
        PCWSTR(target.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

// ============================================================================
// 窗口类与消息处理
// ============================================================================

fn ensure_class_registered() {
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    if REGISTERED.load(Ordering::Relaxed) {
        return;
    }
    unsafe {
        let class = wide(PANEL_CLASS);
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(panel_wnd_proc),
            hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            // 光标用箭头；具体"能不能点"由 WM_MOUSEMOVE 动态切换成小手。
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            // 品牌图标（exe 内嵌 ID=1 资源；WNDCLASSW 无 hIconSm，小图标回退 hIcon）
            hIcon: crate::ui::load_app_icon(),
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
    }
    REGISTERED.store(true, Ordering::Relaxed);
}

extern "system" fn panel_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            // 整块区域都由我们自己画，告诉系统"别再擦一遍背景"，
            // 否则每次重绘都会先闪一下白底。
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let x = (lparam.0 & 0xFFFF) as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);
                let card = card_rect(client);
                // 三个仓库行（下标 2/3/4）逐个判定，编码成不同的悬停状态，
                // 绘制时靠这个编号决定哪一行高亮（见 paint 第 6 节）。
                let hover = if point_in(close_rect(card), x, y) {
                    1
                } else if point_in(repo_row(card, 2), x, y) {
                    2
                } else if point_in(repo_row(card, 3), x, y) {
                    3
                } else if point_in(repo_row(card, 4), x, y) {
                    4
                } else {
                    0
                };

                // 可点的位置换成"小手"光标，这是最便宜也最直观的可点击提示。
                let cursor = if hover != 0 { IDC_HAND } else { IDC_ARROW };
                if let Ok(c) = LoadCursorW(None, cursor) {
                    let _ = SetCursor(Some(c));
                }

                if hover != HOVER.with(|h| h.get()) {
                    HOVER.with(|h| h.set(hover));
                    // 卡片上有图片，整窗重绘一次也就几毫秒，不值得为省这点开销
                    // 去算"只刷哪一块"。
                    let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, false);
                }

                // 登记"鼠标离开窗口"的通知，否则光标和高亮会一直留着。
                let mut tme = windows::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<
                        windows::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT,
                    >() as u32,
                    dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::TrackMouseEvent(&mut tme);
                LRESULT(0)
            }
            x if x == WM_MOUSELEAVE => {
                if HOVER.with(|h| h.get()) != 0 {
                    HOVER.with(|h| h.set(0));
                    let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                // 【方案 D · Esc（P1）】Esc 关闭关于浮层。
                // open() 里已经 SetFocus 到浮层本身（浮层内全是自绘、没有
                // 子控件），按键消息直接落在这里，不会漏到背后的主控台。
                // wparam 里是虚拟键码：VK_ESCAPE = 0x1B。
                if wparam.0 == VK_ESCAPE.0 as usize {
                    close(hwnd);
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let x = (lparam.0 & 0xFFFF) as i32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);
                let card = card_rect(client);

                if point_in(close_rect(card), x, y) {
                    close(hwnd);
                } else if point_in(repo_row(card, 2), x, y) {
                    open_url(GITHUB_URL);
                } else if point_in(repo_row(card, 3), x, y) {
                    open_url(GITEE_URL);
                } else if point_in(repo_row(card, 4), x, y) {
                    open_url(GITCODE_URL);
                } else if !point_in(card, x, y) {
                    // 点卡片外面的遮罩 = "点空白处关闭"，和绝大多数弹出层的习惯一致。
                    close(hwnd);
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// 关闭浮层，并让主控台把刚被盖住的区域重画一遍。
unsafe fn close(hwnd: HWND) {
    let parent = GetParent(hwnd).unwrap_or_default();
    let _ = DestroyWindow(hwnd);
    if !parent.0.is_null() {
        // 不清空这块区域的话，销毁瞬间会留下浮层最后一帧的残影。
        let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(parent), None, false);
    }
}

// ============================================================================
// 布局计算
// ============================================================================

/// 卡片矩形：在客户区里水平、垂直居中。
///
/// 所有内容坐标都写成"卡片左上角 + 偏移"，这样卡片一动、内容整体跟着走，
/// 不会出现"窗口变大但文字还钉在旧位置"的情况。
fn card_rect(client: RECT) -> RECT {
    let w = sc(CARD_W);
    let h = sc(CARD_H);
    let cx = (client.right + client.left) / 2;
    let cy = (client.bottom + client.top) / 2;
    RECT {
        left: cx - w / 2,
        top: cy - h / 2,
        right: cx + w / 2,
        bottom: cy + h / 2,
    }
}

/// 关闭按钮的点击热区（卡片右上角 56×56 的方块）。
fn close_rect(card: RECT) -> RECT {
    RECT {
        left: card.right - sc(56),
        top: card.top,
        right: card.right,
        bottom: card.top + sc(56),
    }
}

/// 联系方式区里仓库链接行的点击热区。
///
/// 行高 32、从卡片顶部 +140 开始；`idx` = 行下标：
/// 0=微信 1=QQ 2=GitHub 3=Gitee 4=GitCode（后三行可点，前两个只是文本）。
fn repo_row(card: RECT, idx: i32) -> RECT {
    let top = card.top + sc(140) + sc(32) * idx;
    RECT {
        left: card.left + sc(32),
        top,
        right: card.right - sc(32),
        bottom: top + sc(32),
    }
}

/// 判断点是否在矩形内（点击与悬停共用）。
fn point_in(rc: RECT, x: i32, y: i32) -> bool {
    x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom
}

/// 用相对卡片左上角的方式构造矩形（少写一大堆 `card.left + ...`）。
fn at(card: RECT, left: i32, top: i32, right: i32, bottom: i32) -> RECT {
    RECT {
        left: card.left + sc(left),
        top: card.top + sc(top),
        right: card.left + sc(right),
        bottom: card.top + sc(bottom),
    }
}

// ============================================================================
// 绘制
// ============================================================================

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    let card = card_rect(client);

    // ---- 1. 遮罩 + 白卡片 ----
    fill_solid(hdc, client, SCRIM);
    draw_card_soft_on(hdc, card, sc(24), SCRIM);

    // ---- 2. 字体（一次创建，画完统一释放）----
    let f_title = make_font(-sc(16), FW_SEMIBOLD.0 as i32);
    let f_name = make_font(-sc(18), FW_SEMIBOLD.0 as i32);
    let f_small = make_font(-sc(13), FW_NORMAL.0 as i32);
    let f_value = make_font(-sc(15), FW_NORMAL.0 as i32);
    let old_font = SelectObject(hdc, f_title.into());

    // ---- 3. 标题 + 关闭按钮 ----
    draw_text_left(
        hdc,
        "关于作者",
        at(card, 32, 22, 300, 48),
        Palette::TEXT,
    );

    let close_hot = HOVER.with(|h| h.get()) == 1;
    draw_close_glyph(
        hdc,
        card.right - sc(28),
        card.top + sc(34),
        sc(7),
        sc(2),
        if close_hot {
            Palette::BRAND
        } else {
            Palette::TEXT_SUB
        },
    );

    // ---- 4. 头像 + 名字 + 一句话签名 ----
    let avatar = at(card, 32, 60, 76, 104);
    fill_round(hdc, avatar, Palette::BRAND, sc(44));
    draw_text_center(hdc, "艺", avatar, Palette::ON_BRAND);

    SelectObject(hdc, f_name.into());
    draw_text_left(hdc, "艺杯羹", at(card, 88, 58, 300, 84), Palette::TEXT);

    SelectObject(hdc, f_small.into());
    draw_text_left(
        hdc,
        "探索技术边界，构建数字世界",
        at(card, 88, 86, 440, 106),
        Palette::TEXT_SUB,
    );

    // ---- 5. 分隔线 ----
    fill_solid(hdc, at(card, 32, 124, CARD_W - 32, 125), Palette::BORDER_SOFT);

    // ---- 6. 联系方式五行 ----
    //
    // 【v1.0.0 扩行】原来只有 3 行（微信 / QQ / GitHub），现在把 Gitee、GitCode
    // 两个镜像仓库也列进来（用户要求"作者介绍里可以把三个都写着"）。
    // 【排版说明】标签固定在左边一列（x=32..92）、值从 x=96 开始，
    // 每行行高 32px，与 repo_row() 的点击热区严格对齐，五行排成两条竖线。
    SelectObject(hdc, f_small.into());
    draw_text_left(hdc, "微信", at(card, 32, 143, 92, 166), Palette::TEXT_SUB);
    draw_text_left(hdc, "QQ", at(card, 32, 175, 92, 198), Palette::TEXT_SUB);
    draw_text_left(hdc, "GitHub", at(card, 32, 207, 92, 230), Palette::TEXT_SUB);
    draw_text_left(hdc, "Gitee", at(card, 32, 239, 92, 262), Palette::TEXT_SUB);
    draw_text_left(hdc, "GitCode", at(card, 32, 271, 92, 294), Palette::TEXT_SUB);

    SelectObject(hdc, f_value.into());
    draw_text_left(hdc, WECHAT_ID, at(card, 96, 140, 320, 166), Palette::TEXT);
    draw_text_left(hdc, QQ_ID, at(card, 96, 172, 320, 198), Palette::TEXT);

    // 三个仓库链接统一画法：品牌色文字 + 下划线，明确表达"这是链接、点了会跳转"；
    // 悬停时加深一档（BRAND_DARK），和顶栏入口的交互反馈保持一致。
    // 元组 = (值文本, 下划线宽度, 对应的 HOVER 编码)；
    // 下划线宽度按 15px 字宽估算（github.com 沿用原值 92，其余按字符数等比推算）。
    let links: [(&str, i32, u8); 3] =
        [("github.com", 92, 2), ("gitee.com", 83, 3), ("gitcode.com", 101, 4)];
    for (i, (text, uw, code)) in links.iter().enumerate() {
        // 行的顶边 = 204 + 32*i，和 repo_row(card, 2+i) 严格对应；
        // 文本框高 26、下划线画在顶边 +24 处（1px 粗），与原 GitHub 行画法一致。
        let value_top = 204 + 32 * i as i32;
        let hot = HOVER.with(|h| h.get()) == *code;
        let color = if hot { Palette::BRAND_DARK } else { Palette::BRAND };
        draw_text_left(hdc, text, at(card, 96, value_top, 320, value_top + 26), color);
        fill_solid(hdc, at(card, 96, value_top + 24, 96 + *uw, value_top + 25), color);
    }

    // ---- 7. 第二块内容：两张二维码 ----
    // 【v1.0.0】联系方式从 3 行扩到 5 行后，本区整体下移 64px（新增 2 行 × 32px）。
    fill_solid(hdc, at(card, 32, 316, CARD_W - 32, 317), Palette::BORDER_SOFT);

    SelectObject(hdc, f_small.into());
    draw_text_left(
        hdc,
        "公众号 · 微信搜一搜「艺杯羹」",
        at(card, 32, 330, 292, 350),
        Palette::TEXT_SUB,
    );
    draw_text_left(
        hdc,
        "赞赏码 · 请作者喝杯茶",
        at(card, 308, 330, 568, 350),
        Palette::TEXT_SUB,
    );

    // 两个"格子"里画图：等比缩放 + 居中，不拉伸变形。
    let slot_official = at(card, 32, 350, 292, 500);
    let slot_reward = at(card, 308, 350, 568, 500);
    IMAGES.with(|cell| {
        let mut imgs = cell.borrow_mut();
        if !imgs.loaded {
            imgs.loaded = true;
            // 只在第一次绘制时解码；解码失败（极少数精简版系统缺 WIC）
            // 就留空——文字信息（微信 / QQ / GitHub）仍然完整可用。
            imgs.official = decode_png(include_bytes!("../assets/qr_official.png"));
            imgs.reward = decode_png(include_bytes!("../assets/qr_reward.png"));
        }
        if let Some(img) = &imgs.official {
            draw_image(hdc, img, slot_official);
        }
        if let Some(img) = &imgs.reward {
            draw_image(hdc, img, slot_reward);
        }
    });

    // ---- 7.5 版本号与构建时间（规格 E：关于页标注版本，便于用户反馈问题时报版本）----
    //
    // 【y 坐标取值依据】卡片总高 CARD_H = 526，最后一行内容（两张二维码的
    // 摆放格子）底边在 500（见上面 slot_official / slot_reward 的 at(..., 500)），
    // 也就是说卡片底部还剩 526 − 500 = 26px 的空隙。这里取 504~522：
    // 上边 504 = 二维码底边 500 再留 4px，避免小字贴图太近；
    // 下边 522 = 卡片底边 526 再留 4px，保证整行文字都在卡片内不越界。
    // 行高 522 − 504 = 18px，大于 13px 字高，配合 DT_VCENTER 文字在行内垂直居中。
    // （左右沿用卡片 32px 内边距，水平居中由 draw_text_center 的 DT_CENTER 完成。）
    //
    // 【字体】复用第 2 步创建的 f_small（13px 常规字重）——这里显式再 Select 一次，
    // 和本文件其它分段"每次用前先选字体"的既有写法保持一致；SelectObject 返回的
    // 旧字体句柄与第 8 步统一恢复的 old_font 是同一套流程，不会泄漏 GDI 对象。
    SelectObject(hdc, f_small.into());
    draw_text_center(
        hdc,
        // 版本号取 Cargo.toml 的版本（编译期宏，零运行时开销），
        // 构建时间复用 main.rs 的 build_ts_text（读 HELLOGATE_BUILD_TS 环境变量）。
        &format!(
            "HelloGate v{} · 构建于 {}",
            env!("CARGO_PKG_VERSION"),
            crate::build_ts_text()
        ),
        at(card, 32, 504, CARD_W - 32, 522),
        Palette::TEXT_MUTED,
    );

    // ---- 8. 收尾：把字体换回去再删除 ----
    SelectObject(hdc, old_font);
    let _ = DeleteObject(f_title.into());
    let _ = DeleteObject(f_name.into());
    let _ = DeleteObject(f_small.into());
    let _ = DeleteObject(f_value.into());

    let _ = EndPaint(hwnd, &ps);
}

/// 创建字体（和主控台用同一套字体族与渲染方式，保证两处文字观感一致）。
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

/// 把一张位图等比缩放、居中画进指定区域。
/// pub(crate)：授权码激活窗口（license.rs）复用同一套"等比缩放居中"画法。
pub(crate) unsafe fn draw_image(hdc: HDC, img: &QrImage, slot: RECT) {
    let slot_w = (slot.right - slot.left).max(1);
    let slot_h = (slot.bottom - slot.top).max(1);
    // 取宽、高两个方向缩放比的**较小值**：保证图完整放得下，又不会变形。
    let scale = (slot_w as f32 / img.w as f32).min(slot_h as f32 / img.h as f32);
    let dw = ((img.w as f32) * scale).round() as i32;
    let dh = ((img.h as f32) * scale).round() as i32;
    let dx = slot.left + (slot_w - dw) / 2;
    let dy = slot.top + (slot_h - dh) / 2;

    let mem = CreateCompatibleDC(Some(hdc));
    if mem.is_invalid() {
        return;
    }
    let old = SelectObject(mem, img.hbmp.into());
    // HALFTONE：缩放时做颜色平均。二维码是由密集黑白方格组成的，
    // 用默认的 BLACKONWHITE 模式缩小会把码点糊成噪点，可能扫不出来。
    let _ = SetStretchBltMode(hdc, HALFTONE);
    let _ = StretchBlt(
        hdc,
        dx,
        dy,
        dw,
        dh,
        Some(mem),
        0,
        0,
        img.w,
        img.h,
        SRCCOPY,
    );
    SelectObject(mem, old);
    let _ = DeleteDC(mem);
}

// ============================================================================
// PNG 解码（WIC）
// ============================================================================

/// 把编译进 exe 的 PNG 字节解码成一张 32 位位图。
/// pub(crate)：授权码激活窗口（license.rs）要解码同一张公众号二维码。
///
/// # 步骤
/// 1. 把字节数组包成 COM 的 `IStream` —— WIC 只认流，不认"内存指针"；
/// 2. 建一个 `IWICImagingFactory`（系统里的图片解码工厂）；
/// 3. 从流里打开解码器，取出第一帧；
/// 4. 转成 `32bppPBGRA`：这个格式的内存布局和 Windows 32 位 DIB 完全一致，
///    可以整块拷贝，不用逐像素换算；
/// 5. 建一个自上而下的 DIB 区（`biHeight` 为负），把像素拷进去。
///
/// 返回 `None` 表示系统不支持或图片损坏；调用方会安静地跳过图片。
pub(crate) unsafe fn decode_png(bytes: &'static [u8]) -> Option<QrImage> {
    // 确保当前线程初始化过 COM。重复调用无害：
    // 已初始化时返回 S_FALSE 或 RPC_E_CHANGED_MODE，两种情况都说明"能用"。
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

    let stream = SHCreateMemStream(Some(bytes))?;

    let factory: IWICImagingFactory =
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
    let decoder = factory
        .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
        .ok()?;
    let frame = decoder.GetFrame(0).ok()?;

    // 统一转成 32 位预乘 BGRA（见函数头第 4 步）。
    let conv = factory.CreateFormatConverter().ok()?;
    conv.Initialize(
        &frame,
        &GUID_WICPixelFormat32bppPBGRA,
        WICBitmapDitherTypeNone,
        None::<&IWICPalette>,
        0.0,
        WICBitmapPaletteTypeCustom,
    )
    .ok()?;

    let (mut w, mut h) = (0u32, 0u32);
    conv.GetSize(&mut w, &mut h).ok()?;
    if w == 0 || h == 0 {
        return None;
    }

    let mut bmi: BITMAPINFO = std::mem::zeroed();
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w as i32,
        // 负高度 = 自上而下存放。WIC 输出的扫描行就是自上而下的，
        // 用正高度会得到一张上下颠倒的图。
        biHeight: -(h as i32),
        biPlanes: 1,
        biBitCount: 32,
        // BI_RGB = 不压缩；这个版本的 windows crate 把该字段声明成 u32，
        // 所以取枚举里的原始值（0）。
        biCompression: BI_RGB.0,
        ..std::mem::zeroed()
    };

    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let hbmp = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(hbmp.into());
        return None;
    }

    let len = (w as usize) * (h as usize) * 4;
    let buf = std::slice::from_raw_parts_mut(bits as *mut u8, len);
    conv.CopyPixels(std::ptr::null(), w * 4, buf).ok()?;

    Some(QrImage {
        hbmp,
        w: w as i32,
        h: h as i32,
    })
}

/// 把 Rust 字符串转成 NUL 结尾的 UTF-16（Win32 API 只认这个）。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
