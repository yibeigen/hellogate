//! 认证模块（Phase 3 P0-3）
//!
//! 职责：调用 **Windows Hello** 原生验证接口，让用户用指纹 / 人脸 / 系统 PIN
//! 完成解锁。
//!
//! 对应 DESIGN_AND_ROADMAP.md §3.2。使用的 API：
//! `Windows.Security.Credentials.UI.UserConsentVerifier`
//!  - `CheckAvailabilityAsync()`  → 检查设备是否支持 Hello
//!  - `RequestVerificationAsync()` → 弹出系统原生验证对话框
//!
//! # 为什么用 WinRT 而不是自绘密码框？
//! 1. **零学习成本**：系统弹窗，用户天然会用。
//! 2. **零生物数据接触**：指纹/人脸由系统内部处理，我们只拿到「成功/失败」结果，
//!    不接触任何敏感数据，安全合规。
//! 3. **统一体验**：指纹 / 人脸 / PIN / 密码 全部由系统界面兜底。

use windows::core::{HSTRING, PCWSTR, Result as WinResult};
use windows::Security::Credentials::UI::{
    UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
};
// 忘记密码时的「Windows 账户密码」校验需要这几个 Win32 接口：
// LogonUserW 让系统自己去比对账户密码（我们不接触密码策略），
// 成功后系统会给一个令牌句柄，用完必须 CloseHandle 释放。
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    LogonUserW, LOGON32_LOGON_INTERACTIVE, LOGON32_PROVIDER_DEFAULT,
};
// 说明：`join()` 是 `windows_future::IAsyncOperation<T>` 的**固有方法**（inherent method），
// 直接调用即可，无需引入任何 trait。但类型本身来自 windows_future crate，
// 需要它作为依赖存在（见 Cargo.toml）。

use crate::logln;
use crate::password::{self, AuthSecrets};

/// 认证结果（对上层屏蔽 WinRT 的枚举细节）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthOutcome {
    /// 验证通过
    Verified,
    /// 用户主动取消
    Cancelled,
    /// 尝试次数耗尽（如指纹多次错误）
    RetriesExhausted,
    /// 设备忙（正在被其他程序使用）
    DeviceBusy,
    /// 设备不可用 / 未配置 Hello（此时应回退到主密码）
    Unavailable(String),
    /// 其他错误
    Error(String),
}

/// 检查当前设备是否可用 Windows Hello。
///
/// # 返回
/// - `Ok(true)`：可用，可直接调用 `verify()`。
/// - `Ok(false)`：不可用（未配置或硬件缺失），应回退到主密码。
/// - `Err(_)`：调用 API 本身出错。
pub fn is_available() -> WinResult<bool> {
    let availability = UserConsentVerifier::CheckAvailabilityAsync()?.join()?;
    Ok(matches!(
        availability,
        UserConsentVerifierAvailability::Available
    ))
}

/// 弹出 Windows Hello 原生验证对话框。
///
/// # 参数
/// - `reason`：展示给用户的验证理由（会显示在系统弹窗上）。
///
/// # 返回
/// 统一的 [`AuthOutcome`]，方便上层状态机处理。
pub fn verify(reason: &str) -> AuthOutcome {
    // 先检查可用性，避免在不支持的设备上白弹窗
    match is_available() {
        Ok(true) => {}
        Ok(false) => {
            return AuthOutcome::Unavailable(
                "设备未配置 Windows Hello（无指纹/人脸/PIN）".to_string(),
            )
        }
        Err(e) => return AuthOutcome::Error(format!("检查 Hello 可用性失败: {e}")),
    }

    // 调用系统验证（阻塞等待用户操作完成）。
    // `join()` 会等待异步操作结束并返回结果（内部即 GetResults）。
    let result: WinResult<UserConsentVerificationResult> =
        UserConsentVerifier::RequestVerificationAsync(&HSTRING::from(reason)).and_then(|op| op.join());

    match result {
        Ok(r) => match r {
            UserConsentVerificationResult::Verified => AuthOutcome::Verified,
            UserConsentVerificationResult::Canceled => AuthOutcome::Cancelled,
            UserConsentVerificationResult::RetriesExhausted => AuthOutcome::RetriesExhausted,
            UserConsentVerificationResult::DeviceBusy => AuthOutcome::DeviceBusy,
            UserConsentVerificationResult::DeviceNotPresent => {
                AuthOutcome::Unavailable("设备不支持 Windows Hello".to_string())
            }
            UserConsentVerificationResult::NotConfiguredForUser => {
                AuthOutcome::Unavailable("当前用户未配置 Windows Hello".to_string())
            }
            UserConsentVerificationResult::DisabledByPolicy => {
                AuthOutcome::Unavailable("Windows Hello 已被策略禁用".to_string())
            }
            _ => AuthOutcome::Error("未知的验证结果".to_string()),
        },
        Err(e) => AuthOutcome::Error(format!("调用 Hello 验证失败: {e}")),
    }
}

