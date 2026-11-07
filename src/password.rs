//! 主密码模块（安全回退方案）
//!
//! # 为什么需要这个模块
//! HelloGate 支持两种验证方式（见主控台的「认证方式」下拉框）：
//! - **系统验证**：Windows Hello（指纹 / 人脸 / PIN）；
//! - **自设密码**：用户自己定一个密码，3 位、6 位、任意长度都行。
//!
//! 程序**只保存 PBKDF2-SHA256 派生值 + 每机随机盐**（不存明文，
//! 见 DESIGN_AND_ROADMAP.md §3.2），两者再随配置被 DPAPI 加密落盘。
//! 验证时弹出本模块的原生密码框，用户输入的密码加盐派生后与存储值比对。
//!
//! # 忘记密码怎么办
//! 密码框左下角有「忘记密码？」按钮。点它后**不再需要抄写恢复码**，
//! 而是改用 Windows 身份验证（人脸/指纹/PIN，或 Windows 账户登录密码）——
//! 见 auth::verify_windows_identity。用户本来就是这台电脑的主人，
//! 借用系统已有的凭据验证身份，比"再记一串恢复码"简单得多。
//!
//! # 零第三方依赖
//! PBKDF2 / 随机盐用的是 Windows 自带的 CNG（bcrypt.dll），
//! 密码框用的是原生 Win32 控件，不引入任何 crate。

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, EndPaint, InvalidateRect, SelectObject,
    SetBkColor, SetBkMode, SetTextColor, HFONT, HDC, PAINTSTRUCT, CLEARTYPE_QUALITY,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, FF_DONTCARE, FW_NORMAL, FW_SEMIBOLD, OUT_DEFAULT_PRECIS,
    TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::Security::Cryptography::{
    BCryptCloseAlgorithmProvider, BCryptDeriveKeyPBKDF2, BCryptGenRandom,
    BCryptOpenAlgorithmProvider, BCRYPT_ALG_HANDLE, BCRYPT_ALG_HANDLE_HMAC_FLAG,
    BCRYPT_SHA256_ALGORITHM, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};
// DRAWITEMSTRUCT + ODS_SELECTED：自绘按钮时系统把"画什么、什么状态"打包发给我们
// （带 BS_OWNERDRAW 的控件不再自己画自己，见 pwd_wnd_proc 的 WM_DRAWITEM）。
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, ODS_SELECTED};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
// SetFocus / EnableWindow / IsWindowEnabled / ReleaseCapture 在 windows crate 里被归到
// KeyboardAndMouse 模块（反直觉，它们其实是窗口/输入函数）。
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, IsWindowEnabled, ReleaseCapture, SetFocus};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CallWindowProcW, CreateWindowExW, DefWindowProcW, DestroyWindow,
    GetClientRect, GetDlgItem, GetParent, GetSystemMetrics, GetWindowTextW, GetWindowRect,
    IsDialogMessageW,
    KillTimer, LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassW, SendMessageW,
    SetForegroundWindow, SetWindowTextW, SetTimer, SetWindowLongPtrW, SetWindowPos,
    TranslateMessage, DispatchMessageW, BS_OWNERDRAW, CS_HREDRAW, CS_VREDRAW, GWLP_WNDPROC, HMENU,
    HTCAPTION, IDCANCEL, IDOK, GetMessageW, MSG, SM_CXSCREEN,
    // SWP_NOSIZE / SWP_NOZORDER / SWP_NOACTIVATE：抖动时只挪 X 坐标，
    // 不改窗口大小、不动层级、不抢焦点（方案 A3）。
    SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, WNDCLASSW, WINDOW_EX_STYLE, WINDOW_STYLE, WS_CHILD,
    WS_CLIPCHILDREN, WS_EX_TOPMOST, WS_POPUP, WS_TABSTOP, WS_VISIBLE, WM_CHAR, WM_CLOSE,
    WM_COMMAND, WM_CREATE, WM_CTLCOLOREDIT, WM_CTLCOLORSTATIC, WM_DESTROY, WM_DRAWITEM,
    WM_LBUTTONDOWN, WM_NCLBUTTONDOWN, WM_PAINT, WM_SETFONT, WM_TIMER,
};

use crate::logln;
// 配色与基础绘图原语来自共用模块 ui.rs：解锁弹窗和主控台共用同一套按钮/卡片/圆角，
// 两个窗口看起来才像同一个软件，而不是各自拼出来的两套 UI。
use crate::ui::{
    create_input, draw_button, draw_card_soft, draw_close_glyph, draw_input_state, draw_logo,
    draw_text_left, fill_solid, round_window, solid_brush, window_text, InputState, Palette,
};

// ============================================================================
// 主密码派生：PBKDF2-SHA256 + 每机随机盐（DESIGN_AND_ROADMAP.md §3.2）
// ============================================================================
//
// 【为什么是 PBKDF2 而不是裸 SHA-256】
// 裸哈希算得太快（显卡每秒可试上亿次），配置一旦泄露就能离线暴力猜密码。
// PBKDF2 把「一次哈希」变成「10 万次迭代哈希」，猜密码的成本直接放大十万倍。
// 再加「每机随机盐」：同一密码在两台电脑上得到完全不同的结果，
// 别人预先算好的「常见密码表」（彩虹表）就废了。
// 这正是文档点名要修正的 SecureAppLocker 缺陷（它用 DPAPI 可逆加密存主密码）。
//
// 【零第三方依赖】用 Windows 自带的 CNG（bcrypt.dll）实现，不引入任何 crate。

/// PBKDF2 迭代次数（文档规定 10 万次；本机实测约 100ms，用户无感）。
pub const PBKDF2_ITERATIONS: u64 = 100_000;

/// 盐的字节数（16 字节 = 128 位随机，足够保证每台机器唯一）。
const SALT_BYTES: usize = 16;

/// 生成一段随机盐，返回小写十六进制字符串（32 个字符）。
///
/// 用 CNG 的系统首选随机源，无需自己开算法句柄。
/// 失败返回空串——调用方据此中止保存，绝不静默降级成"无盐"。
pub fn generate_salt_hex() -> String {
    let mut buf = [0u8; SALT_BYTES];
    unsafe {
        if BCryptGenRandom(None, &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG).is_err() {
            logln!("[password] 错误：生成随机盐失败（BCryptGenRandom）");
            return String::new();
        }
    }
    bytes_to_hex(&buf)
}

/// 用 PBKDF2-SHA256 把「密码 + 盐」派生成 64 位小写十六进制字符串。
///
/// # 参数
/// - `password`：用户输入的明文密码（只在这一瞬间存在，用完立即丢弃）。
/// - `salt_hex`：设置密码时生成、随配置一起存的随机盐（十六进制）。
///
/// # 返回
/// - 成功：64 位小写十六进制（32 字节派生密钥）。
/// - 失败（盐格式非法 / 系统加密服务异常）：空字符串并写日志。
///   调用方把空串视为"派生失败"——存储值永远非空，所以绝不会误判成验证通过。
pub fn derive_password_hex(password: &str, salt_hex: &str) -> String {
    let Some(salt) = hex_to_bytes(salt_hex) else {
        logln!("[password] 错误：盐格式非法（应为十六进制字符串）");
        return String::new();
    };

    let mut dk = [0u8; 32]; // 派生密钥：32 字节
    unsafe {
        // 1. 打开 SHA-256 算法句柄，并带 HMAC 标志——
        //    PBKDF2 的定义就是「以 HMAC 为内核反复迭代」，不加这个标志句柄不可用。
        let mut hprf = BCRYPT_ALG_HANDLE::default();
        if BCryptOpenAlgorithmProvider(
            &mut hprf,
            BCRYPT_SHA256_ALGORITHM,
            PCWSTR::null(),
            BCRYPT_ALG_HANDLE_HMAC_FLAG,
        )
        .is_err()
        {
            logln!("[password] 错误：无法初始化 CNG 加密服务（BCryptOpenAlgorithmProvider）");
            return String::new();
        }

        // 2. 一次调用完成 10 万次 HMAC-SHA256 迭代，输出 32 字节派生密钥。
        let st = BCryptDeriveKeyPBKDF2(
            hprf,
            Some(password.as_bytes()),
            Some(&salt),
            PBKDF2_ITERATIONS,
            &mut dk,
            0,
        );

        // 3. 用完立即关闭句柄（无论成败）。
        let _ = BCryptCloseAlgorithmProvider(hprf, 0);

        if st.is_err() {
            logln!("[password] 错误：PBKDF2 派生失败");
            return String::new();
        }
    }

    bytes_to_hex(&dk)
}

/// 字节 → 小写十六进制字符串。
fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 十六进制字符串 → 字节（盐在配置里以 hex 存储，用时还原）。
/// 长度必须为偶数且全部是合法 hex 字符，否则返回 None。
fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

