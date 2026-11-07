//! 检查更新（v1.0.0 新增）
//!
//! # 做什么
//! 主控台顶栏的「检查更新」入口 → 后台线程用 **WinHTTP**（Windows 自带）
//! 请求 GitHub Releases API 拿最新版本号 → 和本程序编译进去的版本对比：
//! - 已是最新：Toast 一句话反馈；
//! - 有新版：弹一张小卡片，列出 **GitHub / Gitee / GitCode** 三个下载入口
//!   （GitHub 慢的时候，国内用户可以走另外两个镜像站）；
//! - 网络失败：Toast 提示，不弹窗打断。
//!
//! # 为什么用 WinHTTP 而不是 curl/reqwest
//! 项目定位是"零依赖单 exe"。winhttp.dll 是系统组件（IE 时代就在了），
//! 用它不增加任何分发体积，也不会被杀软当成"捆绑第三方网络库"重点关照。
//!
//! # 为什么结果要 PostMessage 回主控台
//! 网络请求可能耗时数秒，绝不能卡住 UI 线程。后台线程算完后把结果
//! `Box` 成堆内存，指针塞进 `WM_UPDATE_RESULT` 的 lParam 投回主控台消息循环，
//! 由 UI 线程拆箱展示——这是 Win32 里最经典的"线程结果回投"模式。

use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryDataAvailable,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, WINHTTP_FLAG_SECURE,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, DeleteObject, EndPaint, InvalidateRect, SelectObject,
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, FF_DONTCARE, FW_NORMAL, FW_SEMIBOLD,
    HFONT, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SetFocus, TrackMouseEvent, VK_ESCAPE, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowRect, LoadCursorW,
    PostMessageW, RegisterClassW, SetCursor, ShowWindow, WM_CLOSE, WM_KEYDOWN,
    WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_PAINT, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
    CS_HREDRAW, CS_VREDRAW, IDC_ARROW, IDC_HAND, SW_SHOWNORMAL,
};

use crate::about;
use crate::console::sc;
use crate::toast;
use crate::ui::{
    draw_close_glyph, draw_text_left, fill_round, fill_solid, round_window, Palette,
};

/// 本程序版本（来自 Cargo.toml，编译期注入，永远和 exe 真实版本一致）。
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 后台线程 → 主控台的结果回投消息。
///
/// WM_APP+2/+3 已被 main.rs 占用（显示主控台 / Toast），这里用 +4。
pub const WM_UPDATE_RESULT: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 4;

/// 检查结果的三种形态（Box 后经 lParam 指针传回 UI 线程）。
pub enum Outcome {
    /// 已是最新（无需打扰用户，Toast 一句即可）。
    Latest,
    /// 发现新版本，字符串是 GitHub 上的 tag（形如 "v1.0.1"）。
    NewVersion(String),
    /// 检查失败（断网、被墙、限流等），字符串是给用户看的中文原因。
    Failed(String),
}

/// 防连点门闩：一次检查没返回之前，再点「检查更新」直接忽略。
/// （网络请求在后台线程跑，用原子布尔跨线程共享。）
static CHECKING: AtomicBool = AtomicBool::new(false);

// ============================================================================
// 对外入口
// ============================================================================

/// 点「检查更新」：起后台线程查 GitHub，结果用 WM_UPDATE_RESULT 投回 owner。
///
/// 立即返回，UI 不卡顿；重复点击被 CHECKING 门闩挡住。
pub fn spawn_check(owner: windows::Win32::Foundation::HWND) {
    if CHECKING.swap(true, Ordering::SeqCst) {
        // 上一次还没查完：Toast 提醒一下，免得用户以为没点上。
        toast::show(Some(owner), "正在检查更新，请稍候…");
        return;
    }
    // std::thread::spawn 的闭包要求 Send：HWND 是指针值，跨线程"只用来
    // PostMessage"是安全的（PostMessageW 本身线程安全，会帮你编组到目标线程）。
    let owner_raw = owner.0 as isize;
    std::thread::spawn(move || {
        let outcome = match fetch_latest_tag() {
            Ok(tag) => {
                if is_newer(&tag, CURRENT_VERSION) {
                    Outcome::NewVersion(tag)
                } else {
                    Outcome::Latest
                }
            }
            Err(e) => Outcome::Failed(e),
        };
        unsafe {
            // 把结果装箱成堆上的单个对象，指针塞进 lParam 投回 UI 线程。
            // 接收方（handle_result）负责 from_raw 拆箱并释放——一次投一次拆，不泄漏。
            let boxed = Box::into_raw(Box::new(outcome));
            let _ = PostMessageW(
                Some(windows::Win32::Foundation::HWND(owner_raw as *mut _)),
                WM_UPDATE_RESULT,
                WPARAM(0),
                LPARAM(boxed as isize),
            );
        }
    });
}