// ============================================================================
// 认证路由：按配置的认证方式选择「系统验证」或「自设密码」
// ============================================================================

/// 认证路由的结论（对调用方的语义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRoute {
    /// 验证通过，直接放行。
    Passed,
    /// 验证未通过（取消 / 密码错误 / 出错），调用方保持锁定状态。
    Failed,
    /// 无任何可用凭据（Hello 不可用 + 未设密码）。这是"首次使用"的合法状态：
    /// 主控台首次进入应放行，遮罩解锁则必须保持锁定（由调用方自行决断）。
    NoCredentials,
}

/// 按配置的认证方式执行一次完整验证，三个入口（遮罩解锁 / 设置自锁 / 守门进程）共用。
///
/// # 路由规则
/// - `method == "password"` 且密码已设置：直接弹**自设密码框**，
///   全程不碰 Windows Hello——用户选密码模式通常正是为了绕开 Hello；
/// - 其余情况（hello 模式，或 password 模式但密码残缺的异常配置）：先弹 **Hello**；
///   - Hello 通过 → 放行；
///   - Hello 不可用且已设密码 → 回退密码框；
///   - Hello 不可用且没设密码 → `NoCredentials`；
///   - 用户取消 / 次数耗尽 / 出错 → `Failed`（宁可多拦，不可漏放）。
///
/// # 忘记密码怎么办
/// 密码框左下角有「忘记密码？」按钮，点它后改用 **Windows 身份**验证
/// （Hello 人脸/指纹/PIN，或 Windows 账户登录密码），详见 [`verify_windows_identity`]。
///
/// # 为什么放这里
/// 路由逻辑在常驻进程（main.rs 两处）和守门进程（gater.rs）都要用，
/// 放在 auth.rs 天然可用（auth 不依赖 RUNTIME，凭据由调用方以快照传入）。
pub fn route_verification(method: &str, display: &str, secrets: &AuthSecrets) -> AuthRoute {
    // ---- 分支一：用户明确选择「自设密码」模式 ----
    if method == "password" && secrets.has_password() {
        logln!("[auth] 认证方式=自设密码，弹密码框 ...");
        return match password::password_unlock_flow(display, secrets) {
            password::UnlockFlow::Passed => AuthRoute::Passed,
            password::UnlockFlow::Failed => AuthRoute::Failed,
        };
    }
    if method == "password" {
        // 配置异常（选了密码模式却没设成密码）：不能据此放行，降级走 Hello。
        logln!("[auth] 配置为密码模式但未设置密码（异常），降级走 Hello");
    }

    // ---- 分支二：系统验证（Windows Hello）优先 ----
    logln!("[auth] 认证方式=系统验证（Hello），正在唤起 ...");
    match verify_with_timeout(&format!("解锁「{display}」"), HELLO_TIMEOUT_SECS) {
        AuthOutcome::Verified => AuthRoute::Passed,
        AuthOutcome::Unavailable(msg) => {
            // 注意：超时也会走到这里（见 verify_with_timeout 的说明）——
            // "Hello 弹不出来"在提权进程里出现过（join 永久阻塞），
            // 绝不能让它变成一条死路：超时后必须能回退密码框。
            logln!("[auth] Windows Hello 不可用：{msg}，回退主密码 ...");
            if !secrets.has_password() {
                // 没 Hello 也没密码：无从验证，把决断权交回调用方
                // （主控台首次使用放行；遮罩解锁保持锁定）。
                return AuthRoute::NoCredentials;
            }
            match password::password_unlock_flow(display, secrets) {
                password::UnlockFlow::Passed => AuthRoute::Passed,
                password::UnlockFlow::Failed => AuthRoute::Failed,
            }
        }
        other => {
            // 取消 / 次数耗尽 / 出错：一律不放行。
            logln!("[auth] Hello 未通过（{other:?}）");
            AuthRoute::Failed
        }
    }
}