// ============================================================================
// 原生密码输入框（模态）
// ============================================================================
//
// 【为什么自己写窗口，而不用 MessageBox/对话框模板？】
// MessageBox 没有密码输入框；标准对话框模板需要资源文件（.rc），
// 会 complicating 构建。直接 CreateWindowExW 搭一个 4 个控件的小窗口，
// 和主控台（console.rs）同一套路，风格也统一。
//
// 【调用环境】本函数由「认证线程」调用（见 main.rs 的 Unavailable 分支）。
// 那条线程本来就是独立线程，在这里跑自己的模态消息循环不会卡住任何 UI，
// 函数阻塞直到用户点「确定/取消」，返回结果。

/// 密码框窗口类名。
const PWD_CLASS: &str = "HelloGatePasswordClass";

/// 输入框控件 ID。
const IDC_PWD_EDIT: i32 = 3001;
/// 错误提示文字控件 ID（密码错时，红字显示在输入框正下方）。
const IDC_PWD_ERR: i32 = 3002;
/// 「忘记密码？」按钮控件 ID（仅配置了恢复码时才创建）。
const IDC_PWD_FORGET: i32 = 3003;
/// 大字标题（如「解锁『Word』」）。
const IDC_PWD_TITLE: i32 = 3004;
/// 副标题（标题下的一行说明）。
const IDC_PWD_SUB: i32 = 3005;
/// 输入框右端「显示 / 隐藏」小按钮（只在密码框上出现）。
const IDC_PWD_EYE: i32 = 3006;
/// 防暴力尝试锁定计时器 ID（WM_TIMER 用，窗口内唯一即可）。
const PWD_TIMER_ID: usize = 1;

/// 【方案 A3 · 密码错误抖动】抖动动画的计时器 ID。
/// 必须和锁定计时器（PWD_TIMER_ID=1）用不同的 ID——
/// 同一个 ID 的 SetTimer 会互相覆盖，两个动画就打架了。
const SHAKE_TIMER_ID: usize = 2;

/// 抖动序列：右 6px → 左 6 → 右 6 → 左 6 → 归零。
/// 方案给的参数是"偏移 6px、约 120ms 一次、横向抖 2~3 次"，
/// 这里取 4 次换向 + 1 次归位，最后一步偏移为 0 —— 窗口必然回到原位。
const SHAKE_OFFSETS: [i32; 5] = [6, -6, 6, -6, 0];

/// 抖动步间隔（毫秒）。每步借一次 WM_TIMER 走正常消息循环，画面实时跟手。
const SHAKE_INTERVAL_MS: u32 = 100;

// Edit 控件发给父窗口的两个通知码（窗口过程据此切换输入框边框颜色）。
// 直接写数值是为了避免为两个常量多引入一组类型定义；
// 这些值是 Win32 从 Windows 3.1 起就没变过的稳定 ABI。
const EN_SETFOCUS: u32 = 0x0100;
const EN_KILLFOCUS: u32 = 0x0200;
// 说明：EM_SETMARGINS（设置输入框左右内边距）原来在这里定义，
// 组件化后由 ui::create_input 内部统一发送，调用方无需再关心。

// ============================================================================
// 弹窗参数与内部结果
// ============================================================================

/// 一次弹窗的完整文案与行为参数。
///
/// 【为什么抽成结构体】普通密码框和「忘记密码-恢复码」框共用同一套窗口代码，
/// 只有文案和细节行为不同（密码框打圆点、恢复码明文显示；密码框多一个
/// 「忘记密码？」按钮）。用一个参数结构体区分，避免复制粘贴第二份窗口代码。
struct PwdParams {
    /// 窗口标题栏文字（任务栏/Alt+Tab 可见）。
    caption: &'static str,
    /// 窗口内的大字标题。
    title: String,
    /// 标题下的一行说明。
    subtitle: String,
    /// 输入是否打圆点（密码=true；恢复码=false，让用户能核对抄没抄对）。
    masked: bool,
    /// 是否显示「忘记密码？」按钮（没配置恢复码时显示也没用，隐藏）。
    allow_forgot: bool,
    /// 确认按钮文字（密码框=「解锁」，恢复码框=「验证恢复码」）。
    ok_text: &'static str,
}

/// 弹窗的内部结局（比对外的 PromptOutcome 多一个「点了忘记密码」）。
enum PwdResult {
    /// 输入内容验证通过。
    Ok,
    /// 用户取消 / 关窗。
    Cancel,
    /// 用户点了「忘记密码？」，要求改用 Windows 身份验证。
    Forgot,
}

// 弹窗的内部状态。全部 thread_local：弹窗只在其所在线程的消息循环里被访问。
thread_local! {
    // 弹窗结果：None = 还没结束；Some(Ok) = 验证通过；Some(Cancel) = 取消；Some(Forgot) = 要求恢复。
    static PWD_RESULT: RefCell<Option<PwdResult>> = const { RefCell::new(None) };
    // 本次弹窗的文案与行为参数，创建窗口前放入，WM_CREATE 里取用。
    static PWD_PARAMS: RefCell<Option<PwdParams>> = const { RefCell::new(None) };
    // 验证函数，由调用方传入（见 prompt_password）。
    // 之所以要传进来放在这里，是因为「点确定 / 按回车」都发生在窗口过程内部，
    // 只有窗口过程拿得到它——这样验证错了才能**就地重试**，而不是关窗重来。
    static PWD_VERIFY: RefCell<Option<Box<dyn Fn(&str) -> bool>>> = const { RefCell::new(None) };
    // 上一次尝试是否失败（决定输入框画成白底还是淡红底）。
    static PWD_FAILED: Cell<bool> = const { Cell::new(false) };
    // 连续失败的次数（用于防暴力尝试：连续错 3 次起，逐步拉长锁定时间）。
    static FAIL_STREAK: Cell<u32> = const { Cell::new(0) };
    // 抖动动画（方案 A3）：当前步序（-1 = 没在抖）与开始抖时的窗口 X 坐标。
    // 【为什么存基准 X，而不是每步在当前位置上加减】相对累加会累积舍入/取整
    // 误差，抖完窗口就歪了；始终从"基准 + 偏移"定位，最后一步偏移为 0，
    // 保证精确回到原位。
    static SHAKE_STEP: Cell<i32> = const { Cell::new(-1) };
    static SHAKE_BASE_X: Cell<i32> = const { Cell::new(0) };
    // 本弹窗创建的字体句柄（存裸值）。窗口销毁时统一 DeleteObject 释放，
    // 否则每弹一次密码框就泄漏 2 个 GDI 对象，反复上锁会累积。
    static PWD_FONTS: RefCell<Vec<isize>> = const { RefCell::new(Vec::new()) };
    // 输入框当前是否获得焦点。
    // 【用途】获得焦点时把输入框外框画成品牌色——这是现代表单最直观的"我在这儿"提示，
    // 比系统默认那圈凹陷边框清楚得多。
    static PWD_FOCUSED: Cell<bool> = const { Cell::new(false) };
    // 当前是否把密码显示成明文（「显示 / 隐藏」按钮的状态）。
    // 【为什么要记账】按钮文字和输入框的遮罩字符都要跟着它变化，
    // 记在一个地方，两处就永远同步。
    static PWD_SHOWN: Cell<bool> = const { Cell::new(false) };
    // 本窗口的 DPI 缩放基准（96 = 100%）。
    // WM_PAINT 每次重绘都要用它换算坐标，缓存起来避免每帧调一次 GetDpiForWindow。
    static PWD_SCALE: Cell<i32> = const { Cell::new(96) };
    // 自绘按钮时选进 DC 的字体句柄（裸值，绕开 Send/Sync 限制）。
    // 【为什么必须存】自绘收到的 DC 不带我们设置的字体，不显式 SelectObject 的话，
    // 按钮文字会退回系统默认宋体，和旁边的 Static 文字明显不一致。
    static PWD_UI_FONT: Cell<isize> = const { Cell::new(0) };
}

/// 被子类化的 Edit 控件「原始窗口过程」的地址。
/// Edit 是系统类窗口，所有 Edit 的原始过程都相同，所以一个全局值就够。
static ORIG_EDIT_PROC: AtomicIsize = AtomicIsize::new(0);

/// 密码框的结局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptOutcome {
    /// 验证通过，可以解锁。
    Unlocked,
    /// 用户点「取消」/ 按 Esc / 关窗。
    Cancelled,
    /// 用户点了「忘记密码？」，要求改用 Windows 身份验证。
    Forgot,
}