/// 主控台收到 WM_UPDATE_RESULT 时调用：拆箱、按结果分发反馈。
pub unsafe fn handle_result(owner: windows::Win32::Foundation::HWND, lparam: LPARAM) {
    CHECKING.store(false, Ordering::SeqCst);
    // 回收后台线程投来的堆对象（所有权在此终结，Box::from_raw 后自动释放）。
    let outcome = *Box::from_raw(lparam.0 as *mut Outcome);
    match outcome {
        Outcome::Latest => {
            toast::show(Some(owner), &format!("已是最新版本 v{CURRENT_VERSION}"));
        }
        Outcome::Failed(err) => {
            toast::show(Some(owner), &format!("检查更新失败：{err}"));
        }
        Outcome::NewVersion(tag) => {
            show_update_card(owner, tag);
        }
    }
}

// ============================================================================
// 版本比较
// ============================================================================

/// 判断 GitHub 上的 tag 是否比当前版本新。
///
/// 【规则】去掉 `v` 前缀，按 `.` 分段比数字：1.0.1 > 1.0.0，1.1.0 > 1.0.9。
/// 比不了（格式异常）时，只要 tag 和当前版本字符串不同就提示"有新版本"——
/// 宁可多提醒一次，也不漏掉真正的更新。
fn is_newer(tag: &str, current: &str) -> bool {
    let norm = |s: &str| s.trim().trim_start_matches('v').to_string();
    let (t, c) = (norm(tag), norm(current));
    if t == c {
        return false;
    }
    let seg = |s: &str| -> Vec<u64> {
        s.split('.').map(|p| p.parse::<u64>().unwrap_or(0)).collect()
    };
    let (ts, cs) = (seg(&t), seg(&c));
    for i in 0..ts.len().max(cs.len()) {
        let a = *ts.get(i).unwrap_or(&0);
        let b = *cs.get(i).unwrap_or(&0);
        if a != b {
            return a > b;
        }
    }
    // 数字段完全相等（比如 "1.0.0-rc1" 这种带后缀的 tag）：字符串不同就提示。
    true
}

// ============================================================================
// GitHub Releases API（WinHTTP）
// ============================================================================