// ============================================================================
// 忘记密码 → Windows 身份验证（人脸/指纹/PIN 或 Windows 账户密码）
// ============================================================================

/// 「忘记自己设的 HelloGate 密码」时的验证手段：证明操作者就是这台电脑的主人。
///
/// # 为什么用 Windows 身份
/// 用户的锁屏密码/人脸/指纹本来就已经由 Windows 保护得好好的，我们没必要
/// 再发明一套"恢复码"（用户还得抄写、弄丢就彻底完了）。直接借用系统凭据：
/// 1. 先弹 **Windows Hello**（人脸 / 指纹 / PIN）——系统内部校验，我们不接触生物数据；
/// 2. Hello 不可用或用户取消 → 弹框要求输入 **Windows 账户登录密码**，
///    交给 `LogonUserW` 校验（等价于在锁屏界面输密码）。
///
/// # 返回
/// `true` = 验证通过（调用方可以放行）；`false` = 取消/失败（保持锁定）。
pub fn verify_windows_identity(reason: &str) -> bool {
    logln!("[auth] 忘记密码：改用 Windows 身份验证 ...");
    match verify_with_timeout(reason, HELLO_TIMEOUT_SECS) {
        AuthOutcome::Verified => {
            logln!("[auth] Windows 身份验证通过（Hello）");
            true
        }
        other => {
            logln!("[auth] Windows Hello 未通过（{other:?}），改用 Windows 账户密码 ...");
            match password::prompt_account_password(verify_account_password) {
                password::PromptOutcome::Unlocked => {
                    logln!("[auth] Windows 账户密码验证通过");
                    true
                }
                _ => {
                    logln!("[auth] Windows 账户密码验证被取消或失败");
                    false
                }
            }
        }
    }
}

/// 快速探测 Windows Hello 是否可用（最多等 3 秒）。
///
/// 【为什么不直接用 verify】`is_available()` 只是问系统"这台机器配没配 Hello"，
/// 不会弹窗，通常几十毫秒就返回；但万一系统接口卡住，我们不能让主控台跟着干等。
/// 3 秒拿不到答案就当作"不可用"，直接走 Windows 账户密码，用户不会觉得卡。
pub fn hello_available_quick() -> bool {
    let (tx, rx) = std::sync::mpsc::channel::<bool>();
    std::thread::spawn(move || {
        let _ = tx.send(is_available().unwrap_or(false));
    });
    rx.recv_timeout(std::time::Duration::from_secs(3))
        .unwrap_or(false)
}

