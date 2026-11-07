//! 授权码激活（v1.0.0 新增）
//!
//! # 这是干什么的
//! HelloGate 免费开源，但作者希望用户先关注微信公众号「艺杯羹」、
//! 发送关键词「文章」领取授权码，输入后才可使用软件。
//! 激活是**一次性**的：成功后把状态写进配置（DPAPI 加密落盘），之后不再询问。
//!
//! # 为什么源码里只存哈希、不存明文授权码
//! 软件是开源的（GitHub / Gitee / GitCode 三平台都能看到源码）。
//! 如果把 `1588` 这样的明文写进代码，等于形同虚设——看源码就知道答案。
//! 存 SHA-256 哈希后，普通用户（本模块防的目标）无法从哈希反推授权码；
//! 有心人虽然能改二进制绕过，但那是所有本地授权机制的共同上限，
//! 与"锁住家里小孩不乱点软件"这个产品定位无关。
//!
//! # 交互流程
//! 1. 首次启动（配置里没有激活标记）→ 弹出自绘激活卡片；
//! 2. 卡片左侧是公众号二维码（复用 about.rs 编进 exe 的那张图），
//!    右侧是授权码输入框 + 「激活」按钮；
//! 3. 输对了：写配置、关窗、继续启动；输错了：红字提示，就地重试；
//! 4. 点 ✕ / 按 Esc / 关窗：视为放弃，软件直接退出（下次启动还会再问）。

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, EndPaint, InvalidateRect,
    SelectObject, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, FF_DONTCARE, FW_NORMAL,
    FW_SEMIBOLD, HFONT, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
};
// GetDpiForSystem 在 HiDpi 模块（与 console.rs 的导入位置一致）。
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CallWindowProcW, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GetClientRect, GetDlgItem, GetParent, GetSystemMetrics, GetWindowTextW,
    GetMessageW, IsDialogMessageW, LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassW,
    SendMessageW, SetCursor, SetForegroundWindow, SetWindowTextW, SetWindowLongPtrW, SetWindowPos,
    TranslateMessage, BS_OWNERDRAW, CS_HREDRAW, CS_VREDRAW, EN_KILLFOCUS, EN_SETFOCUS, GWLP_WNDPROC,
    HMENU, HTCAPTION, IDC_ARROW, IDC_HAND, IDCANCEL, IDOK, MSG, SM_CXSCREEN, SM_CYSCREEN,
    SWP_SHOWWINDOW, WM_CHAR, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_DRAWITEM, WM_ERASEBKGND,
    WM_KEYDOWN, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_NCHITTEST, WM_PAINT, WM_SETFONT,
    WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW, WS_CHILD, WS_CLIPCHILDREN, WS_EX_TOPMOST, WS_POPUP,
    WS_TABSTOP, WS_VISIBLE,
};

use crate::about;
use crate::config::Config;
use crate::ui::{
    create_input, draw_button, draw_card_soft_on, draw_close_glyph, draw_logo, draw_text_center,
    draw_text_left, draw_input_state, fill_solid, InputState, Palette,
};

// ============================================================================
// 授权码校验（SHA-256）
// ============================================================================

/// 授权码的 SHA-256 哈希（小写十六进制）。
///
/// 【它是怎么来的】`sha256("1588")`，用任意在线工具或 PowerShell
/// `Get-FileHash` 都能验证。源码公开，但公开哈希 ≠ 公开授权码。
const LICENSE_HASH: &str = "d24eac45e69be063cc0053eb02650954eec62c314c405e564a4d11e951392e75";

/// 校验用户输入的授权码是否正确（去首尾空白后比对 SHA-256）。
fn check_license(input: &str) -> bool {
    match sha256_hex(input.trim()) {
        Some(hex) => hex.eq_ignore_ascii_case(LICENSE_HASH),
        // CNG 初始化失败（极老的精简系统）：宁可拒绝激活，也不放行。
        None => false,
    }
}