/// 解锁用的凭据快照（主密码的存储值）。
///
/// 【为什么是快照】调用方（设置自锁 / 守门进程）在弹窗前从配置里
/// 拷贝出这几个字段，弹窗期间不再持配置锁；弹窗是长交互（用户可能发呆几分钟），
/// 提前拷走能让其它线程照常读写配置。
pub struct AuthSecrets {
    /// 主密码派生值（空 = 未设置）。
    pub pwd_hash: String,
    /// 主密码盐。
    pub pwd_salt: String,
    /// 当前密码是否还是「出厂初始密码」。
    ///
    /// 【为什么需要这个标志】出厂状态下的密码是公开的，等于还没上锁。
    /// 带着这个标志，调用方就能在**不弹验证框**的情况下直接放行，
    /// 并在界面上提示用户去主控台设置自己的密码。
    pub pwd_is_default: bool,
}

impl AuthSecrets {
    /// 是否已设置主密码（哈希和盐成对齐备才算，残缺按未设置处理）。
    pub fn has_password(&self) -> bool {
        !self.pwd_hash.is_empty() && !self.pwd_salt.is_empty()
    }
}

/// 「密码 + 忘记密码」完整交互的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockFlow {
    /// 主密码输入正确（或通过 Windows 身份验证找回了），可以解锁。
    Passed,
    /// 用户放弃（取消 / 密码始终不对）。
    Failed,
}

/// 完整的密码解锁流程（含「忘记密码」分支）。
///
/// # 交互流程
/// 1. 弹密码框，左下角有「忘记密码？」按钮；
/// 2. 密码对 → 返回 [`UnlockFlow::Passed`]；
/// 3. 点了「忘记密码？」→ 改用 **Windows 身份验证**
///    （Hello 人脸/指纹/PIN，不可用则 Windows 账户密码，见
///    `auth::verify_windows_identity`）；
///    - 通过 → 返回 [`UnlockFlow::Passed`]（本次放行，密码保持原样，
///      用户以后想改密码可以进主控台重新设置）；
///    - 取消/失败 → 退回密码框继续（用户可能只是误点）；
/// 4. 用户取消 → 返回 [`UnlockFlow::Failed`]。
///
/// # 参数
/// - `display`：被解锁对象的友好名（如「记事本」），用于弹窗标题。
/// - `secrets`：凭据快照（存储值，见 [`AuthSecrets`]）。
pub fn password_unlock_flow(display: &str, secrets: &AuthSecrets) -> UnlockFlow {
    // ★ 出厂初始状态 → 直接放行，不弹框、不拦截。
    //
    // 【为什么】这时挂着的初始密码是公开的（安装文档里写着），任何人都知道，
    // 拦住了等于没拦，只会让用户打开软件时白白多点一次「解锁」。
    // 更糟的是——如果把这串初始密码印在弹窗上（旧版就是这么干的），
    // 那才真的荒谬：能看到界面的人自然也就能用它解锁，等于把钥匙插在锁孔上。
    //
    // 真正有效的保护是「用户设置过自己的密码」。所以这一阶段：
    //   1. 界面上一律不出现密码值；
    //   2. 直接放行，用户进主控台设好密码后才真正开始拦截；
    //   3. 主控台会用琥珀色提示 + 开机托盘气泡提醒用户去设置。
    if secrets.pwd_is_default {
        logln!("[password] 尚未设置主密码（出厂初始状态）→ 直接放行，不拦截");
        return UnlockFlow::Passed;
    }

    let title = format!("解锁「{display}」");
    // 副标题只讲"该做什么"，绝不出现任何密码值。
    let subtitle = "请输入主密码，或点「忘记密码」用 Windows 身份验证".to_string();
    loop {
        // 密码框：校验闭包把存储值拷进去（'static 要求），派生比对。
        let (h, s) = (secrets.pwd_hash.clone(), secrets.pwd_salt.clone());
        // 「忘记密码？」始终显示：现在恢复手段是 Windows 身份验证，
        // 不再依赖用户手里是否有一串恢复码。
        let outcome = prompt_password(
            &title,
            &subtitle,
            move |pwd| derive_password_hex(pwd, &s) == h,
            true,
        );
        match outcome {
            PromptOutcome::Unlocked => return UnlockFlow::Passed,
            PromptOutcome::Cancelled => return UnlockFlow::Failed,
            PromptOutcome::Forgot => {
                logln!("[password] 用户点击「忘记密码」，改用 Windows 身份验证");
                if crate::auth::verify_windows_identity("HelloGate 密码找回") {
                    return UnlockFlow::Passed;
                }
                // 身份验证没通过/取消：回到密码框，用户可以再试密码。
                logln!("[password] Windows 身份验证未通过，返回密码框");
            }
        }
    }
}

/// 弹出密码输入框（模态，阻塞直到验证成功或取消）。
///
/// # 参数
/// - `title`：大字标题（如「解锁「记事本」」）。
/// - `subtitle`：一行说明（如「请输入密码」）。
/// - `verify`：校验函数——收到用户填的明文，返回对不对。
/// - `allow_forgot`：是否显示「忘记密码？」按钮（仅配置了恢复码时为 true）。
///
/// # 交互（学参考截图的「就地重试」）
/// 密码错了**不关窗**：输入框变淡红底、下方出红字、内容清空并自动聚焦。
/// 连续错 3 次起逐步锁定输入（2s → 4s → 8s … 上限 60s），防止有人拿
/// 短密码（比如 3 位纯数字）挨个试——见 `try_unlock` 的防暴力逻辑。
pub fn prompt_password(
    title: &str,
    subtitle: &str,
    verify: impl Fn(&str) -> bool + 'static,
    allow_forgot: bool,
) -> PromptOutcome {
    prompt_window(
        PwdParams {
            caption: "HelloGate - 主密码",
            title: title.to_string(),
            subtitle: subtitle.to_string(),
            masked: true,
            allow_forgot,
            ok_text: "解锁",
        },
        verify,
    )
}

/// 弹出「Windows 账户密码」输入框（忘记密码流程的第二步）。
///
/// 与普通密码框共用同一套窗口代码，区别只在于文案：标题说清楚这是
/// **电脑的锁屏密码**，而不是 HelloGate 里自己设的密码——这两个概念很像，
/// 用户非常容易搞混，必须用文字明确区分。
pub fn prompt_account_password(verify: impl Fn(&str) -> bool + 'static) -> PromptOutcome {
    prompt_window(
        PwdParams {
            caption: "HelloGate - Windows 身份验证",
            title: "忘记密码 - 验证 Windows 账户".to_string(),
            subtitle: "请输入本机 Windows 账户的登录密码（锁屏时用的那个）".to_string(),
            masked: true,
            allow_forgot: false,
            ok_text: "验证",
        },
        verify,
    )
}