/// 拉取 GitHub 最新 release 的 tag_name。
///
/// 接口：`GET https://api.github.com/repos/yibeigen/hellogate/releases/latest`
/// 返回 JSON 里的 `tag_name` 字段（形如 "v1.0.1"）。
/// GitHub API 强制要求带 User-Agent 头，否则返回 403——
/// WinHttpOpen 时设置的"代理字符串"就是请求的 User-Agent，一步到位。
fn fetch_latest_tag() -> Result<String, String> {
    // 把 &str 转成 Win32 要的 UTF-16 数组（这些字符串要活到 API 调用结束，
    // 所以先 wide() 存进局部 Vec，再把 .as_ptr() 包成 PCWSTR 传进去）。
    let ua = wide("HelloGate-Updater");
    let host = wide("api.github.com");
    let path = wide("/repos/yibeigen/hellogate/releases/latest");
    let verb = wide("GET");

    unsafe {
        // 会话：WINHTTP_ACCESS_TYPE_DEFAULT_PROXY = 跟随系统代理设置。
        let session = WinHttpOpen(
            PCWSTR(ua.as_ptr()),
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
            None,
            None,
            0,
        );
        if session.is_null() {
            return Err("无法初始化系统网络组件".to_string());
        }
        // 超时：解析/连接 8 秒、收发 10 秒。不设在 4 秒是为了容忍
        // 国内访问 GitHub API 偶发的高延迟，同时不至于无限挂着。
        let _ = WinHttpSetTimeouts(session, 8000, 8000, 10000, 10000);

        // 连接：443 = HTTPS 端口。
        let connect = WinHttpConnect(session, PCWSTR(host.as_ptr()), 443, 0);
        if connect.is_null() {
            let _ = WinHttpCloseHandle(session);
            return Err("无法连接 GitHub".to_string());
        }

        // 请求：WINHTTP_FLAG_SECURE 开启 TLS；版本号与 Referer 传 None；
        // 接受类型传 null（= WINHTTP_DEFAULT_ACCEPT_TYPES，windows crate 未导出该常量）。
        let request = WinHttpOpenRequest(
            connect,
            PCWSTR(verb.as_ptr()),
            PCWSTR(path.as_ptr()),
            None,
            None,
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        );
        if request.is_null() {
            let _ = WinHttpCloseHandle(connect);
            let _ = WinHttpCloseHandle(session);
            return Err("无法创建网络请求".to_string());
        }

        // 发送 → 接收响应。GET 没有请求体，可选数据全传 None/0。
        let send = WinHttpSendRequest(request, None, None, 0, 0, 0);
        let recv = send.and_then(|_| WinHttpReceiveResponse(request, std::ptr::null_mut()));
        if recv.is_err() {
            let _ = WinHttpCloseHandle(request);
            let _ = WinHttpCloseHandle(connect);
            let _ = WinHttpCloseHandle(session);
            return Err("GitHub 无响应（网络受限或被墙）".to_string());
        }

        // 读响应体：先问"还有多少字节可读"，有就分块读进 body。
        // 循环到某次可用字节为 0 = 读完。GitHub 的 release JSON 只有几 KB。
        let mut body: Vec<u8> = Vec::new();
        let mut read_failed = false;
        loop {
            let mut avail: u32 = 0;
            if WinHttpQueryDataAvailable(request, &mut avail).is_err() || avail == 0 {
                break;
            }
            let mut chunk = vec![0u8; avail as usize];
            let mut got: u32 = 0;
            if WinHttpReadData(
                request,
                chunk.as_mut_ptr() as *mut _,
                avail,
                &mut got,
            )
            .is_err()
            {
                read_failed = true;
                break;
            }
            chunk.truncate(got as usize);
            body.extend_from_slice(&chunk);
        }

        // 三个句柄按"从里到外"的顺序关闭（request → connect → session）。
        let _ = WinHttpCloseHandle(request);
        let _ = WinHttpCloseHandle(connect);
        let _ = WinHttpCloseHandle(session);

        if read_failed {
            return Err("下载响应失败".to_string());
        }
        let text = String::from_utf8_lossy(&body).to_string();

        // 用 serde_json 取 tag_name。限流（403）或错误页没有这个字段，
        // 统一报"GitHub 未返回版本号"，让用户稍后再试而不是干等。
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|_| "GitHub 返回了无法解析的内容".to_string())?;
        v.get("tag_name")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| "GitHub 未返回版本号（可能访问过于频繁，请稍后再试）".to_string())
    }
}

/// Rust 字符串 → NUL 结尾 UTF-16。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ============================================================================
// 「发现新版本」卡片（owned 弹出窗口，非阻塞）
// ============================================================================
//
// 样式与 about.rs 的浮层同源：WS_POPUP + 主控台为 owner，
// 保证永远盖在主控台之上、主控台关闭时自动销毁，不需要额外生命周期管理。

/// 卡片窗口类名。
const CARD_CLASS: &str = "HelloGateUpdateCardClass";
/// `WM_MOUSELEAVE`（windows crate 未导出，见 console.rs 同名常量说明）。
const WM_MOUSELEAVE: u32 = 0x02A3;

/// 卡片尺寸（96 DPI 逻辑像素）。
const CARD_W: i32 = 480;
const CARD_H: i32 = 300;