/// 用 Windows 自带的 CNG（bcrypt.dll）算 SHA-256，返回小写十六进制字符串。
///
/// 【为什么不自己手写 SHA-256】项目里主密码的 PBKDF2 已经在用同一套 CNG
/// （见 password.rs），复用系统实现：代码少、常数表不可能抄错、还免审计。
fn sha256_hex(msg: &str) -> Option<String> {
    use windows::Win32::Security::Cryptography::{
        BCryptCloseAlgorithmProvider, BCryptCreateHash, BCryptDestroyHash, BCryptFinishHash,
        BCryptHashData, BCryptOpenAlgorithmProvider, BCRYPT_ALG_HANDLE, BCRYPT_HASH_HANDLE,
        BCRYPT_SHA256_ALGORITHM,
    };

    unsafe {
        // 1. 打开 SHA-256 算法提供者（不传 HMAC 标志 = 普通哈希模式）。
        let mut halg = BCRYPT_ALG_HANDLE::default();
        if BCryptOpenAlgorithmProvider(
            &mut halg,
            BCRYPT_SHA256_ALGORITHM,
            None,
            Default::default(),
        )
        .is_err()
        {
            return None;
        }

        // 2. 建一个哈希对象（哈希对象内存与密钥都传 None = 让系统自己分配）。
        let mut hhash = BCRYPT_HASH_HANDLE::default();
        let r = BCryptCreateHash(halg, &mut hhash, None, None, 0);
        let _ = BCryptCloseAlgorithmProvider(halg, 0);
        if r.is_err() {
            return None;
        }

        // 3. 喂数据、取结果（SHA-256 输出固定 32 字节，无需再问长度）。
        let mut out = [0u8; 32];
        if BCryptHashData(hhash, msg.as_bytes(), 0).is_err() {
            let _ = BCryptDestroyHash(hhash);
            return None;
        }
        let r = BCryptFinishHash(hhash, &mut out, 0);
        let _ = BCryptDestroyHash(hhash);
        if r.is_err() {
            return None;
        }

        // 4. 拼成小写十六进制。
        Some(out.iter().map(|b| format!("{b:02x}")).collect())
    }
}

// ============================================================================
// 窗口基础设施
// ============================================================================

/// 激活窗口类名。
const LIC_CLASS: &str = "HelloGateLicenseClass";
/// 授权码输入框控件 ID。
const LIC_IDC_EDIT: i32 = 4001;

/// 窗口尺寸（96 DPI 逻辑像素）。
const LIC_W: i32 = 460;
const LIC_H: i32 = 446;
/// 白色顶栏高度。
const LIC_HEADER: i32 = 64;

/// 本窗口的缩放基准（96 = 100%）。
///
/// 【为什么不复用 console 的 sc()】激活框在**主控台还没打开**时就要弹，
/// 而 console 的 SYSTEM_DPI 是在主控台创建前才探测的。这里自己读一次
/// 系统 DPI 即可——小窗口不存在"装不下要压缩"的问题，直接用系统值。
static LIC_DPI: AtomicU32 = AtomicU32::new(96);

/// 逻辑像素 → 物理像素（本模块专用换算）。
fn ls(logical: i32) -> i32 {
    crate::ui::scale(logical, LIC_DPI.load(Ordering::Relaxed) as i32)
}

thread_local! {
    /// 激活结果：true = 授权码正确并已落盘。
    static LIC_RESULT: Cell<bool> = const { Cell::new(false) };
    /// 上一次输入是否正确（false 时输入框画成淡红错误态）。
    static LIC_FAILED: Cell<bool> = const { Cell::new(false) };
    /// 错误提示文字（画在输入框下方）。
    static LIC_ERR: RefCell<String> = const { RefCell::new(String::new()) };
    /// 输入框是否持有焦点（决定外框颜色）。
    static LIC_FOCUSED: Cell<bool> = const { Cell::new(false) };
    /// 鼠标是否悬停在 ✕ 上（决定 ✕ 的颜色）。
    static LIC_HOVER_CLOSE: Cell<bool> = const { Cell::new(false) };
    /// 公众号二维码（第一次绘制时解码，之后复用）。
    static LIC_QR: RefCell<Option<about::QrImage>> = const { RefCell::new(None) };
}

/// 子类化 Edit 用的原窗口过程地址（回车 → 提交，见 password.rs 的 pwd_edit_proc 同款做法）。
/// 原窗口过程是 64 位指针，用 AtomicUsize 存裸地址值。
static ORIG_EDIT_PTR: AtomicUsize = AtomicUsize::new(0);