/// 弹窗的通用实现：配置参数 → 建窗 → 模态消息循环 → 取回结果。
fn prompt_window(params: PwdParams, verify: impl Fn(&str) -> bool + 'static) -> PromptOutcome {
    // 清掉上一次的状态，放入本次文案与校验函数
    PWD_RESULT.with(|r| *r.borrow_mut() = None);
    PWD_FAILED.with(|f| f.set(false));
    FAIL_STREAK.with(|f| f.set(0));
    PWD_PARAMS.with(|p| *p.borrow_mut() = Some(params));
    PWD_VERIFY.with(|v| *v.borrow_mut() = Some(Box::new(verify)));

    unsafe {
        // 1. 注册窗口类（重复注册返回错误，忽略即可——类已存在且配置相同）。
        let class_name = wide(PWD_CLASS);
        let Ok(hinstance) = GetModuleHandleW(None) else {
            return PromptOutcome::Cancelled;
        };
        let bg_brush = CreateSolidBrush(COLORREF(Palette::BG));
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(pwd_wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hCursor: LoadCursorW(None, windows::Win32::UI::WindowsAndMessaging::IDC_ARROW)
                .unwrap_or_default(),
            hbrBackground: bg_brush.into(),
            // 品牌图标（exe 内嵌 ID=1 资源，与顶栏手绘 Logo 同构）。
            // WNDCLASSW 无 hIconSm，小图标自动回退 hIcon。
            hIcon: crate::ui::load_app_icon(),
            ..Default::default()
        };
        // RegisterClassW 返回 ATOM（u16 整数编号）：0 = 失败。
        // 失败通常只是"类已存在"（第二次及以后弹窗必现），对我们是正常情况。
        //
        // ★ 但失败时这次新建的画刷**没被任何窗口类引用**，必须主动还回去：
        //   否则每弹一次密码框就泄漏一个 GDI 句柄（反复上锁/解锁会持续累积，
        //   GDI 对象是进程级稀缺资源，攒到上万个就可能出现绘制异常）。
        //   第一次注册成功时画刷归窗口类所有，随进程退出由系统回收，不删。
        if RegisterClassW(&wc) == 0 {
            let _ = DeleteObject(bg_brush.into());
        }

        // 2. 创建窗口。
        //    - WS_EX_TOPMOST：遮罩本身是置顶窗口，密码框必须同样置顶才能盖在它上面，
        //      让用户看得见、点得到。
        //    - WS_POPUP：无边框弹出窗，样式与主控台一致（自绘标题文字代替标题栏）。
        //    - 尺寸按 96 DPI 逻辑值 480x320：
        //      高度比旧版（230）多出 90px，是为了容纳"品牌顶栏 + 白卡片 + 独立按钮行"
        //      这三层结构。旧版把标题、说明、输入框、按钮全挤在 230px 里，
        //      副标题一长就被裁掉（截图里那句提示被切掉半行，就是这么来的）。
        let caption = PWD_PARAMS.with(|p| {
            p.borrow().as_ref().map(|pp| pp.caption).unwrap_or("HelloGate")
        });
        let title_w = wide(caption);
        // CreateWindowExW 返回 Result<HWND>（句柄无效时自动取 GetLastError 成错误），
        // 所以 let-else 必须用 Ok 模式解构。
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title_w.as_ptr()),
            WS_POPUP | WS_VISIBLE | WS_CLIPCHILDREN,
            0,
            0,
            480,
            320,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) else {
            return PromptOutcome::Cancelled;
        };

        // Win11 圆角窗口：和主控台保持一致（Win10 上此调用失败会静默忽略）。
        round_window(hwnd);

        // 3. 按 DPI 定尺寸并居中到屏幕。
        //    （放在创建后做：此时窗口已存在，GetDpiForWindow 能拿到它所在屏幕的 DPI。）
        let dpi = GetDpiForWindow(hwnd).max(96) as f32;
        let s = dpi / 96.0;
        // 把缩放基准记进 thread_local：WM_PAINT 与 WM_DRAWITEM 里要用同一套比例，
        // 否则文字和控件的圆角会各算各的，出现"按钮比文字大一圈"的错位。
        PWD_SCALE.with(|c| c.set((dpi.round() as i32).max(48)));
        let w = (480.0 * s).round() as i32;
        let h = (320.0 * s).round() as i32;
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        // 负偏移钳到 0：高缩放比（175%/200%）的小屏上窗口可能比屏幕还大，
        // 居中算出来的坐标会是负数，窗口左上角就被推到屏幕外了。
        let _ = SetWindowPos(
            hwnd,
            None,
            ((sw - w) / 2).max(0),
            ((sh - h) / 2).max(0),
            w,
            h,
            SWP_SHOWWINDOW,
        );

        // 4. 把窗口推到前台并聚焦输入框。
        //    遮罩是 WS_EX_NOACTIVATE（从不抢焦点），所以这里能顺利拿到焦点。
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        if let Ok(edit) = GetDlgItem(Some(hwnd), IDC_PWD_EDIT) {
            let _ = SetFocus(Some(edit));
        }

        // 5. 模态消息循环（IsDialogMessageW 提供 Tab 切换 / Esc 取消等对话框行为）。
        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if !r.as_bool() {
                break; // WM_QUIT（窗口销毁时 PostQuitMessage 发出）
            }
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    // 6. 取回结果并释放校验函数（它可能捕获了调用方的数据，别留在 TLS 里）
    PWD_VERIFY.with(|v| *v.borrow_mut() = None);
    match PWD_RESULT.with(|r| r.borrow_mut().take()) {
        Some(PwdResult::Ok) => PromptOutcome::Unlocked,
        Some(PwdResult::Forgot) => PromptOutcome::Forgot,
        // Some(Cancel)=用户取消；None=窗口没建起来等异常，同样按取消处理
        _ => PromptOutcome::Cancelled,
    }
}

