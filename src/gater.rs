//! 守门模式（IFEO 启动拦截的进程侧）
//!
//! # 这个进程从哪来
//! 受保护程序（如 WINWORD.EXE）被双击时，系统的 IFEO 机制（见 ifeo.rs）
//! 实际启动的是：
//!   `hellogate.exe --gate "C:\...\WINWORD.EXE" <原始参数...>`
//!
//! # 它做什么（一次性，跑完就退出）
//! 1. 验证：Windows Hello → 不可用则主密码（与遮罩解锁完全同一套逻辑）。
//! 2. 通过 → 向**提权常驻实例**发一条「代启动请求」（WM_COPYDATA），
//!    等它把真程序启动起来（事件通知），自己退出。
//! 3. 不通过 / 用户取消 → 直接退出。目标程序**从头到尾没有运行过**，
//!    这就是用户要的"先验证才能打开"。
//!
//! # 为什么守门进程不能自己启动目标程序（与旧版的关键区别）
//! IFEO 登记在 HKLM，只有提权进程能"删登记→启动→恢复"来避开 Debugger
//! 重定向死循环。守门进程是普通权限（UAC 提权后的子进程默认不继承高权限），
//! 既删不了 HKLM 值，直接 spawn 又会被再次拦截 → 无限自我重启链。
//! 所以改成请求制：验证在守门进程做（要弹 Hello/密码框，必须普通权限），
//! 启动动作交给常驻进程做（它有管理员权限）。
//!
//! # 常驻进程不在怎么办
//! 找不到常驻窗口 / 请求超时 → 弹提示框后以失败退出（fail-closed，不放行）。
//! 这与"宁可多锁、不可漏放"的整体策略一致。

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HWND, LRESULT, WAIT_OBJECT_0, WPARAM, LPARAM};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, FindWindowW, RegisterClassW, SendMessageTimeoutW,
    SMTO_ABORTIFHUNG, WM_COPYDATA, WNDCLASSW, WS_OVERLAPPED,
};

use crate::{auth, config, logln, password, process};

/// 守门进程自己隐藏窗口的类名（WM_COPYDATA 的"发送者身份"，
/// 常驻实例据此校验请求确实来自 hellogate.exe，防任意程序伪造代启动）。
const GATE_WINDOW_CLASS: &str = "HelloGateGateWindow";

/// 代启动请求的 dwData 标识（区分于其它可能的 WM_COPYDATA 用途；
/// 常驻实例 main.rs 的 handle_relay 据此过滤，所以必须 pub）。
pub const RELAY_DW_DATA: usize = 1;

/// 请求模式：只清理失效的 IFEO 登记，不代启动（见 release_stale）。
/// 与 main.rs 的 RelayReq.mode 字段配对，拼错会静默失效，所以用常量而不是字面量。
pub const MODE_CLEANUP: &str = "cleanup";