/// ✕ 关闭按钮的点击热区（物理像素，窗口坐标）。
fn close_rect(client_right: i32) -> RECT {
    RECT {
        left: client_right - ls(52),
        top: 0,
        right: client_right,
        bottom: ls(LIC_HEADER),
    }
}

/// 判断点是否在矩形内。
fn point_in(rc: RECT, x: i32, y: i32) -> bool {
    x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom
}

/// 输入框外框矩形（物理像素，父窗坐标）。
///
/// 卡片内布局的"唯一事实来源"：创建 Edit 和绘制外框都取这个矩形。
fn input_frame_rect(card: RECT) -> RECT {
    RECT {
        left: card.left + ls(40),
        top: card.top + ls(222),
        right: card.left + ls(300),
        bottom: card.top + ls(262),
    }
}

/// 「激活」按钮矩形（物理像素）。
fn ok_btn_rect(card: RECT) -> RECT {
    RECT {
        left: card.left + ls(312),
        top: card.top + ls(222),
        right: card.left + ls(404),
        bottom: card.top + ls(262),
    }
}

/// 卡片矩形（白色圆角主体，物理像素）。
fn card_rect(client: RECT) -> RECT {
    RECT {
        left: ls(16),
        top: ls(80),
        right: client.right - ls(16),
        bottom: ls(420),
    }
}

// ============================================================================
// 对外入口
// ============================================================================

/// 确保软件已激活；未激活时弹激活框（阻塞）。
///
/// 返回值：true = 已激活（可以继续启动）；false = 用户取消（调用方应退出进程）。
/// 成功时顺手把 `cfg.license_activated` 置真并保存配置。
pub fn ensure_activated(cfg: &mut Config) -> bool {
    if cfg.license_activated {
        return true;
    }
    let ok = unsafe { show_dialog() };
    if ok {
        cfg.license_activated = true;
        if let Err(e) = crate::config::save(cfg) {
            crate::logln!("[license] 激活状态保存失败: {e}");
        }
        crate::logln!("[license] 授权码验证通过，已永久激活");
    } else {
        crate::logln!("[license] 用户取消激活，进程退出");
    }
    ok
}

/// 建窗 + 模态消息循环，返回是否激活成功。
unsafe fn show_dialog() -> bool {
    // 复位上次残留的状态（thread_local 在同一线程上跨窗口存活）。
    LIC_RESULT.with(|c| c.set(false));
    LIC_FAILED.with(|c| c.set(false));
    LIC_ERR.with(|e| e.borrow_mut().clear());
    LIC_FOCUSED.with(|c| c.set(false));
    LIC_HOVER_CLOSE.with(|c| c.set(false));

    // 读一次系统 DPI 作为本窗口的缩放基准。
    let dpi = GetDpiForSystem().max(96);
    LIC_DPI.store(dpi, Ordering::Relaxed);

    // 1. 注册窗口类（重复注册返回 0，说明类已存在，直接用即可）。
    let class_name = wide(LIC_CLASS);
    let Ok(hinstance) = GetModuleHandleW(None) else {
        return false;
    };
    let bg_brush = CreateSolidBrush(COLORREF(Palette::BG));
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(lic_wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: bg_brush.into(),
        hIcon: crate::ui::load_app_icon(),
        ..Default::default()
    };
    if RegisterClassW(&wc) == 0 {
        // 类已存在：刚创建的画刷没人引用，主动释放防 GDI 泄漏（同 password.rs）。
        let _ = DeleteObject(bg_brush.into());
    }

    // 2. 创建窗口：WS_POPUP 无边框自绘 + 置顶（首次启动时保证用户看得见它）。
    let title = wide("HelloGate - 激活");
    let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOPMOST,
        PCWSTR(class_name.as_ptr()),
        PCWSTR(title.as_ptr()),
        WS_POPUP | WS_VISIBLE | WS_CLIPCHILDREN,
        0,
        0,
        ls(LIC_W),
        ls(LIC_H),
        None,
        None,
        Some(hinstance.into()),
        None,
    ) else {
        return false;
    };
    crate::ui::round_window(hwnd);

    // 居中到主屏（负坐标钳到 0，防高缩放小屏把窗口推出屏幕外）。
    let sw = GetSystemMetrics(SM_CXSCREEN);
    let sh = GetSystemMetrics(SM_CYSCREEN);
    let _ = SetWindowPos(
        hwnd,
        None,
        ((sw - ls(LIC_W)) / 2).max(0),
        ((sh - ls(LIC_H)) / 2).max(0),
        ls(LIC_W),
        ls(LIC_H),
        SWP_SHOWWINDOW,
    );

    // 3. 创建子控件（输入框 + 激活按钮），并聚焦输入框。
    create_controls(hwnd);

    let _ = BringWindowToTop(hwnd);
    let _ = SetForegroundWindow(hwnd);

    // 4. 模态消息循环：IsDialogMessageW 提供 Tab / Esc 的对话框行为。
    let mut msg = MSG::default();
    loop {
        let r = GetMessageW(&mut msg, None, 0, 0);
        if !r.as_bool() {
            break; // WM_QUIT：窗口销毁时 PostQuitMessage 发出
        }
        if !IsDialogMessageW(hwnd, &msg).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    LIC_RESULT.with(|c| c.get())
}