/// 三个下载入口（标题 / 副标题 / 网址）。
///
/// 顺序即展示顺序：GitHub 放第一（主仓库、附 exe 安装包），
/// 两个国内镜像放后（GitHub 慢时的备选，正是用户要的"下载慢可去另外两个平台"）。
const LINKS: [(&str, &str, &str); 3] = [
    (
        "GitHub 发布页",
        "推荐 · 直接下载 exe 安装包",
        crate::about::GITHUB_RELEASES_URL,
    ),
    (
        "Gitee 镜像",
        "国内网络下载更快",
        crate::about::GITEE_URL,
    ),
    (
        "GitCode 镜像",
        "国内网络下载更快",
        crate::about::GITCODE_URL,
    ),
];

thread_local! {
    /// 新版本 tag（卡片标题用）。
    static CARD_TAG: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    /// 悬停目标：0 = 无，1..3 = 链接行，4 = ✕。
    static CARD_HOVER: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

/// 链接行矩形（物理像素）。i = 0..3。
fn link_rect(client_right: i32, i: usize) -> RECT {
    let top = sc(100 + i as i32 * 58);
    RECT {
        left: sc(28),
        top,
        right: client_right - sc(28),
        bottom: top + sc(50),
    }
}

/// ✕ 热区矩形（物理像素）。
fn card_close_rect(client_right: i32) -> RECT {
    RECT {
        left: client_right - sc(52),
        top: 0,
        right: client_right,
        bottom: sc(56),
    }
}

fn point_in(rc: RECT, x: i32, y: i32) -> bool {
    x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom
}

/// 弹出「发现新版本」卡片（盖在主控台上，用户选平台或关掉即可）。
unsafe fn show_update_card(parent: windows::Win32::Foundation::HWND, tag: String) {
    // 登记卡片文案（paint 时读）。
    CARD_TAG.with(|t| *t.borrow_mut() = tag.clone());
    CARD_HOVER.with(|h| h.set(0));

    // 注册窗口类（只注册一次）。
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    let hinst = GetModuleHandleW(None).unwrap_or_default();
    let class_name = wide(CARD_CLASS);
    if !REGISTERED.load(Ordering::Relaxed) {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(card_wnd_proc),
            hInstance: hinst.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: windows::Win32::Graphics::Gdi::CreateSolidBrush(
                windows::Win32::Foundation::COLORREF(COLORREF_WRAP),
            )
            .into(),
            hIcon: crate::ui::load_app_icon(),
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
        REGISTERED.store(true, Ordering::Relaxed);
    }

    // 尺寸与位置：盖在主控台正中。
    let mut pr = RECT::default();
    let _ = GetWindowRect(parent, &mut pr);
    let w = sc(CARD_W);
    let h = sc(CARD_H);
    let x = pr.left + ((pr.right - pr.left) - w) / 2;
    let y = pr.top + ((pr.bottom - pr.top) - h) / 2;

    let title = wide("HelloGate - 检查更新");
    let Ok(card) = CreateWindowExW(
        WS_EX_TOOLWINDOW,
        PCWSTR(class_name.as_ptr()),
        PCWSTR(title.as_ptr()),
        WS_POPUP | WS_VISIBLE,
        x,
        y,
        w,
        h,
        Some(parent),
        None,
        Some(hinst.into()),
        None,
    ) else {
        return;
    };
    round_window(card);
    let _ = ShowWindow(card, SW_SHOWNORMAL);
    let _ = SetFocus(Some(card));
}

/// 卡片背景画刷用的颜色值（COLORREF 是 0x00BBGGRR，这里直接引 Palette::BG）。
const COLORREF_WRAP: u32 = Palette::BG;

unsafe extern "system" fn card_wnd_proc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        windows::Win32::UI::WindowsAndMessaging::WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint_card(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let x = (lparam.0 & 0xFFFF) as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let mut hover = 0u8;
            if point_in(card_close_rect(rc.right), x, y) {
                hover = 4;
            } else {
                for i in 0..3 {
                    if point_in(link_rect(rc.right, i), x, y) {
                        hover = (i + 1) as u8;
                        break;
                    }
                }
            }
            if hover != CARD_HOVER.with(|h| h.get()) {
                CARD_HOVER.with(|h| h.set(hover));
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            let cursor = if hover != 0 { IDC_HAND } else { IDC_ARROW };
            if let Ok(c) = LoadCursorW(None, cursor) {
                let _ = SetCursor(Some(c));
            }
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            LRESULT(0)
        }
        x if x == WM_MOUSELEAVE => {
            if CARD_HOVER.with(|h| h.get()) != 0 {
                CARD_HOVER.with(|h| h.set(0));
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            if point_in(card_close_rect(rc.right), x, y) {
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            for i in 0..3 {
                if point_in(link_rect(rc.right, i), x, y) {
                    // 用系统浏览器打开对应平台；卡片保持打开，方便再选别的。
                    about::open_url(LINKS[i].2);
                    break;
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if wparam.0 == VK_ESCAPE.0 as usize {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 画卡片：标题 + 副标题 + 分隔线 + 三行可点链接 + ✕。
unsafe fn paint_card(hwnd: windows::Win32::Foundation::HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);

    let f_big = make_font(-sc(17), FW_SEMIBOLD.0 as i32);
    let f_mid = make_font(-sc(14), FW_NORMAL.0 as i32);
    let f_sml = make_font(-sc(12), FW_NORMAL.0 as i32);
    let old = SelectObject(hdc, f_big.into());

    // 整窗铺白（卡片本体就是窗口本身，圆角由 round_window 裁）。
    fill_solid(hdc, client, Palette::CARD);

    // 标题 + 副标题
    let tag = CARD_TAG.with(|t| t.borrow().clone());
    draw_text_left(
        hdc,
        &format!("发现新版本 {tag}"),
        RECT { left: sc(28), top: sc(22), right: client.right - sc(56), bottom: sc(50) },
        Palette::TEXT,
    );
    SelectObject(hdc, f_sml.into());
    draw_text_left(
        hdc,
        &format!("当前版本 v{CURRENT_VERSION} · 选择一个平台下载更新"),
        RECT { left: sc(28), top: sc(54), right: client.right - sc(28), bottom: sc(74) },
        Palette::TEXT_MUTED,
    );

    // 分隔线
    fill_solid(
        hdc,
        RECT { left: sc(28), top: sc(88), right: client.right - sc(28), bottom: sc(89) },
        Palette::BORDER_STRONG,
    );

    // 三行链接（悬停行画淡灰底）
    let hover = CARD_HOVER.with(|h| h.get());
    for i in 0..3 {
        let r = link_rect(client.right, i);
        if hover == (i + 1) as u8 {
            fill_round(hdc, r, Palette::HOVER, sc(10));
        }
        SelectObject(hdc, f_mid.into());
        draw_text_left(
            hdc,
            LINKS[i].0,
            RECT { left: r.left + sc(16), top: r.top + sc(6), right: r.right - sc(16), bottom: r.top + sc(26) },
            Palette::TEXT,
        );
        SelectObject(hdc, f_sml.into());
        draw_text_left(
            hdc,
            LINKS[i].1,
            RECT { left: r.left + sc(16), top: r.top + sc(27), right: r.right - sc(16), bottom: r.bottom - sc(2) },
            Palette::TEXT_MUTED,
        );
    }

    // ✕
    draw_close_glyph(
        hdc,
        client.right - sc(26),
        sc(28),
        sc(7),
        sc(2),
        if hover == 4 { Palette::DANGER } else { Palette::TEXT_SUB },
    );

    SelectObject(hdc, old);
    let _ = DeleteObject(f_big.into());
    let _ = DeleteObject(f_mid.into());
    let _ = DeleteObject(f_sml.into());
    let _ = EndPaint(hwnd, &ps);
}

/// 创建字体（微软雅黑 UI，与主控台/关于页一致）。
unsafe fn make_font(height: i32, weight: i32) -> HFONT {
    let face = wide("Microsoft YaHei UI");
    CreateFontW(
        height, 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, FF_DONTCARE.0.into(), PCWSTR(face.as_ptr()),
    )
}