/// 校验「当前 Windows 账户的登录密码」是否正确。
///
/// # 原理
/// 调用 Win32 的 `LogonUserW`，以「交互式登录」方式让系统自己去比对密码：
/// - 密码对 → 系统返回一个登录令牌（句柄），我们立刻关掉它，只取"成功"这个结论；
/// - 密码错 → 系统直接返回错误，我们**拿不到任何可用信息**。
///
/// 这样密码校验完全由 Windows 的认证子系统完成，我们的进程既不知道
/// 密码策略，也不会把明文密码留在任何地方（函数返回时内存里的明文即被释放）。
pub(crate) fn verify_account_password(pwd: &str) -> bool {
    // 当前登录账户的用户名与域名：
    // - 域环境：USERDOMAIN 是域名，系统按 DOMAIN\user 校验；
    // - 本地账户 / Microsoft 账户：USERDOMAIN 是计算机名，同样能校验。
    let user = std::env::var("USERNAME").unwrap_or_default();
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    if user.is_empty() {
        logln!("[auth] 无法读取当前 Windows 用户名，账户密码验证不可用");
        return false;
    }

    let user_w = to_wide(&user);
    let domain_w = to_wide(&domain);
    let pwd_w = to_wide(pwd);

    // 不传域（空串）时交给系统按「本机账户」查找，这是文档允许的用法。
    let domain_ptr = if domain.is_empty() {
        PCWSTR::null()
    } else {
        PCWSTR(domain_w.as_ptr())
    };

    // 成功时系统会返回一个令牌句柄；我们只关心成败，拿到后立即关闭避免句柄泄漏。
    let mut token = HANDLE::default();
    let result = unsafe {
        LogonUserW(
            PCWSTR(user_w.as_ptr()),
            domain_ptr,
            PCWSTR(pwd_w.as_ptr()),
            LOGON32_LOGON_INTERACTIVE,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )
    };
    if result.is_ok() {
        unsafe {
            let _ = CloseHandle(token);
        }
        true
    } else {
        false
    }
}

/// Rust 字符串 → NUL 结尾 UTF-16（Win32 的 W 系列 API 要这个形式）。
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Windows Hello 的等待上限（秒）。
///
/// 【为什么需要超时】`IAsyncOperation::join()` 是**阻塞且不可中断**的。
/// 在提权进程（管理员权限的常驻实例）里，Hello 对话框偶发完全不显示，
/// 而 join 就一直等下去——事故现场：主控台打不开、设置自锁永远卡住。
/// 把 Hello 放到子线程并只等待有限时间，超时就当"不可用"处理，
/// 自动落到密码框：用户至少还有一条路能进主控台。
///
/// 取值权衡：太短会误伤"慢慢掏出手机看提示"的正常用户；
/// 太长则卡住的场景白等。30 秒足够任何正常操作完成，也不至于让人以为死机。
const HELLO_TIMEOUT_SECS: u64 = 30;

/// 带超时的 Hello 验证：把可能永久阻塞的调用隔离到子线程。
///
/// # 返回
/// - 正常完成 → 原样返回 `verify` 的结果。
/// - 超时 → 返回 `Unavailable("...无响应（超时）")`，
///   让上层走"回退密码"分支，而不是把这个应用/主控台彻底锁死。
///
/// # 超时后的残留线程
/// `join()` 不可中断，那个子线程会继续等；进程内多一个休眠线程无害。
/// 它最终若返回（比如 Hello 窗口迟到了 5 分钟才出现），send 会失败
/// （接收方早已离开），结果被丢弃，不会出现"二次解锁"。
pub(crate) fn verify_with_timeout(message: &str, secs: u64) -> AuthOutcome {
    let (tx, rx) = std::sync::mpsc::channel::<AuthOutcome>();
    let prompt = message.to_string();
    // 子线程跑 Hello：这正是事故的根因位置，绝不能让它贴着调用方线程。
    std::thread::spawn(move || {
        // 接收方超时后退出了 → send 失败属正常情况，显式忽略。
        let _ = tx.send(verify(&prompt));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
        Ok(outcome) => outcome,
        Err(_) => {
            logln!("[auth] Hello 在 {secs} 秒内无响应（提权进程弹窗偶发失败），按不可用处理");
            AuthOutcome::Unavailable(format!("Windows Hello 无响应（{secs} 秒超时）"))
        }
    }
}