/// 守门入口。`gate_args` = argv[1..]，即 ["--gate", "<目标程序路径>", 原始参数...]。
/// 永不返回（内部以 process::exit 结束）。
pub fn run(gate_args: &[String]) -> ! {
    // IFEO 会把原始命令行追加在 "--gate" 后面，第一项就是被拦截的程序路径。
    let Some(target) = gate_args.get(1) else {
        logln!("[gate] 命令行缺少目标程序路径，忽略");
        std::process::exit(1);
    };
    let rest: &[String] = &gate_args[2..];
    let name = process::file_name_lower(target);

    let cfg = config::load();

    // 是否已完成用户验证（随请求发给常驻实例做最终把关，防配置竞态绕过）。
    // 【为什么不在声明时给默认值】名单外分支现在直接 fail_box 退出（永不返回），
    // 走到请求发出这一步的只可能是"已验证"路径——延迟赋值可避免
    // "初始值从未被读取"的告警，也让代码意图更明确。
    let verified: bool;
    if cfg.is_protected(&name) {
        let display = cfg.display_name_of(&name);
        logln!("[gate] 拦截到受保护应用「{display}」的启动，先验证 ...");

        // 验证：按配置的认证方式走「系统验证(Hello)」或「自设密码」，
        // 密码框支持就地重试与「忘记密码 → Windows 身份验证」的找回流程。
        let secrets = password::AuthSecrets {
            pwd_hash: cfg.master_password_hash.clone(),
            pwd_salt: cfg.master_password_salt.clone(),
            // ★ 标记「是否还是出厂默认密码」。
            // 用途：密码框的副标题会写「当前使用默认密码 123456，请修改」，
            // 免得出厂状态被当成"已经设好密码了"，用户一直不知道密码是什么。
            pwd_is_default: crate::config::is_default_password(
                &cfg.master_password_hash,
                &cfg.master_password_salt,
            ),
        };
        // 分流处理三种结论。★ 关键区别：NoCredentials（既没有 Windows Hello
        // 也没有主密码）绝不能像"验证失败"那样静默退出——用户会以为
        // "双击了没反应"，而真正该做的是去主控台设置一个密码。
        match auth::route_verification(&cfg.auth_method, &display, &secrets) {
            auth::AuthRoute::Passed => {
                verified = true;
                logln!("[gate] 验证通过，请求常驻实例代启动 {target}");
            }
            auth::AuthRoute::NoCredentials => {
                // 无从验证 ≠ 验证失败：必须明确告诉用户下一步做什么，
                // 否则他只会反复双击、反复没反应，以为程序坏了。
                logln!("[gate] 无可用凭据（Hello 不可用且未设主密码），提示去设置后退出");
                fail_box(
                    "HelloGate 无法验证，因为这台电脑没有可用的解锁凭据：\n\n\
                     · Windows Hello 不可用（或没有响应）\n\
                     · 还没有设置主密码\n\n\
                     请打开 HelloGate 主控台，在「认证方式」里选择「自设密码」\n\
                     并设置一个密码，之后受保护的程序才能正常启动。",
                );
            }
            auth::AuthRoute::Failed => {
                // 取消 / 密码错误 / Hello 出错：静默退出（目标程序从未运行，
                // 用户看到的就和加壳器一样"没点动"）。
                logln!("[gate] 验证未通过，不启动程序");
                std::process::exit(0);
            }
        }
    } else {
        // 登记残留（应用已被移除/禁用）：★ 自动清理 + 直接放行，不再让用户重启 HelloGate。
        //
        // 【为什么不能沿用旧版"发给常驻实例代启动"】
        // 常驻实例是提权（管理员）进程，代启动任何名单外程序就等于开了
        // 一个免 UAC 的提权后门（攻击者可以 `hellogate.exe --gate 任意exe`）。
        // 所以拆成两步：
        //   ① 常驻实例只负责**删掉这条失效登记**（HKLM 只有管理员能写）；
        //   ② 本进程（普通权限）自己启动目标——权限和用户双击完全一样，
        //      拿不到更高权限，因此不构成提权原语。
        // 登记删掉之后，系统不会再把它重定向到我们这里，所以不会出现
        // "启动→再次被拦截→无限循环"。
        logln!("[gate] {name} 不在保护列表（登记残留），自动清理后放行");
        release_stale(&name, target, rest);
    }

    request_launch(&name, target, rest, verified);
}

/// 向提权常驻实例发「代启动请求」并等待完成，然后结束本进程（永不返回）。
///
/// # 通信方式：WM_COPYDATA + 命名事件
/// - WM_COPYDATA 是 Windows 自带的跨进程同步传数据消息，无需自建管道/端口。
///   常驻实例用 ChangeWindowMessageFilterEx 放行了这条消息（UIPI 默认会
///   拦截普通权限→管理员权限的消息，这是唯一需要它主动开的口子）。
/// - 请求里带一次性 nonce（随机事件名），常驻实例启动完目标后按该名字
///   打开事件并 SetEvent，守门进程据此确认"真的启动了"，而不是盲发即退。
fn request_launch(name: &str, target: &str, args: &[String], verified: bool) -> ! {
    let nonce = relay_nonce();
    // 组装请求体（JSON，UTF-8）：nonce 事件名 + 应用名 + 目标路径 + 原始参数。
    // gate_pid 让常驻实例能核实"发送者确实是拦截了这次启动的那个守门进程"
    // （常驻实例会按 PID 读我们的真实命令行做绑定校验，见 main.rs handle_relay）。
    let req = serde_json::json!({
        "nonce": nonce,
        "name": name,
        "target": target,
        "args": args,
        "verified": verified,
        "gate_pid": std::process::id(),
    })
    .to_string();

    // 正常是毫秒级；10 秒是兜底上限（常驻实例可能正忙着别的事）。
    match send_relay(&nonce, &req, 10_000) {
        Ok(()) => {
            logln!("[gate] 常驻实例已确认代启动完成，守门进程退出");
            std::process::exit(0);
        }
        // 超时：请求可能被常驻实例的安全校验拒绝（如名单不符），看常驻实例日志定位。
        Err(e) => fail_box(&e),
    }
}