// ============================================================================
// 子控件创建
// ============================================================================

/// 创建输入框与「激活」按钮（明文显示——授权码只有几位，遮成圆点反而没法核对）。
unsafe fn create_controls(hwnd: HWND) {
    let hinstance = GetModuleHandleW(None).unwrap_or_default();
    let font = make_font(-ls(15), FW_NORMAL.0 as i32);

    // ---------- 输入框（复用 ui::create_input 组件）----------
    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);
    let card = card_rect(client);
    let frame = input_frame_rect(card);
    let edit = create_input(hwnd, frame, LIC_DPI.load(Ordering::Relaxed) as i32, LIC_IDC_EDIT, false, font, 0);

    // 子类化输入框：按回车 = 点「激活」。
    // （普通 WS_POPUP 窗口里回车不会自动触发默认按钮，必须自己转发 WM_COMMAND。）
    if !edit.0.is_null() {
        let orig = SetWindowLongPtrW(
            edit,
            GWLP_WNDPROC,
            lic_edit_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT
                as isize,
        );
        ORIG_EDIT_PTR.store(orig as usize, Ordering::SeqCst);
        let _ = SetFocus(Some(edit));
    }

    // ---------- 「激活」按钮（自绘，外观与主控台主按钮一致）----------
    let btn_class = wide("Button");
    let ok_text = wide("激活");
    let okr = ok_btn_rect(card);
    if let Ok(btn) = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(btn_class.as_ptr()),
        PCWSTR(ok_text.as_ptr()),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
        okr.left,
        okr.top,
        okr.right - okr.left,
        okr.bottom - okr.top,
        Some(hwnd),
        Some(HMENU(IDOK.0 as *mut _)),
        Some(hinstance.into()),
        None,
    ) {
        let _ = SendMessageW(btn, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
    }
    // 注意：font 不删除——Edit/Button 仍在引用它。
    // 本对话框一个进程生命周期内只弹一次，泄漏一个字体对象的代价为零。
}

/// 回车转发：Edit 收到 WM_CHAR(13) 时给父窗发一条"点了激活"的 WM_COMMAND。
unsafe extern "system" fn lic_edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CHAR && (wparam.0 as u32) == 13 {
        if let Ok(parent) = GetParent(hwnd) {
            let _ = PostMessageW(Some(parent), WM_COMMAND, WPARAM(IDOK.0 as usize), LPARAM(0));
        }
        return LRESULT(0);
    }
    let orig = ORIG_EDIT_PTR.load(Ordering::SeqCst);
    if orig == 0 {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let f: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT =
        std::mem::transmute(orig);
    CallWindowProcW(Some(f), hwnd, msg, wparam, lparam)
}

// ============================================================================
// 窗口过程
// ============================================================================