/// 密码框的窗口过程。
unsafe extern "system" fn pwd_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CREATE => {
            build_pwd_controls(hwnd);
            LRESULT(0)
        }
        WM_PAINT => {
            // 背景、白色顶栏、白卡片、输入框外框、右上角 ✕ 全部在这里手绘。
            // 窗口带 WS_CLIPCHILDREN：子控件所在的矩形不会被这段绘制覆盖，
            // 所以标题文字和输入框不会和自己的背景打架。
            draw_dialog_background(hwnd);
            LRESULT(0)
        }
        WM_DRAWITEM => {
            // 带 BS_OWNERDRAW 的按钮不再由系统绘制，改由我们完全负责外观。
            // lparam 就是系统打包好的 DRAWITEMSTRUCT（见 draw_dialog_item）。
            draw_dialog_item(lparam);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            // 顶栏空白处拖动窗口；右上角 ✕ 关闭（等同取消）。
            let mut rect = RECT::default();
            let _ = GetClientRect(hwnd, &mut rect);
            let s = PWD_SCALE.with(|c| c.get());
            let px = |v: i32| (v as f32 * s as f32 / 96.0).round() as i32;
            // lParam 里打包了鼠标坐标：低 16 位是 x，高 16 位是 y。
            // 先截成 16 位再按有符号解释，负坐标（多屏左侧）才不会算错。
            let x = (lparam.0 as i32 & 0xFFFF) as i16 as i32;
            let y = ((lparam.0 as i32 >> 16) & 0xFFFF) as i16 as i32;
            if y < px(56) {
                if x >= rect.right - px(52) {
                    cancel_pwd(hwnd);
                } else {
                    // 无边框窗口没有系统标题栏可拖，标准做法是：
                    // 先释放鼠标捕获，再伪造一次"点在标题栏上"的消息，让系统接管拖动。
                    // 比自己记录按下偏移量再算移动更稳——多屏、不同 DPI 下都不会抖。
                    let _ = ReleaseCapture();
                    let _ = SendMessageW(
                        hwnd,
                        WM_NCLBUTTONDOWN,
                        Some(WPARAM(HTCAPTION as usize)),
                        Some(LPARAM(0)),
                    );
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            // WM_COMMAND 的 wParam：低 16 位是控件 ID，高 16 位是通知码（按钮点击=0）
            let id = (wparam.0 & 0xFFFF) as i32;
            let code = (wparam.0 >> 16) as u32;
            // 输入框获得/失去焦点 → 重绘外框。
            // 聚焦时外框变品牌色，是现代表单最直观的"光标在这儿"提示。
            if id == IDC_PWD_EDIT && (code == EN_SETFOCUS || code == EN_KILLFOCUS) {
                PWD_FOCUSED.with(|f| f.set(code == EN_SETFOCUS));
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }
            // IDOK / IDCANCEL 是 MESSAGEBOX_RESULT 类型（系统对话框返回值常量），
            // 取 `.0` 得到底层 i32 后才能和上面算出的 id 比较。
            if id == IDOK.0 {
                try_unlock(hwnd);
                LRESULT(0)
            } else if id == IDCANCEL.0 {
                cancel_pwd(hwnd);
                LRESULT(0)
            } else if id == IDC_PWD_FORGET {
                // 用户点「忘记密码？」：记为 Forgot 并关窗。
                // 上层（password_unlock_flow）收到后会切换到恢复码验证框。
                PWD_RESULT.with(|r| *r.borrow_mut() = Some(PwdResult::Forgot));
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            } else if id == IDC_PWD_EYE {
                // 「显示 / 隐藏」：只切换遮罩字符，不提交、不关窗。
                toggle_pwd_visibility(hwnd);
                LRESULT(0)
            } else {
                LRESULT(0)
            }
        }
        WM_CLOSE => {
            // 用户用 Alt+F4 等方式关窗 = 取消
            cancel_pwd(hwnd);
            LRESULT(0)
        }
        WM_TIMER => {
            // 防暴力锁定到期：解锁输入框和按钮，允许下一次尝试。
            // （EnableWindow 在 windows 0.62 里直接收 HWND 并返回 BOOL，
            //   不是 Result，也没有 Option 包装。）
            if wparam.0 == PWD_TIMER_ID {
                let _ = KillTimer(Some(hwnd), PWD_TIMER_ID);
                let edit = GetDlgItem(Some(hwnd), IDC_PWD_EDIT).unwrap_or_default();
                let _ = EnableWindow(edit, true);
                if let Ok(ok) = GetDlgItem(Some(hwnd), IDOK.0) {
                    let _ = EnableWindow(ok, true);
                }
                if let Ok(cancel) = GetDlgItem(Some(hwnd), IDCANCEL.0) {
                    let _ = EnableWindow(cancel, true);
                }
                if let Ok(forget) = GetDlgItem(Some(hwnd), IDC_PWD_FORGET) {
                    if !forget.0.is_null() {
                        let _ = EnableWindow(forget, true);
                    }
                }
                let err = GetDlgItem(Some(hwnd), IDC_PWD_ERR).unwrap_or_default();
                let msg = wide("锁定已解除，请重试");
                let _ = SetWindowTextW(err, PCWSTR(msg.as_ptr()));
                // 锁定解除后输入框要回到正常白底——否则会一直停留在"密码错"的淡红状态，
                // 用户会误以为还没解除。
                PWD_FAILED.with(|f| f.set(false));
                let _ = InvalidateRect(Some(hwnd), None, false);
                let _ = SetFocus(Some(edit));
            } else if wparam.0 == SHAKE_TIMER_ID {
                // 【方案 A3 · 密码错误抖动】推进一步：
                // 偏移序列 [6,-6,6,-6,0]，每步从"基准 X + 偏移"定位，
                // 最后一步偏移为 0 —— 窗口必然精确回到原位（不做相对累加，
                // 避免误差累积导致抖完窗口歪掉）。
                let step = SHAKE_STEP.with(|s| s.get());
                if step < 0 {
                    // 状态已被复位（例如窗口正在关闭）：停表兜底。
                    let _ = KillTimer(Some(hwnd), SHAKE_TIMER_ID);
                } else {
                    let base = SHAKE_BASE_X.with(|x| x.get());
                    let off = SHAKE_OFFSETS[step as usize];
                    // SWP_NOSIZE / SWP_NOZORDER / SWP_NOACTIVATE：
                    // 只挪 X，不动大小、不改层级、不抢焦点——纯视觉位移。
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        base + off,
                        0,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                    if (step as usize) + 1 >= SHAKE_OFFSETS.len() {
                        // 已走到归零步：复位状态并停表。
                        SHAKE_STEP.with(|s| s.set(-1));
                        let _ = KillTimer(Some(hwnd), SHAKE_TIMER_ID);
                    } else {
                        SHAKE_STEP.with(|s| s.set(step + 1));
                    }
                }
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            // Static 控件默认会用"窗口背景色"擦自己的底。放在白色卡片里就会
            // 露出几块灰底，看着像贴了条胶带。这里统一返回**卡片白**画刷，
            // 文字才真正"融进"卡片。
            let hdc = HDC(wparam.0 as *mut _);
            let _ = SetBkMode(hdc, TRANSPARENT);
            // 按控件身份分配文字颜色：错误行=红色，副标题=次要灰，标题=正文色。
            let target = HWND(lparam.0 as *mut _);
            let err = GetDlgItem(Some(hwnd), IDC_PWD_ERR).unwrap_or_default();
            let sub = GetDlgItem(Some(hwnd), IDC_PWD_SUB).unwrap_or_default();
            let color = if target == err {
                Palette::DANGER
            } else if target == sub {
                Palette::TEXT_SUB
            } else {
                Palette::TEXT
            };
            let _ = SetTextColor(hdc, COLORREF(color));
            LRESULT(solid_brush(Palette::CARD).0 as isize)
        }
        WM_CTLCOLOREDIT => {
            // 输入框底色：正常白；密码错时淡红——用户一眼就知道该重输，
            // 不用再读一遍说明文字。
            // 返回的画刷会被系统用来擦输入框背景，所以颜色必须和 SetBkColor 一致，
            // 否则文字背后会残留一条别的颜色。
            let hdc = HDC(wparam.0 as *mut _);
            if PWD_FAILED.with(|f| f.get()) {
                let _ = SetTextColor(hdc, COLORREF(Palette::DANGER));
                let _ = SetBkColor(hdc, COLORREF(Palette::DANGER_BG));
                LRESULT(solid_brush(Palette::DANGER_BG).0 as isize)
            } else {
                let _ = SetTextColor(hdc, COLORREF(Palette::TEXT));
                let _ = SetBkColor(hdc, COLORREF(Palette::CARD));
                LRESULT(solid_brush(Palette::CARD).0 as isize)
            }
        }
        WM_DESTROY => {
            // 释放本弹窗用到的字体（GDI 对象必须手动删，否则反复弹窗会泄漏）。
            PWD_FONTS.with(|f| {
                for raw in f.borrow_mut().drain(..) {
                    let _ = DeleteObject(windows::Win32::Graphics::Gdi::HFONT(raw as *mut _).into());
                }
            });
            // 清掉窗口级状态，避免下一次弹窗沿用上一次的焦点/字体句柄。
            PWD_FOCUSED.with(|f| f.set(false));
            PWD_UI_FONT.with(|f| f.set(0));
            PWD_SHOWN.with(|f| f.set(false));
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 手绘整窗背景：页面底色 → 白色顶栏 → 白色柔和投影卡片 → 输入框外框 → 关闭 ✕。
///
/// # 视觉目标（与主控台统一的 Stripe / Linear 极简风）
/// - 大面积留白，品牌色只出现在"小标识"和"主按钮"上，不再整块铺顶栏；
/// - 内容放在带柔和投影的白色圆角卡片上，靠层次而不是重边框分区；
/// - 分割线、输入框描边都做到 1px，尽量轻。
///
/// # 为什么这些不交给控件
/// Static 控件只能画直角矩形，也画不出 1px 细描边和圆角。所以"容器级"的视觉
/// （底色、卡片、边框）必须自己用 GDI 画，控件只负责里面那点文字和输入。
unsafe fn draw_dialog_background(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let mut rect = RECT::default();
    let _ = GetClientRect(hwnd, &mut rect);

    let s = PWD_SCALE.with(|c| c.get());
    let px = |v: f32| (v * s as f32 / 96.0).round() as i32;

    // ---- 1. 页面底色 ----
    // 纯白卡片要靠比它深一档的底色"浮"起来，这一步不能省。
    fill_solid(hdc, rect, Palette::BG);

    // ---- 2. 白色顶栏（不再是整块品牌色）----
    // 旧版把顶栏铺满靛蓝，像十年前的 Web 后台；现代极简界面只让品牌色
    // 出现在"可点击的东西"和小图标上，标题区保持白底，靠一条极浅的分割线收边。
    let header = RECT {
        left: 0,
        top: 0,
        right: rect.right,
        bottom: px(56.0),
    };
    fill_solid(hdc, header, Palette::CARD);
    // 顶栏底部 1px 分割线：用最浅的 BORDER_SOFT。它只负责"分开"，
    // 不能抢标题的注意力（换成 BORDER 会像表格线，太重）。
    fill_solid(
        hdc,
        RECT {
            left: 0,
            top: px(56.0),
            right: rect.right,
            bottom: px(57.0),
        },
        Palette::BORDER_SOFT,
    );

    // 左上角 32x32 手绘品牌 Logo：渐变圆角底板 + 白盾 + H 镂空。
    // draw_logo（ui.rs）与 make_icon.ps1 生成的 .ico 同构——
    // 界面 Logo 和 exe 图标永远一致；品牌标识只占这一小块，
    // 既是视觉锚点，又不会把半屏染成靛蓝。
    let logo = RECT {
        left: px(20.0),
        top: px(12.0),
        right: px(52.0),
        bottom: px(44.0),
    };
    draw_logo(hdc, logo);

    // 产品名：20px 半粗、正文深色（TEXT）。文字起点在 Logo 右侧 12px，
    // 形成"图标 + 名称"的标准组合。
    let title_font = make_font(-px(20.0), FW_SEMIBOLD.0 as i32);
    let old = SelectObject(hdc, title_font.into());
    draw_text_left(
        hdc,
        "HelloGate",
        RECT {
            left: px(64.0),
            right: rect.right,
            top: px(7.0),
            bottom: px(30.0),
        },
        Palette::TEXT,
    );
    SelectObject(hdc, old);
    let _ = DeleteObject(title_font.into());

    // 定位语：12px 次要灰（TEXT_SUB），一行带过，不参与视觉层级竞争。
    let sub_font = make_font(-px(12.0), FW_NORMAL.0 as i32);
    let old = SelectObject(hdc, sub_font.into());
    draw_text_left(
        hdc,
        "应用锁 · 打开前验证",
        RECT {
            left: px(65.0),
            right: rect.right,
            top: px(29.0),
            bottom: px(48.0),
        },
        Palette::TEXT_SUB,
    );
    SelectObject(hdc, old);
    let _ = DeleteObject(sub_font.into());

    // 右上角关闭 ✕：必须换成 TEXT_SUB。
    // 旧的白色是为"整块品牌色顶栏"服务的；现在顶栏是白底，白线会完全看不见。
    draw_close_glyph(
        hdc,
        rect.right - px(26.0),
        px(28.0),
        px(7.0),
        px(2.0),
        Palette::TEXT_SUB,
    );

    // ---- 3. 白色圆角卡片（柔和投影）----
    // draw_card_soft 会先在卡片外圈铺页面底色，再叠两层极淡的灰，最后画白色本体，
    // 视觉上像卡片浮在桌面上——这是"现代感"最关键的一笔。
    // 圆角参数传 24（RoundRect 收的是"圆角椭圆直径"，视觉半径约 12px），
    // 与主控台保持同一档收敛程度。
    draw_card_soft(
        hdc,
        RECT {
            left: px(16.0),
            top: px(72.0),
            right: px(464.0),
            bottom: px(248.0),
        },
        px(24.0),
    );

    // ---- 4. 输入框外框 ----
    //
    // 【组件化（B1）】三态统一交给 ui::draw_input_state，画法只此一处：
    // - 密码错 → Danger（淡红底 + 2px 红边，错误必须一眼看见）；
    // - 聚焦  → Focused（品牌色 2px，等价于网页的 focus ring）；
    // - 普通  → Normal（白底 + 1px 浅灰描边）。
    // 以前错误态是这里自己铺 DANGER_BG 再手描红边的独立分支，与普通/
    // 聚焦两态的代码并排维护；现在和主控台共用同一份实现，改一次两边同步。
    // 矩形仍必须与 build_pwd_controls 里传给 create_input 的外框完全一致。
    let focused = PWD_FOCUSED.with(|f| f.get());
    let failed = PWD_FAILED.with(|f| f.get());
    let input_rc = RECT {
        left: px(36.0),
        top: px(168.0),
        right: px(444.0),
        bottom: px(206.0),
    };
    let input_state = if failed {
        InputState::Danger
    } else if focused {
        InputState::Focused
    } else {
        InputState::Normal
    };
    draw_input_state(hdc, input_rc, input_state);

    let _ = EndPaint(hwnd, &ps);
}

/// 手绘所有 `BS_OWNERDRAW` 按钮。
///
/// # 参数
/// `lparam` 是系统发来的 `DRAWITEMSTRUCT` 指针，里面打包了绘制所需的全部信息：
/// - `CtlID`：这是哪个控件（我们按 ID 决定画成主按钮还是次按钮）；
/// - `rcItem`：控件矩形（物理像素，不用再换算）；
/// - `itemState`：状态位，`ODS_SELECTED` 为真表示"正被按下"；
/// - `hwndItem`：控件句柄，用它读控件自身的文字，不用另外维护一份文案。
unsafe fn draw_dialog_item(lparam: LPARAM) {
    if lparam.0 == 0 {
        return;
    }
    let dis = &*(lparam.0 as *const DRAWITEMSTRUCT);
    let hdc = dis.hDC;
    let rc = dis.rcItem;
    let pressed = (dis.itemState.0 & ODS_SELECTED.0) != 0;
    let s = PWD_SCALE.with(|c| c.get());
    let px = |v: f32| (v * s as f32 / 96.0).round() as i32;

    // 把我们创建的正文字体选进 DC。不做这一步，按钮会退回系统默认宋体，
    // 和旁边 Static 的字号/字形明显不一致——一眼就能看出"拼凑感"。
    let font_raw = PWD_UI_FONT.with(|f| f.get());
    if font_raw != 0 {
        let _ = SelectObject(hdc, HFONT(font_raw as *mut _).into());
    }

    let text = window_text(dis.hwndItem);
    // 【圆角统一取"胶囊"】RoundRect 的圆角参数是"圆角椭圆的直径"，
    // 传按钮高度本身，得到视觉半径 = 高度的一半，两端就是完整的半圆。
    // 这样 38px 高的主/次按钮和 30px 高的「显示」小按钮都是标准胶囊形。
    let capsule = rc.bottom - rc.top;
    match dis.CtlID as i32 {
        // ---------- 主操作：解锁 / 验证恢复码（品牌色实心胶囊）----------
        id if id == IDOK.0 => {
            let (bg, fg) = if pressed {
                (Palette::BRAND_DARK, Palette::ON_BRAND)
            } else {
                (Palette::BRAND, Palette::ON_BRAND)
            };
            draw_button(hdc, rc, Palette::BG, bg, fg, &text, None, capsule);
        }
        // ---------- 次操作：取消（白底 + 1px BORDER_STRONG 胶囊）----------
        // 白色底 + 实一点的描边，和主按钮拉开层级：主操作"实心"，次操作"描边"。
        id if id == IDCANCEL.0 => {
            let bg = if pressed { Palette::HOVER } else { Palette::CARD };
            draw_button(
                hdc,
                rc,
                Palette::BG,
                bg,
                Palette::TEXT,
                &text,
                Some(Palette::BORDER_STRONG),
                capsule,
            );
        }
        // ---------- 旁路出口：忘记密码？（纯文字，无底色无边框）----------
        // 视觉上只是一个小链接，颜色用 TEXT_SUB（次要灰），不参与按钮层级。
        IDC_PWD_FORGET => {
            fill_solid(hdc, rc, Palette::BG);
            let color = if pressed {
                Palette::BRAND
            } else {
                Palette::TEXT_SUB
            };
            draw_text_left(
                hdc,
                &text,
                RECT {
                    left: rc.left + px(6.0),
                    right: rc.right,
                    top: rc.top,
                    bottom: rc.bottom,
                },
                color,
            );
        }
        // ---------- 输入框右端的「显示 / 隐藏」：44x30 小胶囊 ----------
        // 平时白底 + BORDER_SOFT 细描边 + TEXT_SUB 文字，像输入框里的一枚小芯片；
        // 按下换成 BRAND_SOFT 浅紫底 + BRAND 文字，给一个明确的点击反馈。
        // 它压在白色卡片/输入框上，所以 under 用 CARD，圆角处不会露出底色。
        IDC_PWD_EYE => {
            let (bg, fg) = if pressed {
                (Palette::BRAND_SOFT, Palette::BRAND)
            } else {
                (Palette::CARD, Palette::TEXT_SUB)
            };
            draw_button(
                hdc,
                rc,
                Palette::CARD,
                bg,
                fg,
                &text,
                Some(Palette::BORDER_SOFT),
                capsule,
            );
        }
        _ => {}
    }
}

/// 创建密码框里的控件：标题、说明、密码输入框、确定 / 取消按钮。
///
/// 所有坐标按窗口实际 DPI 缩放（高分屏不糊不错位）。
unsafe fn build_pwd_controls(hwnd: HWND) {
    let hinstance = GetModuleHandleW(None).unwrap_or_default();
    // DPI 来源不改（见 ui::scale 文档与决策 D2）：本窗口已创建，继续用
    // GetDpiForWindow 逐窗取 DPI（密码弹窗可能落在另一块不同 DPI 的屏幕上），
    // 只把换算公式收敛到 ui::scale。闭包签名不变，调用方无需改动。
    let dpi = GetDpiForWindow(hwnd).max(96) as i32;
    let px = |logical: f32| crate::ui::scale(logical.round() as i32, dpi);

    // 文案与行为参数（prompt_window 放进 thread_local 的那份）。
    // 取不到（异常情况）时给一套默认值，保证窗口总能建出来。
    let params = PWD_PARAMS
        .with(|p| p.borrow_mut().take())
        .unwrap_or_else(|| PwdParams {
            caption: "HelloGate - 主密码",
            title: "请输入主密码".to_string(),
            subtitle: String::new(),
            masked: true,
            allow_forgot: false,
            ok_text: "解锁",
        });
    let (title, subtitle) = (params.title, params.subtitle);

    // 三档字号，对应三层信息：
    // - title_font：卡片内的大标题，20px 半粗；
    // - font：正文 / 按钮 / 输入框，15px；
    // - small_font：副标题与错误提示，13px。
    //
    // 【为什么字高传负数】CreateFontW 的 nHeight 传负数表示"字符 em 高"，
    // 传正数表示"字符单元格高"。传正数会得到一个明显偏小、上下留白过多的字，
    // 这是 Win32 的老规矩，几乎所有原生软件都写负数。
    let font = make_font(-px(15.0), FW_NORMAL.0 as i32);
    let title_font = make_font(-px(20.0), FW_SEMIBOLD.0 as i32);
    let small_font = make_font(-px(13.0), FW_NORMAL.0 as i32);
    // 记下句柄，窗口销毁时统一释放（见 WM_DESTROY）。
    PWD_FONTS.with(|f| {
        let mut v = f.borrow_mut();
        v.push(font.0 as isize);
        v.push(title_font.0 as isize);
        v.push(small_font.0 as isize);
    });
    // 自绘按钮里选这支字体，保证按钮文字和旁边的 Static 文字一模一样。
    PWD_UI_FONT.with(|f| f.set(font.0 as isize));

    let static_class = wide("Static");
    // 说明：Edit 控件的创建已收进 ui::create_input（组件化，方案 B1），
    // "Edit" 类名、EM_SETMARGINS 内边距都由它内部处理，这里不再需要。
    let btn_class = wide("Button");

    // ---------- 卡片内的大标题 ----------
    // 位置按新的卡片内边距算：卡片是 (16,72)-(464,248)，左右留 20px 内边距。
    let title_w = wide(&title);
    let lbl_title = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(static_class.as_ptr()),
        PCWSTR(title_w.as_ptr()),
        WS_CHILD | WS_VISIBLE,
        px(36.0),
        px(88.0),
        px(408.0),
        px(30.0),
        Some(hwnd),
        Some(HMENU(IDC_PWD_TITLE as *mut _)),
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();
    SendMessageW(lbl_title, WM_SETFONT, Some(WPARAM(title_font.0 as usize)), Some(LPARAM(1)));

    // ---------- 副标题 ----------
    // 【关键修复】旧版只给这块文字 24px 高，说明一长就被裁掉——
    // 你截图里那句提示被切掉半行，就是高度不够造成的。
    // 这里给到 40px（13px 字能装两行），并把宽度限制在卡片内。
    if !subtitle.is_empty() {
        let sub_w = wide(&subtitle);
        let lbl_sub = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(static_class.as_ptr()),
            PCWSTR(sub_w.as_ptr()),
            WS_CHILD | WS_VISIBLE,
            px(36.0),
            px(120.0),
            px(408.0),
            px(40.0),
            Some(hwnd),
            Some(HMENU(IDC_PWD_SUB as *mut _)),
            Some(hinstance.into()),
            None,
        )
        .unwrap_or_default();
        SendMessageW(lbl_sub, WM_SETFONT, Some(WPARAM(small_font.0 as usize)), Some(LPARAM(1)));
    }

    // ---------- 密码输入框 ----------
    // 【组件化（方案 B1）】创建这一段（样式、垂直居中、内边距、右端让位）
    // 全部收进 ui::create_input：外框矩形是唯一事实来源，Edit 的位置由它
    // 推导出来；父窗口画框用的也是这同一个矩形（见 draw_dialog_background
    // 里的 draw_input_state），两边几何不会再各自硬编码出两套值。
    //
    // params.masked=true（密码框）：ES_PASSWORD 输入内容显示为圆点，旁人看不到；
    // params.masked=false（恢复码框）：明文显示，让用户能核对抄没抄对。
    //
    // 【右端预留】密码框右侧要放「显示 / 隐藏」小按钮（约 52px），
    // create_input 的最后一个参数就是让位宽度：0 = 占满到外框右侧内缩 2px 处。
    let input_frame = RECT {
        left: px(36.0),
        top: px(168.0),
        right: px(444.0),
        bottom: px(206.0),
    };
    let reserve_right = if params.masked { px(52.0) } else { 0 };
    let edit = create_input(
        hwnd,
        input_frame,
        GetDpiForWindow(hwnd).max(96) as i32,
        IDC_PWD_EDIT,
        params.masked,
        font,
        reserve_right,
    );

    // ---------- 「显示 / 隐藏」小按钮 ----------
    // 【为什么需要它】密码框输错了很难察觉：看到的永远是一排圆点，
    // 连"我到底输的是 123456 还是 123457"都分不清，只能整段删掉重打。
    // 给它加一个一键切明文的入口，核对完再切回圆点即可。
    //
    // 【为什么只在密码框上出现】恢复码框本身就是明文（params.masked=false），
    // 没有再切一次的必要，放个按钮反而多余。
    //
    // 【为什么叫文字按钮而不是小眼睛图标】这里全部用 GDI 手绘，画眼睛要拼
    // 两条贝塞尔弧 + 瞳孔，字号一缩放就容易糊；两个汉字在 44px 宽度里
    // 清清楚楚，也不会出现"字体缺字形变成豆腐块"的问题。
    let _btn_eye = if params.masked {
        let eye_text = wide("显示");
        let b = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(btn_class.as_ptr()),
            PCWSTR(eye_text.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
            px(392.0),
            px(172.0),
            px(44.0),
            px(30.0),
            Some(hwnd),
            Some(HMENU(IDC_PWD_EYE as *mut _)),
            Some(hinstance.into()),
            None,
        )
        .unwrap_or_default();
        SendMessageW(b, WM_SETFONT, Some(WPARAM(small_font.0 as usize)), Some(LPARAM(1)));
        Some(b)
    } else {
        None
    };
    // 每次弹窗都从"隐藏（圆点）"开始，避免上一次的预览状态被带进来。
    PWD_SHOWN.with(|c| c.set(false));

    // ★ 子类化输入框：普通窗口（非对话框模板）里按回车不会自动触发「确定」，
    //   这里拦截 Edit 收到的回车字符，转发一条 WM_COMMAND(IDOK) 给父窗口，
    //   让用户输完密码直接按回车就能提交。
    //   （函数指针要先转成 fn 指针类型再转 isize，直接 `函数 as isize` 会有警告。）
    let orig = SetWindowLongPtrW(
        edit,
        GWLP_WNDPROC,
        pwd_edit_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT as isize,
    );
    if orig != 0 {
        ORIG_EDIT_PROC.store(orig, Ordering::SeqCst);
    }

    // ---------- 错误提示（平时空着，密码错时这里出红字）----------
    let lbl_err = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(static_class.as_ptr()),
        PCWSTR::null(),
        WS_CHILD | WS_VISIBLE,
        px(36.0),
        px(214.0),
        px(408.0),
        px(24.0),
        Some(hwnd),
        Some(HMENU(IDC_PWD_ERR as *mut _)),
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();
    SendMessageW(lbl_err, WM_SETFONT, Some(WPARAM(small_font.0 as usize)), Some(LPARAM(1)));

    // ---------- 忘记密码？（纯文字按钮，放最左）----------
    //
    // 视觉上是"旁路出口"而不是主操作——正常用户不该点它，忘了密码的人才需要。
    // 所以不画底色、不画边框，只留一行文字，和右边的实心主按钮形成明确层级。
    let btn_forget = if params.allow_forgot {
        let forget_text = wide("忘记密码？");
        let b = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(btn_class.as_ptr()),
            PCWSTR(forget_text.as_ptr()),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
            px(36.0),
            px(264.0),
            px(116.0),
            px(38.0),
            Some(hwnd),
            Some(HMENU(IDC_PWD_FORGET as *mut _)),
            Some(hinstance.into()),
            None,
        )
        .unwrap_or_default();
        SendMessageW(b, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
        Some(b)
    } else {
        None
    };

    // ---------- 取消 / 解锁 ----------
    // 两个按钮都改成 BS_OWNERDRAW：外观完全由 draw_dialog_item 负责。
    // 确认按钮文字随场景变化：密码框叫「解锁」，恢复码框叫「验证恢复码」。
    let ok_text = wide(params.ok_text);
    let btn_ok = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(btn_class.as_ptr()),
        PCWSTR(ok_text.as_ptr()),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
        px(368.0),
        px(264.0),
        px(80.0),
        px(38.0),
        Some(hwnd),
        Some(HMENU(IDOK.0 as *mut _)),
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();
    let cancel_text = wide("取消");
    let btn_cancel = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(btn_class.as_ptr()),
        PCWSTR(cancel_text.as_ptr()),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_OWNERDRAW as u32),
        px(276.0),
        px(264.0),
        px(84.0),
        px(38.0),
        Some(hwnd),
        Some(HMENU(IDCANCEL.0 as *mut _)),
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default();
    for b in [Some(btn_ok), Some(btn_cancel), btn_forget].into_iter().flatten() {
        SendMessageW(b, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
    }
}

/// 被子类化后的 Edit 窗口过程：只处理回车，其余交回系统原过程。
unsafe extern "system" fn pwd_edit_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // WM_CHAR 的 wParam 是字符码。13 = 回车（用户按 Enter 时 Edit 会收到 '\r'）。
    if msg == WM_CHAR && (wparam.0 as u32) == 13 {
        // 用 PostMessage（异步）而不是 SendMessage：
        // 避免在 Edit 正处理消息时，父窗口同步销毁整个对话框导致重入混乱。
        if let Ok(parent) = GetParent(hwnd) {
            let _ = PostMessageW(Some(parent), WM_COMMAND, WPARAM(IDOK.0 as usize), LPARAM(0));
        }
        return LRESULT(0);
    }
    // 其它消息原样交给 Edit 的系统默认过程处理。
    let orig = ORIG_EDIT_PROC.load(Ordering::SeqCst);
    if orig == 0 {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    // 把存下来的函数地址还原成 WNDPROC 回调类型再转交。
    let f: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT =
        std::mem::transmute(orig);
    // WNDPROC 在 windows 0.62 里是类型别名（Option<回调函数指针>），不是元组结构体，
    // 所以直接传 Some(f) 即可。
    CallWindowProcW(Some(f), hwnd, msg, wparam, lparam)
}

/// 切换输入框的「圆点 / 明文」显示（对应输入框右端的「显示 / 隐藏」小按钮）。
///
/// # 原理
/// Edit 的 `ES_PASSWORD` 只是创建时的初始样式，运行时可以用
/// `EM_SETPASSWORDCHAR` 改"用哪个字符遮住原文"：
/// - 传 0 → 不遮，直接显示明文；
/// - 传 `*`（0x2A）→ 恢复系统密码框默认的星号遮罩。
///
/// 不重建控件，所以已经输入的内容和光标位置都不会丢。
unsafe fn toggle_pwd_visibility(hwnd: HWND) {
    // EM_SETPASSWORDCHAR 的消息号（winuser.h 常量，windows crate 未单独封装）。
    const EM_SETPASSWORDCHAR: u32 = 0x00CC;
    // 0x2A = '*'。实测 Windows 密码框默认就是用星号遮住原文，
    // 恢复时写回同一个字符，切换前后外观才完全一致（不会突然从 * 变成 ●）。
    const MASK: usize = 0x2A;

    let shown = !PWD_SHOWN.with(|c| c.get());
    PWD_SHOWN.with(|c| c.set(shown));

    let Ok(edit) = GetDlgItem(Some(hwnd), IDC_PWD_EDIT) else {
        return;
    };
    let mask = if shown { 0 } else { MASK };
    let _ = SendMessageW(
        edit,
        EM_SETPASSWORDCHAR,
        Some(WPARAM(mask)),
        Some(LPARAM(0)),
    );

    // 按钮文字跟着状态变：正在显示明文时写「隐藏」，否则写「显示」。
    if let Ok(btn) = GetDlgItem(Some(hwnd), IDC_PWD_EYE) {
        let text = wide(if shown { "隐藏" } else { "显示" });
        let _ = SetWindowTextW(btn, PCWSTR(text.as_ptr()));
        // 自绘按钮改了文字必须自己请求重绘，系统不会替我们刷新。
        let _ = InvalidateRect(Some(btn), None, true);
    }

    // 点按钮会把焦点抢过去；切完立刻还给输入框，用户可以接着打字。
    let _ = SetFocus(Some(edit));
    let _ = InvalidateRect(Some(edit), None, true);
}

/// 让密码框窗口横向抖几下（方案 A3：密码错误的即时反馈）。
///
/// 【为什么用计时器而不是 Sleep 循环】Sleep 期间消息不派发，
/// SetWindowPos 改了坐标窗口也不会重画，抖动根本看不见；
/// 用 WM_TIMER 每步借一次正常的消息循环（见 pwd_wnd_proc 的
/// SHAKE_TIMER_ID 分支），画面实时跟手，期间输入照常可用。
unsafe fn start_shake(hwnd: HWND) {
    // 已经在抖就别重复启动（用户连按回车会叠加出乱序的抖动）。
    if SHAKE_STEP.with(|s| s.get()) >= 0 {
        return;
    }
    let mut rc = RECT::default();
    if GetWindowRect(hwnd, &mut rc).is_err() {
        return; // 拿不到位置就不抖——绝不为了动画影响正常解锁流程
    }
    SHAKE_BASE_X.with(|x| x.set(rc.left));
    SHAKE_STEP.with(|s| s.set(0));
    let _ = SetTimer(Some(hwnd), SHAKE_TIMER_ID, SHAKE_INTERVAL_MS, None);
}

/// 用户点「解锁」/按回车：在框内就地校验密码。
///
/// - 对了 → 记下成功并关窗（调用方据此解锁应用）。
/// - 错了 → **不关窗**：窗口横向抖一下（方案 A3）、输入框转淡红底、
///   下方出红字、内容清空并重新聚焦，用户接着打字就是下一次尝试。
/// - 连错 3 次起 → 防暴力锁定：输入框和按钮整体禁用 2s → 4s → 8s …
///   （每次翻倍，上限 60 秒），到点由 WM_TIMER 自动恢复。
///
/// 【为什么要防暴力】用户可以选择很短的密码（如 3 位数字，总共才 1000 种组合）。
/// 没有锁定的话，一个人工点击大约每 2 秒就能试一次，半小时内就能穷举完。
/// 加上指数退避后，穷举成本被拉长到数小时以上，配合 PBKDF2 的单次 100ms
/// 派生耗时，暴力尝试在 practical 层面失去意义。
unsafe fn try_unlock(hwnd: HWND) {
    let edit = GetDlgItem(Some(hwnd), IDC_PWD_EDIT).unwrap_or_default();
    // 防暴力锁定期间：输入框被禁用，此时直接忽略提交（回车子类化仍可能触发）。
    if !IsWindowEnabled(edit).as_bool() {
        return;
    }
    let mut buf = [0u16; 256];
    let len = GetWindowTextW(edit, &mut buf) as usize;
    let text = String::from_utf16_lossy(&buf[..len]);

    // 调用方传进来的校验函数（PBKDF2 派生比对，见 password_unlock_flow）。
    // 拿不到函数（异常情况）时一律算失败，宁可让用户重试，绝不放行。
    let ok = PWD_VERIFY.with(|v| {
        v.borrow()
            .as_ref()
            .map(|f| f(&text))
            .unwrap_or(false)
    });

    if ok {
        // 成功后清零连败计数：下次弹窗从零开始（prompt_window 也会重置，双保险）。
        FAIL_STREAK.with(|f| f.set(0));
        PWD_RESULT.with(|r| *r.borrow_mut() = Some(PwdResult::Ok));
        let _ = DestroyWindow(hwnd);
        return;
    }

    // 校验失败：切到"红框重输"状态；窗口同时横向抖几下给即时反馈（方案 A3，
    // 比"红字"更先被注意到——人眼对运动比对颜色敏感）；连败次数够多就拉长锁定。
    let streak = FAIL_STREAK.with(|f| {
        let v = f.get() + 1;
        f.set(v);
        v
    });
    PWD_FAILED.with(|f| f.set(true));
    start_shake(hwnd);
    let err = GetDlgItem(Some(hwnd), IDC_PWD_ERR).unwrap_or_default();
    if streak >= 3 {
        // 指数退避：第 3 次错锁 2 秒，之后每次翻倍，封顶 60 秒。
        // （1 << (streak-2)：streak=3 → 2s，4 → 4s，5 → 8s …… min(60) 封顶。）
        let secs = (1u64 << (streak - 2).min(6)).min(60);
        // 禁用输入框与所有按钮，杜绝锁定期间继续提交。
        let _ = EnableWindow(edit, false);
        if let Ok(b) = GetDlgItem(Some(hwnd), IDOK.0) {
            let _ = EnableWindow(b, false);
        }
        if let Ok(b) = GetDlgItem(Some(hwnd), IDCANCEL.0) {
            let _ = EnableWindow(b, false);
        }
        if let Ok(b) = GetDlgItem(Some(hwnd), IDC_PWD_FORGET) {
            if !b.0.is_null() {
                let _ = EnableWindow(b, false);
            }
        }
        let msg = wide(&format!("尝试次数过多，已锁定 {secs} 秒后再试"));
        let _ = SetWindowTextW(err, PCWSTR(msg.as_ptr()));
        // 定时器到点发 WM_TIMER，由它解锁控件并清掉这条提示。
        let _ = SetTimer(Some(hwnd), PWD_TIMER_ID, (secs * 1000) as u32, None);
        logln!("[password] 连续错误 {streak} 次，防暴力锁定 {secs} 秒");
    } else {
        let msg = wide("密码不正确，请重试");
        let _ = SetWindowTextW(err, PCWSTR(msg.as_ptr()));
        logln!("[password] 密码错误，允许就地重试");
    }
    // 清空输入框并聚焦：用户直接打字即可，不用先手动删掉错的密码。
    // 改文本本身会让 Edit 重绘，重绘时就会走上面的 WM_CTLCOLOREDIT 拿到红底。
    // （锁定期间也清空——防止锁定解除后残留一次"白送的"错误输入。）
    let _ = SetWindowTextW(edit, PCWSTR::null());
    if IsWindowEnabled(edit).as_bool() {
        let _ = SetFocus(Some(edit));
    }
}

/// 用户点「取消」/Esc/关窗：记为取消并销毁窗口（销毁后消息循环随 WM_QUIT 退出）。
unsafe fn cancel_pwd(hwnd: HWND) {
    PWD_RESULT.with(|r| *r.borrow_mut() = Some(PwdResult::Cancel));
    let _ = DestroyWindow(hwnd);
}

/// 创建字体（微软雅黑 UI，与主控台统一）。
unsafe fn make_font(height: i32, weight: i32) -> HFONT {
    let face = wide("Microsoft YaHei UI");
    CreateFontW(
        height, 0, 0, 0, weight, 0, 0, 0,
        DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
        FF_DONTCARE.0.into(), PCWSTR(face.as_ptr()),
    )
}

/// 把 Rust 字符串转成 NUL 结尾的 UTF-16。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