/// 清理「登记残留」并放行目标程序（永不返回）。
///
/// 场景：某个程序在 IFEO 里还连着 HelloGate 的拦截登记，但它已经不在保护列表
/// （被移除、被禁用、总开关关闭、配置被重置等）。用户双击时系统照样把它
/// 重定向到守门进程，如果什么都不做，用户就永远打不开这个程序了。
///
/// 处理顺序（顺序不能颠倒）：
/// 1. 先请常驻实例删登记——HKLM 需要管理员权限，普通权限的守门进程删不了；
/// 2. 登记删干净后，本进程用**当前用户权限**启动目标程序。
///    这一步必须在删登记之后：否则启动会被 IFEO 再次重定向，形成死循环。
fn release_stale(name: &str, target: &str, args: &[String]) -> ! {
    let nonce = relay_nonce();
    let req = serde_json::json!({
        // mode 告诉常驻实例："这次只清理，不要代启动"（见 main.rs 的 relay 分支）。
        "mode": MODE_CLEANUP,
        "nonce": nonce,
        "name": name,
        "target": target,
        "args": args,
        "verified": false,
        "gate_pid": std::process::id(),
    })
    .to_string();

    if let Err(e) = send_relay(&nonce, &req, 5_000) {
        // 最常见的失败是"HelloGate 没在运行"：没有管理员进程帮忙删登记，
        // 只能把原因讲清楚，而不是留一个永远打不开的程序。
        fail_box(&format!(
            "{e}\n\n该程序带有一条失效的 HelloGate 启动拦截登记。\n\
             请先启动 HelloGate（系统托盘），再重新打开此程序。"
        ));
    }

    // 登记已删：现在启动不会被重定向。这里刻意用普通权限 spawn，
    // 与用户在资源管理器里双击完全等价（程序自己要求 UAC 时，系统会正常弹）。
    match std::process::Command::new(target).args(args).spawn() {
        Ok(child) => logln!(
            "[gate] 已清理失效登记并放行: {target} (PID={})",
            child.id()
        ),
        Err(e) => fail_box(&format!("失效登记已清理，但启动程序失败：\n{target}\n{e}")),
    }
    std::process::exit(0);
}