unsafe extern "system" fn lic_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        // 全部自绘，别让系统先擦白底（防闪烁）。
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_NCHITTEST => {
            // 顶栏空白处可以拖动窗口（无边框窗口的标配）。
            let x = (lparam.0 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let mut pt = windows::Win32::Foundation::POINT { x, y };
            let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            if pt.y >= 0 && pt.y < ls(LIC_HEADER) && !point_in(close_rect(rc.right), pt.x, pt.y) {
                return LRESULT(HTCAPTION as isize);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            if point_in(close_rect(rc.right), x, y) {
                cancel(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            // ✕ 悬停变色 + 小手光标（其余区域恢复箭头）。
            let x = (lparam.0 & 0xFFFF) as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let over_close = point_in(close_rect(rc.right), x, y);
            if over_close != LIC_HOVER_CLOSE.with(|c| c.get()) {
                LIC_HOVER_CLOSE.with(|c| c.set(over_close));
                let _ = InvalidateRect(Some(hwnd), Some(&RECT { left: rc.right - ls(52), top: 0, right: rc.right, bottom: ls(LIC_HEADER) }), false);
            }
            let cursor = if over_close { IDC_HAND } else { IDC_ARROW };
            if let Ok(c) = LoadCursorW(None, cursor) {
                let _ = SetCursor(Some(c));
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            let code = (wparam.0 >> 16) as u32;
            if id == LIC_IDC_EDIT && (code == EN_SETFOCUS || code == EN_KILLFOCUS) {
                LIC_FOCUSED.with(|c| c.set(code == EN_SETFOCUS));
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }
            if id == IDOK.0 {
                try_activate(hwnd);
            } else if id == IDCANCEL.0 {
                cancel(hwnd);
            }
            LRESULT(0)
        }
        WM_DRAWITEM => {
            // 「激活」按钮的自绘（结构同主控台 draw_owner_item 的品牌主按钮）。
            let dis = &*(lparam.0 as *const windows::Win32::UI::Controls::DRAWITEMSTRUCT);
            let text = crate::ui::window_text(dis.hwndItem);
            let rc = dis.rcItem;
            let pressed = (dis.itemState.0
                & windows::Win32::UI::Controls::ODS_SELECTED.0)
                != 0;
            draw_button(
                dis.hDC,
                rc,
                Palette::CARD,
                if pressed { Palette::BRAND_DARK } else { Palette::BRAND },
                Palette::ON_BRAND,
                &text,
                None,
                // 胶囊圆角 = 半个按钮高。RoundRect 的宽高参数是"椭圆直径"，
                // 传超过半高的值会被钳成整椭圆（之前误传了按钮宽度）。
                (rc.bottom - rc.top) / 2,
            );
            LRESULT(1)
        }
        WM_KEYDOWN => {
            if wparam.0 == VK_ESCAPE.0 as usize {
                cancel(hwnd);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            cancel(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 校验输入框里的授权码。
unsafe fn try_activate(hwnd: HWND) {
    let edit = GetDlgItem(Some(hwnd), LIC_IDC_EDIT).unwrap_or_default();
    let mut buf = [0u16; 64];
    let len = GetWindowTextW(edit, &mut buf) as usize;
    let input = String::from_utf16_lossy(&buf[..len]);

    if check_license(&input) {
        LIC_RESULT.with(|c| c.set(true));
        let _ = DestroyWindow(hwnd);
        return;
    }

    // 错了不关窗：输入框变淡红 + 红字提示 + 清空重输（就地重试）。
    LIC_FAILED.with(|c| c.set(true));
    LIC_ERR.with(|e| {
        *e.borrow_mut() = if input.trim().is_empty() {
            "请输入授权码".to_string()
        } else {
            "授权码不正确，请核对公众号回复的内容".to_string()
        }
    });
    let _ = SetWindowTextW(edit, PCWSTR::null());
    let _ = SetFocus(Some(edit));
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// 用户取消（✕ / Esc / 关窗）：结果保持 false，关窗退出模态循环。
unsafe fn cancel(hwnd: HWND) {
    LIC_RESULT.with(|c| c.set(false));
    let _ = DestroyWindow(hwnd);
}

// ============================================================================
// 绘制
// ============================================================================

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut client = RECT::default();
    let _ = GetClientRect(hwnd, &mut client);

    // ---- 字体（本次绘制专用，画完统一释放）----
    let f_title = make_font(-ls(16), FW_SEMIBOLD.0 as i32);
    let f_body = make_font(-ls(14), FW_NORMAL.0 as i32);
    let f_small = make_font(-ls(12), FW_NORMAL.0 as i32);
    let old_font = SelectObject(hdc, f_title.into());

    // ---- 0. 整窗背景 ----
    // 【修复】WM_ERASEBKGND 返回 1 后系统不再擦背景，顶栏/卡片之间的空隙
    // 必须自己填色，否则截图与肉眼都会看到黑色缝隙。
    fill_solid(hdc, client, Palette::BG);

    // ---- 1. 白色顶栏 + Logo + 标题 + ✕ ----
    fill_solid(hdc, RECT { left: 0, top: 0, right: client.right, bottom: ls(LIC_HEADER) }, Palette::CARD);
    draw_logo(hdc, RECT { left: ls(20), top: ls(18), right: ls(48), bottom: ls(46) });
    draw_text_left(
        hdc,
        "激活 HelloGate",
        RECT { left: ls(58), top: 0, right: client.right - ls(60), bottom: ls(LIC_HEADER) },
        Palette::TEXT,
    );
    let hover_close = LIC_HOVER_CLOSE.with(|c| c.get());
    draw_close_glyph(
        hdc,
        client.right - ls(26),
        ls(LIC_HEADER / 2),
        ls(7),
        ls(2),
        if hover_close { Palette::DANGER } else { Palette::TEXT_SUB },
    );

    // ---- 2. 白色卡片 ----
    let card = card_rect(client);
    draw_card_soft_on(hdc, card, ls(12), Palette::BG);

    // ---- 3. 引导文案（卡片内相对坐标）----
    SelectObject(hdc, f_body.into());
    draw_text_center(
        hdc,
        "微信关注公众号「艺杯羹」，发送「文章」获取授权码",
        RECT { left: card.left + ls(24), top: card.top + ls(18), right: card.right - ls(24), bottom: card.top + ls(42) },
        Palette::TEXT_SUB,
    );

    // ---- 4. 公众号二维码（复用 about.rs 编进 exe 的那张横版图）----
    let slot = RECT {
        left: card.left + ls(80),
        top: card.top + ls(52),
        right: card.right - ls(80),
        bottom: card.top + ls(204),
    };
    LIC_QR.with(|cell| {
        let mut qr = cell.borrow_mut();
        if qr.is_none() {
            // 解码失败（精简系统缺 WIC）就留白，不影响输入授权码。
            *qr = about::decode_png(include_bytes!("../assets/qr_official.png"));
        }
        if let Some(img) = &*qr {
            about::draw_image(hdc, img, slot);
        }
    });

    // ---- 5. 输入框外框（Edit 本体是子窗口，只画它底下的圆角框）----
    let failed = LIC_FAILED.with(|c| c.get());
    let focused = LIC_FOCUSED.with(|c| c.get());
    let frame = input_frame_rect(card);
    let state = if failed {
        InputState::Danger
    } else if focused {
        InputState::Focused
    } else {
        InputState::Normal
    };
    draw_input_state(hdc, frame, state);

    // ---- 6. 错误提示红字 ----
    SelectObject(hdc, f_small.into());
    let err = LIC_ERR.with(|e| e.borrow().clone());
    if !err.is_empty() {
        draw_text_left(
            hdc,
            &err,
            RECT { left: card.left + ls(40), top: card.top + ls(270), right: card.right - ls(24), bottom: card.top + ls(292) },
            Palette::DANGER,
        );
    }

    // ---- 7. 底部安心提示 ----
    SelectObject(hdc, f_small.into());
    draw_text_center(
        hdc,
        "激活一次，永久有效 · 关闭本窗口将退出软件",
        RECT { left: card.left + ls(24), top: card.top + ls(300), right: card.right - ls(24), bottom: card.top + ls(322) },
        Palette::TEXT_MUTED,
    );

    // ---- 收尾：还原字体并删除 ----
    SelectObject(hdc, old_font);
    let _ = DeleteObject(f_title.into());
    let _ = DeleteObject(f_body.into());
    let _ = DeleteObject(f_small.into());
    let _ = EndPaint(hwnd, &ps);
}

/// 创建字体（与主控台/关于页同款字体族，保证三处文字观感一致）。
unsafe fn make_font(height: i32, weight: i32) -> HFONT {
    let face = wide("Microsoft YaHei UI");
    CreateFontW(
        height, 0, 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET, OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY, FF_DONTCARE.0.into(), PCWSTR(face.as_ptr()),
    )
}

/// Rust 字符串 → NUL 结尾 UTF-16（Win32 API 只认这个）。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