/// 把一条请求发给常驻实例并等待回执（失败返回给用户看的提示文案）。
///
/// 抽成独立函数是因为现在有两条用途完全不同的请求（代启动 / 清理残留），
/// 但"找窗口 → 建事件 → 发消息 → 等回执"这一整套完全一样。
fn send_relay(nonce: &str, req: &str, wait_ms: u32) -> Result<(), String> {
    // 1. 找提权常驻实例的隐藏消息窗口（类名见 main.rs 的 MSG_WINDOW_CLASS）。
    let resident = find_resident().ok_or_else(|| {
        "HelloGate 未在运行，无法处理这次启动。\n请先启动 HelloGate（系统托盘）。".to_string()
    })?;

    // 2. 建一个本进程的隐藏窗口，作为 WM_COPYDATA 的发送者身份凭据。
    let sender = create_gate_window()
        .ok_or_else(|| "HelloGate 守门进程初始化失败，无法处理这次启动。".to_string())?;

    // 3. 创建"完成事件"：名字含 PID+纳秒，保证同一时刻多个守门进程互不串扰。
    let event_name = wide(nonce);
    let event = unsafe {
        CreateEventW(
            None,
            true,  // 手动重置：哪怕 SetEvent 发生在等待之前，信号也不会丢
            false, // 初始未触发
            PCWSTR(event_name.as_ptr()),
        )
    }
    .map_err(|_| "HelloGate 守门进程内部错误（无法创建同步事件）。".to_string())?;

    // 4. 组装 WM_COPYDATA：dwData 是用途标识，lpData 指向 UTF-8 的 JSON 字节。
    let bytes = req.as_bytes();
    let cds = COPYDATASTRUCT {
        dwData: RELAY_DW_DATA,
        cbData: bytes.len() as u32,
        lpData: bytes.as_ptr() as *mut core::ffi::c_void,
    };

    // 5. 同步发送（带超时）：常驻实例即便正卡在某个模态循环里，消息也会
    //    排进它的队列，等它泵到即处理；SMTO_ABORTIFHUNG 防止它真挂死时把我们拖住。
    let sent = unsafe {
        SendMessageTimeoutW(
            resident,
            WM_COPYDATA,
            WPARAM(sender.0 as usize),
            LPARAM(&cds as *const _ as isize),
            SMTO_ABORTIFHUNG,
            5000,
            None,
        )
    };
    if sent.0 == 0 {
        unsafe { let _ = CloseHandle(event); }
        return Err("HelloGate 无响应，无法处理这次启动。\n请重新打开 HelloGate 后重试。".to_string());
    }

    // 6. 等常驻实例把事办完（删登记 / 启动程序），再决定下一步。
    let wait = unsafe { WaitForSingleObject(event, wait_ms) };
    unsafe {
        let _ = CloseHandle(event);
    }
    if wait == WAIT_OBJECT_0 {
        Ok(())
    } else {
        Err("HelloGate 未能受理本次启动请求。\n请打开 HelloGate 日志查看原因。".to_string())
    }
}

/// 守门窗口的窗口过程：什么都不做，全交系统默认处理。
unsafe extern "system" fn gate_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// 按类名找提权常驻实例的隐藏消息窗口。
///
/// 窗口枚举不受 UIPI 限制（只有"发消息"受限），普通权限进程能找到高权限窗口。
fn find_resident() -> Option<HWND> {
    let class_name = wide(crate::MSG_WINDOW_CLASS);
    unsafe {
        let hwnd = FindWindowW(PCWSTR(class_name.as_ptr()), PCWSTR::null()).ok()?;
        if hwnd.0.is_null() {
            None
        } else {
            Some(hwnd)
        }
    }
}

/// 创建守门进程自己的隐藏窗口（仅作 WM_COPYDATA 发送者身份，不参与消息循环）。
fn create_gate_window() -> Option<HWND> {
    unsafe {
        let hinstance = GetModuleHandleW(None).ok()?;
        let class_name = wide(GATE_WINDOW_CLASS);
        let wc = WNDCLASSW {
            // 守门窗口不需要处理任何消息，转发给系统默认过程即可。
            // （windows 0.62 里 DefWindowProcW 是 Rust ABI，不能直接当
            //  "system" ABI 的窗口过程用，必须包一层 extern "system" fn。）
            lpfnWndProc: Some(gate_wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassW(&wc); // 重复注册会失败，忽略
        CreateWindowExW(
            Default::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR::null(),
            WS_OVERLAPPED, // 不给 WS_VISIBLE，窗口不可见
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        )
        .ok()
    }
}

/// 生成一次性 nonce 事件名：进程 ID + 单调纳秒，足够唯一。
fn relay_nonce() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("HelloGateRelay-{}-{}", std::process::id(), nanos)
}

/// 弹提示框告知用户启动失败，并以失败码结束本进程（永不返回）。
fn fail_box(msg: &str) -> ! {
    logln!("[gate] {msg}");
    // 自绘对话框（dialog.rs）替代原生 MessageBoxW：
    // 守门进程此时还没有任何消息循环，dialog::warn 内部自带模态循环，
    // 所以照样能弹出来并等到用户点掉——和原 MessageBox 一样阻塞。
    crate::dialog::warn(None, "HelloGate", msg);
    std::process::exit(1);
}

/// Rust 字符串 → NUL 结尾 UTF-16（Win32 宽字符 API 用）。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
