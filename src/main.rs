//! HelloGate 主程序（Phase 4：完整功能）
//!
//! 本文件把各模块串成完整的常驻服务：
//!   - `config`    —— 受保护应用列表（DPAPI 加密存储）
//!   - `state`     —— 宽限期状态机（一次性 / 时间型 / 会话型）
//!   - `session`   —— 锁屏 / 睡眠唤醒事件监听（自动重锁）
//!   - `hook`      —— 监听前台窗口切换（事件驱动，零轮询）
//!   - `process`   —— 由窗口句柄解析出 exe 文件名
//!   - `overlay`   —— 亚克力毛玻璃遮罩（不杀进程）
//!   - `auth`      —— Windows Hello 解锁
//!   - `tray`      —— 系统托盘图标与菜单
//!   - `autostart` —— 开机自启（HKCU\...\Run）
//!   - `console`   —— 主控台配置界面
//!   - `ifeo`      —— IFEO 启动拦截登记（"先验证、后运行"的注册表侧）
//!   - `gater`     —— 守门进程（被拦截程序的验证与代启动请求）
//!   - `password`  —— 主密码（Windows Hello 不可用时的回退）
//!
//! # 架构说明（线程模型）
//! - **主线程**：运行消息循环，同时承载「隐藏消息窗口」，接收托盘回调
//!   （`WM_TRAY_ICON`）与会话事件（`WM_WTSSESSION_CHANGE` / `WM_POWERBROADCAST`）。
//! - **Hook 线程**：`SetWinEventHook` 需要安装它的线程持续泵消息，所以单独开一个
//!   线程跑钩子 + 消息循环，事件通过 channel 发回主线程处理。
//! - **认证线程**：每次解锁弹 Windows Hello 都在独立线程里做，避免阻塞 UI。

// 隐藏 Windows 下弹出的黑色控制台窗口（release 版生效）。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod auth;
// 「关于作者」浮层：顶栏点作者名字时弹出，展示公众号与赞赏码。
mod about;
mod applist;
mod autostart;
mod config;
mod console;
// 自绘提示/确认对话框（UI 优化方案 A1）：替代全部系统 MessageBoxW
mod dialog;
// 组件画廊（调试用静态展示窗，决策 D3）：托盘右键菜单打开，
// WM_PAINT 纯绘制各控件各状态，用于目视核对视觉体系。
mod gallery;
mod gater;
mod hook;
mod ifeo;
// 授权码激活（v1.0.0）：首次启动弹激活框，输入公众号「艺杯羹」领取的授权码。
mod license;
mod overlay;
mod password;
mod process;
mod session;
mod state;
// 轻量 Toast 提示（UI 优化方案 A1）：无需决策的短反馈
mod toast;
mod tray;
// 检查更新（v1.0.0）：WinHTTP 查 GitHub 最新 release + 「发现新版本」下载卡片。
mod update;
// 两个窗口（主控台 / 解锁弹窗）共用的一套配色与绘图基元。
// 单独成模块是为了防止两边各写一份、慢慢长成两种样子（见 ui.rs 顶部说明）。
mod ui;

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    EVENT_MODIFY_STATE, GetCurrentProcessId, OpenEventW, SetEvent,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowW,
    GetMessageW, PostMessageW, PostQuitMessage, RegisterClassW, TranslateMessage,
    MSG, MSGFLT_ALLOW, SW_HIDE, WM_APP, WM_COPYDATA, WNDCLASSW,
    WS_OVERLAPPED,
};

use config::Config;
use state::StateMachine;

// ============================================================================
// 日志基础设施
// ============================================================================
//
// 【为什么不用 println!】
// HelloGate 编译成 GUI 子系统程序（`windows_subsystem = "windows"`），
// 用户双击运行时进程**没有控制台**。实测在这种情况下一整条标准输出链路是"哑"的：
// `println!` 的内容既不会报错、也不会落盘，日志文件永远是 0 字节。
// （也试过用 `SetStdHandle` 把 stdout 重定向到日志文件——对双击启动的进程同样无效，
//  因为 Rust 的 stdio 在进程启动阶段就已经把无效句柄判定成"丢弃所有输出"了。）
//
// 结论：日志必须自己写文件，不能依赖标准输出。下面这套 `write_log` + `logln!`
// 就是干这个的，用法和 `println!` 完全一样，但保证能落到磁盘上。

/// 日志文件路径：`%APPDATA%\HelloGate\hellogate.log`，与配置文件放在一起，便于用户打包发给我们。
fn log_path() -> PathBuf {
    config::config_path().with_file_name("hellogate.log")
}

/// 向日志文件追加一行文本。
///
/// 每次调用都「打开 → 写入 → 关闭」。日志量很小（只在状态变化时写几条），
/// 这样比维护一个全局文件句柄更简单，也天然避开了多线程写同一个句柄的锁问题。
pub fn write_log(msg: &str) {
    use std::io::Write;

    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{msg}");
    }
}

/// 写日志的简写宏，用法与 `println!` 完全一致。
macro_rules! logln {
    ($($arg:tt)*) => {
        $crate::write_log(&format!($($arg)*))
    };
}
// 让其它模块也能用 `crate::logln!`（见 console.rs / config.rs / overlay.rs）。
pub(crate) use logln;

/// 全局共享的「运行时状态」（配置 + 状态机 + 事件通道）。
///
/// # 为什么用 Mutex 包一层？
/// 主线程要处理窗口消息（读配置、改状态机），认证线程解锁成功后也要改状态机，
/// 两者跨线程访问，必须加锁。用 Mutex 最简单直接。
struct Runtime {
    /// 当前配置（用户在主控台保存后会被替换）。
    cfg: Config,
    /// 宽限期状态机。
    sm: StateMachine,
    /// 会话事件发送端（隐藏窗口过程用它投递锁屏/睡眠事件）。
    session_tx: Sender<session::SessionEvent>,
    /// 上一次处于前台的「受保护应用」的 exe 小写文件名。
    ///
    /// 有了它，当用户从受保护应用切到别的窗口时，我们就能精确调用
    /// `StateMachine::on_leave`，只让「那一个应用」的一次性宽限期失效，
    /// 而不是粗暴地清空所有解锁记录。
    last_protected: Option<String>,
}

/// 隐藏消息窗口的类名。
///
/// 除了"单实例判定"，守门进程（gater.rs）也按这个类名 FindWindow 找到常驻实例，
/// 向其发 WM_COPYDATA 代启动请求，所以必须 pub 并保持稳定。
pub const MSG_WINDOW_CLASS: &str = "HelloGateMsgWindowClass";

/// 提权自举的命令行标记。
///
/// 非提权实例用 `runas` 拉起自己时会带上这个参数；提权后的实例看到它就跳过
/// 提权检查直接常驻，否则"检查权限→再提权→再检查"会无限弹 UAC。
const ELEVATED_FLAG: &str = "--elevated";

/// 自定义消息：请求打开主控台。
///
/// # 为什么需要这条消息？
/// 有两条路径需要「让主控台弹出来」：
/// 1. 用户**首次**双击 exe（此时还没有配置，不知道该保护什么）；
/// 2. 用户**再次**双击 exe（此时程序已在后台运行，不该再起一个实例）。
///
/// 两条路径都不能直接调用 `open_console()`，因为主控台是阻塞式模态窗口，
/// 必须在窗口消息循环已经跑起来之后才能安全打开。
/// 所以统一做法是：投递这条消息给自己，等消息循环处理到它时再真正打开。
///
/// # 为什么要用 `WM_APP + 2` 而不是 `+ 1`？
/// `WM_APP`（0x8000）往后是留给应用自定义的消息区间。
/// 其中 `+ 1` 已经被托盘模块拿去当 `WM_TRAY_ICON` 了（见 `tray.rs`）。
/// 如果这里也用 `+ 1`，两个消息的数值完全相同，窗口过程会先命中托盘的判断分支，
/// 结果就是"消息发出去了但界面没出来"——这个坑踩过一次，所以务必保持编号唯一。
const WM_HELLOGATE_SHOW_CONSOLE: u32 = WM_APP + 2;

/// 「主控台保存成功」→ 在屏幕右下角浮一条 Toast。
///
/// # 为什么靠消息转发，而不是在主控台线程里直接 `toast::show`
/// 保存成功后主控台窗口随即关闭、它的线程也马上结束——线程一退出，
/// 它建的任何窗口都会被系统连带销毁，Toast 根本来不及播完动画。
/// 主线程的隐藏消息窗口有**常驻的**消息循环，把"该弹 Toast 了"这条消息
/// 投给它、由它去 `toast::show`，动画才有循环可依托（见 message_wnd_proc）。
///
/// 编号规则同上：`WM_APP + 1` 托盘、`+ 2` 开主控台，这里必须是 `+ 3`，
/// 撞号的后果是消息发出去了但没有任何窗口去响应。
const WM_HELLOGATE_TOAST: u32 = WM_APP + 3;

/// 进程级运行时状态（延迟初始化，见 `main`）。
static RUNTIME: Mutex<Option<Runtime>> = Mutex::new(None);

/// 正在验证中的受保护应用名（None = 没有验证在进行）。
///
/// 【为什么需要它】验证弹窗（我们自己的密码框 / 系统 Hello 对话框）出现时会抢前台，
/// 于是系统发来"切到了非受保护应用"事件。若照常处理就会把遮罩隐藏，
/// 让受保护内容在用户输密码的过程中完全暴露（实际踩过的 bug）。
/// 有了这个标记，事件处理就能识别"前台切走其实是验证弹窗"，保持遮罩不动。
static AUTH_GUARD: Mutex<Option<String>> = Mutex::new(None);

fn main() {
    // 0. 声明「DPI 感知」，必须在创建任何窗口之前完成。
    //
    // 【为什么必须做】
    // 现代笔记本普遍使用 125% / 150% / 200% 的缩放。如果进程不声明 DPI 感知，
    // Windows 会把它当成"老程序"做位图拉伸：我们按 720x520 创建的窗口会被
    // 系统悄悄放大成 1440x1040（200% 屏），而内部控件仍按 720x520 布局，
    // 结果就是界面只填满窗口的一个角落、文字发虚。
    // 声明 PER_MONITOR_AWARE_V2 之后，我们拿到的坐标就是真实物理像素，自己按
    // DPI 比例缩放布局，界面在任何缩放下都清晰、不走样。
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // 0.5 在日志里打一条分隔线，方便把多次启动的记录区分开。
    //     顺带打出「exe 的编译时刻」：排查"界面为什么还是旧样子"时，
    //     对比它和这条启动记录的时间即可——启动晚于编译 = 跑的是新代码；
    //     启动早于编译 = 还在跑内存里的旧进程，退出重启即可（今天实测三连坑）。
    logln!("\n========== HelloGate 启动 {} ==========", now_text());
    logln!("构建编译于: {}", build_ts_text());

    // 0.8 ★ 守门模式分派（IFEO 启动拦截，见 ifeo.rs / gater.rs）。
    //
    // 受保护程序被启动时，系统实际拉起的是 `hellogate.exe --gate <原程序> <原参数>`。
    // 守门进程验证通过后替系统启动真程序，不通过则程序根本不运行。
    //
    // 【为什么必须放在单实例守卫之前】
    // 守门进程是一次性辅助进程，不是"第二个常驻实例"：
    // 若先走单实例检查，会被当成重复启动而直接退出（通知别人开主控台），
    // 受保护程序就永远启动不了；也不能弹主控台窗口——用户只是想开个 Word。
    let argv: Vec<String> = std::env::args().collect();

    if argv.len() >= 2 && argv[1] == ifeo::GATE_FLAG {
        gater::run(&argv[1..]);
    }

    // 0.9 ★ 提权自举（管理员模式启动拦截的前提，见 ifeo.rs 模块头）。
    //
    // 【为什么必须提权】IFEO 的 Debugger 重定向只在 HKLM 生效（HKCU 实测系统
    // 根本不认），而写 HKLM 需要管理员权限。所以常驻实例必须提权。
    //
    // 【为什么不用 manifest requireAdministrator】守门子进程（hellogate --gate）
    // 是系统经 IFEO 拉起的，manifest 强制提权会让每次启动受保护程序都弹 UAC。
    // asInvoker + 运行时自举：开机/启动时只弹一次 UAC，守门进程保持普通权限，
    // 才能正常弹 Windows Hello / 密码框。
    //
    // 【为什么放在单实例守卫之前】非提权实例若走到单实例检查，会向提权实例
    // PostMessage——跨权限消息默认被 UIPI 拦截，等于白发。这里自己处理：
    // 已有常驻实例就只发"开主控台"请求（提权窗口已放行该消息），否则发起 UAC。
    let elevated_requested = argv.len() >= 2 && argv[1] == ELEVATED_FLAG;
    if !elevated_requested && unsafe { !IsUserAnAdmin().as_bool() } {
        if let Some(existing) = find_existing_instance() {
            unsafe {
                let _ = PostMessageW(Some(existing), WM_HELLOGATE_SHOW_CONSOLE, WPARAM(0), LPARAM(0));
            }
            logln!("[main] 已有常驻实例在运行，已通知其打开主控台（非提权实例退出）");
            return;
        }
        unsafe {
            let exe = std::env::current_exe().unwrap_or_default();
            let exe_w = wide(&exe.to_string_lossy());
            let flag_w = wide(ELEVATED_FLAG);
            let result = ShellExecuteW(
                None,
                w!("runas"),
                PCWSTR(exe_w.as_ptr()),
                PCWSTR(flag_w.as_ptr()),
                None,
                SW_HIDE,
            );
            // ShellExecuteW 成功返回 >32 的句柄；失败返回 0~32 的错误码
            // （用户在 UAC 里点"否"就是 SE_ERR_ACCESSDENIED=5）。
            if (result.0 as isize) <= 32 {
                logln!("[main] UAC 提权被取消或失败，本次不启动（启动拦截需要管理员权限）");
                // 自绘对话框（dialog.rs）替代系统 MessageBoxW——A1 的最后
                // 一处调用点。此时消息循环还没进，dialog 内部自带模态循环。
                crate::dialog::warn(
                    None,
                    "HelloGate",
                    "HelloGate 需要管理员权限才能拦截程序启动。\n请在 UAC 弹窗中点击「是」后重试。",
                );
            } else {
                logln!("[main] 已发起 UAC 提权，提权后的实例将常驻后台");
            }
        }
        return;
    }

    // 1. 单实例守卫：如果已经有实例在后台跑，就不要起第二个。
    //
    // 【为什么需要它】
    // HelloGate 常驻托盘，用户第一次双击后程序就一直在跑。如果用户想"打开界面"
    // 又双击了一次 exe，就会出现两个实例抢同一批窗口事件（谁弹遮罩不可预测）。
    // 正确做法是：通知已有实例把主控台弹出来，然后自己立刻退出。
    if let Some(existing) = find_existing_instance() {
        unsafe {
            // 投递一条自定义消息，让对方在它自己的消息循环里打开主控台。
            // 用 PostMessage（异步、不阻塞）而不是 SendMessage，避免在对方忙时卡住。
            let _ = PostMessageW(Some(existing), WM_HELLOGATE_SHOW_CONSOLE, WPARAM(0), LPARAM(0));
        }
        logln!("[main] 检测到已有实例在运行，已通知它打开主控台");
        return;
    }

    // 2. 判断是不是「首次运行」——配置文件的存不存在就是标志。
    //
    // 【为什么要判断这个】
    // 首次运行时用户对 HelloGate 一无所知，如果只是默默缩到托盘（而 Windows 11 又把
    // 新托盘图标折叠进 ^ 里），用户会觉得"双击了根本没反应"。
    // 所以首次运行必须主动把主控台亮出来，给用户一个明确的入口。
    let first_run = !config::config_path().exists();

    // 3. 加载配置（DPAPI 解密；失败自动回退默认）
    let mut cfg = config::load();

    // ★ 授权码门（v1.0.0）：未激活时先弹激活框（模态、阻塞）。
    //
    // 【为什么放在这里】
    // - 在 IFEO 登记同步之前：没激活就不该改动系统注册表；
    // - 在主控台打开之前：激活是"用软件前的第一道门"，顺序符合直觉；
    // - 提权/单实例之后：避免多个实例各弹一次激活框打架。
    //
    // 【取消激活 = 退出进程】用户点 ✕ / Esc 放弃输入授权码时，软件直接退出，
    // 不做任何系统改动——这是"授权"语义的自然结果。
    // （--gate 短命子进程在上面已经分派返回，不会走到这里，拦截不受激活影响。）
    if !license::ensure_activated(&mut cfg) {
        logln!("[main] 未激活，退出（不写注册表、不启动托盘）");
        return;
    }
    logln!("========================================================");
    logln!("  HelloGate —— 非侵入式 Windows 应用锁");
    logln!("========================================================");
    logln!("受保护应用 {} 个", cfg.apps.len());
    logln!(
        "启动前验证: {}，运行中遮罩: {}",
        if cfg.guard_enabled { "开启" } else { "已关闭" },
        if cfg.overlay_enabled { "开启" } else { "关闭" }
    );
    logln!("宽限期模式: {:?}，时长 {} 秒", cfg.grace_mode, cfg.grace_seconds);

    // 3.2 同步 IFEO 启动拦截登记（「先验证、后运行」）。
    //     每次启动都同步的原因：
    //     - exe 被移动/重命名 → 登记里的旧路径失效，重建后重定向才不会启动失败；
    //     - 守门进程曾在"删除登记→恢复"之间崩溃 → 这里补回，保护不留缺口；
    //     - 配置被外部改动 → 登记跟着对齐，失效条目顺手清理。
    if let Err(e) = ifeo::sync(&cfg) {
        logln!("[ifeo] 同步启动拦截登记失败: {e}");
    }

    // 3.5 自启状态校正：以注册表实际状态为准，避免「配置说开了、注册表里没有」
    //     （比如用户手动删除了启动项）导致的不一致。
    let actual_autostart = autostart::is_enabled();
    if actual_autostart != cfg.autostart {
        logln!(
            "[autostart] 配置({}) 与注册表({}) 不一致，按注册表实际状态同步",
            cfg.autostart, actual_autostart
        );
        // 把实际状态写回配置，保证后续保存时不会再翻转回去。
        let mut cfg = cfg;
        cfg.autostart = actual_autostart;
        // 落盘保存（失败也不致命，仅提示）
        if let Err(e) = config::save(&cfg) {
            logln!("[autostart] 同步配置保存失败: {e}");
        }
        // 用同步后的配置继续初始化运行时
        return run_with_config(cfg, first_run);
    }

    run_with_config(cfg, first_run)
}

/// 找当前是否已有 HelloGate 实例在跑。
///
/// # 原理
/// 每个正在运行的实例都会创建一个「隐藏消息窗口」，类名固定为 `MSG_WINDOW_CLASS`。
/// 用 `FindWindowW` 按类名去找，找得到就说明已经有实例了。
///
/// # 注意
/// 还没注册窗口类的进程（也就是第一次启动的那一瞬间）去找，会返回"找不到"，
/// 所以这个检查必须放在创建窗口之前——而 main 里它确实在最前面。
fn find_existing_instance() -> Option<HWND> {
    let class_name = wide(MSG_WINDOW_CLASS);
    unsafe {
        let hwnd = FindWindowW(PCWSTR(class_name.as_ptr()), PCWSTR::null()).ok()?;
        if hwnd.0.is_null() {
            None
        } else {
            Some(hwnd)
        }
    }
}

/// 生成一个用于日志分隔的简短时间戳（格式：2026-10-29 14:03:22）。
///
/// # 为什么不用 chrono 之类的库？
/// 只为了在日志里打一行分隔符，引入一个第三方时间库不值得。
/// 这里用 SystemTime 手动换算，几十行内解决，且零依赖。
fn now_text() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    now_text_from_secs(secs)
}

/// 把 Unix 秒数格式化成北京时间「YYYY-MM-DD HH:MM:SS」。
/// （中国时区固定 UTC+8、无夏令时，简单加偏移即可；now_text / build_ts_text 共用。）
fn now_text_from_secs(secs: i64) -> String {
    // 把 Unix 时间戳换成「年月日 时分秒」。
    // 这里只做简单的 UTC+8 换算（中国时区），不处理夏令时（中国没有夏令时）。
    let local = secs + 8 * 3600;
    let (days, rem) = (local.div_euclid(86_400), local.rem_euclid(86_400));
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // 由「1970-01-01 起的天数」反推公历年月日
    let (y, m, d) = ymd_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// 把编译脚本注入的 HELLOGATE_BUILD_TS（Unix 秒）格式化成可读时间。
///
/// 【为什么不用直接输出数字】日志是给用户和我们双方看的，
/// "2026-11-06 14:24" 比 "1773..." 一眼能懂，能直接和文件修改时间对比。
///
/// 【为什么是 pub(crate)】关于页（about.rs）要在卡片底部标注
/// "HelloGate v版本 · 构建于 时间"，构建时间就从这里取；
/// pub(crate) = 只在本 crate 内可见，对外部使用者仍然隐藏。
pub(crate) fn build_ts_text() -> String {
    let secs: i64 = option_env!("HELLOGATE_BUILD_TS")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if secs <= 0 {
        return "未知（未注入编译时间戳）".to_string();
    }
    // 复用 now_text 的"Unix 秒 → 北京时间"换算：先转天数再反推年月日太绕，
    // 直接借用 now_text 的内部实现——它接收秒数，这里把格式化抽出来共用。
    now_text_from_secs(secs)
}

/// 把「自 1970-01-01 起的天数」换算成 (年, 月, 日)。
///
/// 算法很朴素：从 1970 年开始一年一年往上加，直到剩余天数不够一整年为止；
/// 再在一个月一个月往上加。对日志用途来说完全够用，且不用查表。
fn ymd_from_days(mut days: i64) -> (i64, i64, i64) {
    let mut year = 1970i64;
    loop {
        let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
        let year_days = if leap { 366 } else { 365 };
        if days < year_days {
            break;
        }
        days -= year_days;
        year += 1;
    }

    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    // 每个月的天数（二月按闰年调整）
    let months = [
        31,
        if leap { 29 } else { 28 },
        31, 30, 31, 30, 31, 31, 30, 31, 30, 31,
    ];

    let mut month = 1i64;
    for len in months {
        if days < len {
            break;
        }
        days -= len;
        month += 1;
    }

    (year, month, days + 1)
}

/// 用最终确定的配置启动运行时（把 main 的后续步骤独立出来，
/// 便于「自启状态校正」分支复用）。
///
/// # 参数
/// - `cfg`：最终生效的配置。
/// - `first_run`：是否首次运行。为 `true` 时会在初始化完成后主动弹出主控台。
fn run_with_config(cfg: config::Config, first_run: bool) {
    // 2. 初始化运行时状态
    let (session_tx, session_rx) = mpsc::channel::<session::SessionEvent>();
    let sm = StateMachine::new(cfg.grace_mode, cfg.grace_seconds);
    // 【为什么要先记下来】cfg 马上就要被移进下面的 RUNTIME，之后的代码
    // 只能按名字去锁里取；这里顺手算好"要不要启用运行中遮罩"，
    // 后面的线程启动判断直接用这个布尔值即可。
    let overlay_on = cfg.guard_enabled && cfg.overlay_enabled;
    // 是否仍是出厂默认密码（主密码一个都还没设过）。
    // 【为什么要在这里算】cfg 马上就要被移动进 RUNTIME，之后只能加锁去取；
    // 更重要的是，托盘气泡要在"图标刚加好"的那一刻发，此时拿一个布尔值最省事。
    let pwd_default = crate::config::is_default_password(
        &cfg.master_password_hash,
        &cfg.master_password_salt,
    );
    *RUNTIME.lock().unwrap() = Some(Runtime {
        cfg,
        sm,
        session_tx: session_tx.clone(),
        last_protected: None,
    });

    // 3. 创建隐藏消息窗口（接收托盘 + 会话消息），并注册会话通知
    let msg_hwnd = create_message_window();
    if let Some(hwnd) = msg_hwnd {
        // ★ 放行两条来自普通权限进程的跨权限消息（UIPI 默认全部拦截）：
        //   - WM_COPYDATA：守门进程的代启动请求（见 handle_relay）；
        //   - WM_HELLOGATE_SHOW_CONSOLE：用户再次双击 exe 时，非提权实例请本实例弹主控台。
        // 只放行这两条具体消息，其它消息依旧被系统拦截，攻击面最小。
        unsafe {
            let _ = ChangeWindowMessageFilterEx(hwnd, WM_COPYDATA, MSGFLT_ALLOW, None);
            let _ = ChangeWindowMessageFilterEx(hwnd, WM_HELLOGATE_SHOW_CONSOLE, MSGFLT_ALLOW, None);
        }

        if let Err(e) = session::register(hwnd) {
            logln!("[main] 注册会话通知失败: {e}");
        }
        // 显示托盘图标。
        // 【为什么不再读配置】旧配置里的 show_tray 字段在界面上从来没有开关，
        // 保存时恒被写成 true，属于"死配置"。托盘是重新打开主控台的重要入口
        // （Windows 11 会把新图标折叠进 ^，但展开就能找到盾牌），始终显示。
        // 旧配置文件里残留的 show_tray 字段会被 serde 自动忽略，无需迁移。
        if let Err(e) = tray::add_icon(hwnd) {
            logln!("[main] 添加托盘图标失败: {e}");
        } else if pwd_default && !first_run {
            // ★ 出厂默认密码状态：发一条托盘气泡，提醒去设置自己的密码。
            //
            // 【为什么这件事必须提示】这种状态下解锁流程是**直接放行**的
            // （见 password::password_unlock_flow），也就是此刻并没有人拦着。
            // 用户如果不知道，会误以为 HelloGate 已经在保护他了。
            //
            // 【为什么用托盘气泡而不是弹窗】弹窗会打断用户手头的事，
            // 而且此刻他可能只是开机、并没有想配置软件。气泡不抢焦点、
            // 自动消失，点托盘图标就能顺路去设置——成本最低的提醒方式。
            //
            // 【为什么首次运行不弹】首次运行本来就会自动打开主控台，
            // 那里已经有一条琥珀色提示，再弹气泡是重复打扰。
            //
            // 【文本里为什么没有密码值】把密码印在提示里等于告诉所有
            // 能看到这台屏幕的人"钥匙在哪儿"，提示只需要说"去设置密码"。
            tray::show_balloon(
                hwnd,
                "HelloGate 尚未设置主密码",
                "当前打开受保护程序不会验证。点托盘图标打开主控台，设置自己的密码后才会真正拦截。",
            );
            logln!("[main] 出厂默认密码状态：已通过托盘气泡提示用户去设置主密码");
        }

        // 【首次运行的关键一步】主动弹出主控台。
        //
        // 这里不能直接调用 open_console()——它是阻塞的模态窗口，会把本函数卡住，
        // 导致后面的钩子线程、消息循环都启动不了。
        // 所以先投递一条消息给「自己」，等下面的消息循环跑起来后自然会处理到它。
        if first_run {
            unsafe {
                let _ = PostMessageW(Some(hwnd), WM_HELLOGATE_SHOW_CONSOLE, WPARAM(0), LPARAM(0));
            }
            logln!("[main] 首次运行，将自动打开主控台引导用户添加应用");
        }
    } else {
        logln!("[main] 警告：隐藏消息窗口创建失败，托盘与会话重锁将不可用");
    }

    // 4. 启动会话事件消费线程（锁屏 / 睡眠 → 重锁）
    start_session_worker(session_rx);

    // 5. 启动 Hook 线程：监听前台窗口切换（仅用于"运行中遮罩"）。
    //
    // ★ 本产品默认只做「打开软件前验证」：overlay_enabled 默认 false，
    //   所以这里**不会**装前台窗口钩子，也就没有"用着用着突然被盖住"
    //   这类问题。历史上遮罩链路还是卡死事故的主要来源，默认不起它最稳。
    if overlay_on {
        start_hook_thread();
        logln!("[main] 运行中遮罩已启用（高级开关），后台监听前台窗口切换");
    } else {
        logln!("[main] 运行中遮罩未启用：只在程序启动前验证，运行中不再拦截");
    }

    // 6. 主线程进入消息循环（承载隐藏窗口，处理托盘/会话消息）
    logln!("[main] 进入消息循环，HelloGate 已在后台运行。");
    run_message_loop();
}

// ============================================================================
// 隐藏消息窗口：接收托盘回调与会话事件
// ============================================================================

/// 创建隐藏消息窗口。
///
/// 它从不显示，仅用于接收系统消息（托盘点击、锁屏/睡眠通知）。
fn create_message_window() -> Option<HWND> {
    unsafe {
        let hinstance = GetModuleHandleW(None).ok()?;
        let class_name = wide(MSG_WINDOW_CLASS);

        let wc = WNDCLASSW {
            lpfnWndProc: Some(message_wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        // 重复注册会失败，忽略即可。
        let _ = RegisterClassW(&wc);

        let title = wide("HelloGate Message Window");
        // WS_OVERLAPPED 但不给 WS_VISIBLE，所以窗口永远不可见。
        CreateWindowExW(
            Default::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED,
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

/// 隐藏消息窗口的窗口过程。
unsafe extern "system" fn message_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // 1. 会话 / 电源事件（锁屏、睡眠唤醒等）
    if let Some(result) = forward_session_message(msg, wparam) {
        return LRESULT(result);
    }

    // 2. 托盘图标回调（鼠标事件都通过这条自定义消息进来）
    if msg == tray::WM_TRAY_ICON {
        // lparam 的低位是鼠标消息（如 WM_RBUTTONUP = 0x0205）
        let mouse_msg = (lparam.0 as u32) & 0xFFFF;
        const WM_RBUTTONUP: u32 = 0x0205;
        const WM_LBUTTONUP: u32 = 0x0202;
        // WM_LBUTTONDBLCLK = 0x0203：用户双击托盘图标时 Windows 发来的消息。
        const WM_LBUTTONDBLCLK: u32 = 0x0203;
        if mouse_msg == WM_RBUTTONUP {
            // 右键 → 弹菜单
            if let Some(cmd) = tray::show_menu(hwnd) {
                handle_tray_command(hwnd, cmd);
            }
        } else if mouse_msg == WM_LBUTTONUP || mouse_msg == WM_LBUTTONDBLCLK {
            // 左键单击或双击 → 打开主控台。
            // 【为什么双击也要处理】旧代码只处理了单击（WM_LBUTTONUP），而用户
            // 双击托盘图标时 Windows 发的是 WM_LBUTTONDBLCLK、中间不会再补一条
            // 单击消息，导致"双击托盘没反应、只有单击才开"（v1.0.0 用户实测反馈）。
            // 两种都接上，无论用户习惯单击还是双击都能打开主控台。
            // 重复触发由 open_console 的 CAS 旗标挡住，不会叠出两个窗口。
            handle_tray_command(hwnd, tray::CMD_OPEN_CONSOLE);
        }
        return LRESULT(0);
    }

    // 3. 请求打开主控台。
    //    来源有两种：首次运行时的自动引导，或用户重复双击 exe（由新实例投递过来）。
    if msg == WM_HELLOGATE_SHOW_CONSOLE {
        open_console();
        return LRESULT(0);
    }

    // 3.5 保存成功的轻量反馈（A1）。
    //     主线程循环常驻，Toast 在这里播完 3 秒动画后自毁；
    //     它**不发** WM_QUIT（发了会把主循环一起退出，见 toast.rs 的说明）。
    if msg == WM_HELLOGATE_TOAST {
        crate::toast::save_ok();
        return LRESULT(0);
    }

    // 4. ★ 代启动请求（守门进程验证通过后发来，见 gater.rs / handle_relay_worker）。
    //
    // 【主线程只做"解析"，其余全部交给工作线程】WM_COPYDATA 的 lpData 指向
    // 发送方的内存，只在本次消息处理期间有效，所以必须在这里立刻把 JSON
    // 拷贝成 Rust 数据；之后的所有校验（跨进程读内存、注册表读写）都可能慢，
    // 绝不能占着主线程——主线程一卡，托盘/主控台/会话事件全部停摆
    // （2026-11-01 实测教训）。
    if msg == WM_COPYDATA {
        let cds = &*(lparam.0 as *const COPYDATASTRUCT);
        if cds.dwData == gater::RELAY_DW_DATA && cds.cbData > 0 && !cds.lpData.is_null() {
            let bytes = std::slice::from_raw_parts(cds.lpData as *const u8, cds.cbData as usize);
            let parsed = std::str::from_utf8(bytes)
                .ok()
                .and_then(|t| serde_json::from_str::<RelayReq>(t).ok());
            match parsed {
                Some(req) => {
                    // HWND 不是 Send 类型，按裸数值交给工作线程再还原
                    // （与 hook.rs 传 RawEvent.hwnd 同一套做法）。
                    let sender_addr = wparam.0 as isize;
                    thread::spawn(move || handle_relay_worker(sender_addr, req));
                }
                None => logln!("[relay] 请求体解析失败，忽略"),
            }
        }
        return LRESULT(1);
    }

    if msg == windows::Win32::UI::WindowsAndMessaging::WM_DESTROY {
        PostQuitMessage(0);
        return LRESULT(0);
    }

    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// 把会话/电源消息转发到会话事件通道。
fn forward_session_message(msg: u32, wparam: WPARAM) -> Option<isize> {
    let guard = RUNTIME.lock().unwrap();
    let rt = guard.as_ref()?;
    session::handle_message(msg, wparam, &rt.session_tx)
}

// ============================================================================
// 代启动请求（管理员模式启动拦截的常驻侧，与 gater.rs 配对）
// ============================================================================

/// 代启动请求体（守门进程经 WM_COPYDATA 发来的 JSON，字段见 gater.rs）。
#[derive(serde::Deserialize)]
struct RelayReq {
    /// 请求用途：空串 = 代启动（默认，保持老客户端兼容）；
    /// `cleanup` = 只清理失效登记、不启动程序（见 gater.rs 的 release_stale）。
    #[serde(default)]
    mode: String,
    /// 一次性事件名：启动完成后按它打开事件并 SetEvent，守门进程据此确认结果。
    nonce: String,
    /// 受保护应用的小写文件名（如 winword.exe）。
    name: String,
    /// 目标程序完整路径（IFEO 追加在命令行里的那个）。
    target: String,
    /// 原始命令行参数（双击 .docx 时的文档路径等），原样传给目标。
    #[serde(default)]
    args: Vec<String>,
    /// 守门进程是否已完成用户验证（未验证的请求一律拒绝，防竞态绕过）。
    verified: bool,
    /// 发送请求的守门进程 PID。
    ///
    /// 【为什么需要它】WM_COPYDATA 的 wParam（发送方窗口句柄）是发送方自己填的，
    /// 系统不校验，可以被伪造成"某个真正守门进程的窗口句柄"。但 PID 我们会去
    /// 系统里核实：按它打开进程、确认是 hellogate.exe、且读出的真实命令行
    /// （见 process::command_line）里带着本次请求的目标路径——伪造的 PID
    /// 要么对应的进程不是守门进程，要么命令行对不上，都过不了关。
    #[serde(default)]
    gate_pid: u32,
}

/// 代启动请求的处理线程：六道校验 + "删登记→启动→恢复" + 通知事件，全部在此执行。
///
/// 【为什么整体放在工作线程】校验里包含跨进程读内存（command_line）与
/// 注册表读写，极端情况下都可能慢；主线程（消息循环）绝不能被它们占住，
/// 否则托盘、主控台、会话事件全部失灵（2026-11-01 实测教训）。
///
/// 【为什么校验这么严】本进程是提权的，"代启动"本质上是一个提权原语——
/// 谁都能发请求就等于开了"以管理员权限跑任意程序"的后门。六道关卡，缺一不可：
/// 1. dwData 必须是代启动标识（在主线程的 wndproc 里已过滤）；
/// 2. 发送者 PID 不能是常驻实例自己——合法发送者一定是"另一个"进程（守门进程）。
///    不做这条的话，攻击者拿常驻实例自己窗口的句柄冒充发送者即可过关
///    （它的进程路径天然就是 hellogate.exe）；
/// 3. 请求体里的 gate_pid 必须与实际发送者 PID 一致（防拼接伪造）；
/// 4. 发送者进程路径必须是 hellogate.exe 自身；
/// 5. 守门进程的**真实命令行**必须包含本次目标路径（把请求绑定到"拦截了
///    这次启动的那个守门进程"，防借壳代启动；命令行由系统经 IFEO 拼出，
///    跨进程读取，无法伪造）；
/// 6. 目标必须在当前保护名单内且 verified=true（名单外程序一律不代启动——
///    否则"自己运行 hellogate.exe --gate 任意exe"就能免 UAC 提权跑任意程序）。
///
/// 【校验不过怎么办】直接 return——不设置完成事件，守门进程会在超时后
/// 提示"未能受理本次启动请求"（fail-closed：宁可多锁、不可漏放）。
fn handle_relay_worker(sender_addr: isize, req: RelayReq) {
    // 2~4. 发送者身份三重校验（wParam 反查 PID → 排除自身 → 对账 gate_pid → 对路径）。
    let sender = HWND(sender_addr as *mut _);
    let Some((sender_pid, sender_path, _)) = process::identify(sender) else {
        logln!("[relay] 无法解析发送者进程，拒绝请求");
        return;
    };
    if sender_pid == unsafe { GetCurrentProcessId() } {
        logln!("[relay] 发送者是常驻实例自身（冒充特征），拒绝请求");
        return;
    }
    if req.gate_pid != sender_pid {
        logln!(
            "[relay] 请求声称的守门 PID({}) 与实际发送者({})不符，拒绝",
            req.gate_pid, sender_pid
        );
        return;
    }
    let me = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if sender_path.to_lowercase() != me {
        logln!("[relay] 发送者不是 HelloGate 守门进程，拒绝: {sender_path}");
        return;
    }

    // 5a. 目标与请求名一致性：target 的文件名必须等于请求里的 name
    //     （防止 name 填名单内程序、target 却指向别处）。
    if process::file_name_lower(&req.target) != req.name {
        logln!("[relay] 目标文件名与请求名不符，拒绝: {}", req.target);
        return;
    }

    // 5b. 命令行绑定：守门进程的真实命令行必须包含本次目标路径。
    //
    // 守门进程的命令行是系统经 IFEO 拼出的 `"<hellogate>" --gate "<目标>" ...`，
    // 读取方式见 process::command_line（跨进程读 PEB）。读不到（进程恰好退出、
    // 非 x64 等罕见情况）一律按校验不过处理——宁可多拦一次，不可漏放一次。
    let Some(cmdline) = process::command_line(sender_pid) else {
        logln!("[relay] 无法读取守门进程命令行，拒绝请求（fail-closed）");
        return;
    };
    let cmd_lower = cmdline.to_lowercase();
    if !cmd_lower.contains(ifeo::GATE_FLAG) || !cmd_lower.contains(&req.target.to_lowercase()) {
        logln!("[relay] 守门进程命令行与请求目标不匹配，拒绝: {cmdline}");
        return;
    }

    // 6. 白名单约束：只代启动当前保护名单内的程序，且必须 verified=true。
    //    （以常驻实例内存里的配置为准，比守门进程读的磁盘快照更新。）
    let protected = RUNTIME
        .lock()
        .unwrap()
        .as_ref()
        .map(|rt| rt.cfg.is_protected(&req.name))
        .unwrap_or(false);

    // ★ 清理残留模式（见 gater.rs 的 release_stale）。
    //
    // 【为什么单独一条分支】它只要求"删掉这个程序的失效登记"，**不要求启动**。
    // 启动动作由普通权限的守门进程自己完成，常驻实例（管理员）不碰任何可执行
    // 文件，所以不存在"借代启动提权"的风险——这正是旧版被否掉的原因。
    //
    // 【为什么反过来也要限制】只允许清理**名单外**的登记。否则任何程序都能
    // 发一个请求，把受保护应用的登记删掉，等于给了外界一个廉价的"关锁"开关。
    if req.mode == gater::MODE_CLEANUP {
        if protected {
            logln!("[relay] 清理请求指向仍在保护名单的程序，拒绝: {}", req.name);
            return;
        }
        ifeo::remove_for_app(&req.name);
        logln!("[relay] 已清理失效启动拦截登记: {}", req.name);
        // 回执：守门进程收到事件后才自己启动程序，避免"登记还没删就启动"
        // 导致再次被 IFEO 重定向（死循环）。
        signal_relay(&req.nonce);
        return;
    }

    if !protected {
        // 名单外程序一律不代启动（fail-closed：宁可多锁、不可漏放）。
        logln!("[relay] 「{}」不在当前保护名单，拒绝代启动", req.name);
        return; // 不触发事件 → 守门进程超时提示
    }
    if !req.verified {
        logln!("[relay] 「{}」在保护名单内但请求未经用户验证，拒绝", req.name);
        return; // 不触发事件 → 守门进程超时提示，绝不放行
    }

    logln!("[relay] 受理代启动: {} (已通过全部校验)", req.target);

    // 本函数本身就运行在工作线程，直接做实事（注册表删/写 + 进程启动
    // 都是可能慢的操作，反正不再占主线程）。
    let RelayReq { nonce, name, target, args, .. } = req;
    // 临时摘掉该应用的 Debugger 重定向，否则启动会被再次拦截（死循环）。
    ifeo::remove_for_app(&name);
    let spawned = std::process::Command::new(&target).args(&args).spawn();
    // 恢复登记（无论启动成败都要恢复——宁可多锁，不可漏锁；
    // 万一恢复失败，常驻实例下次启动时 sync 会重建）。
    if let Err(e) = ifeo::restore_for_app(&name) {
        logln!("[relay] 恢复启动拦截登记失败: {e}");
    }
    match &spawned {
        Ok(child) => {
            logln!("[relay] 代启动成功: {target} (PID={})", child.id());
            // 只有确认启动成功才通知守门进程；失败就不设事件，
            // 让守门进程走超时提示——用户至少明确知道"程序没打开"。
            signal_relay(&nonce);
        }
        Err(e) => logln!("[relay] 代启动失败: {target}: {e}"),
    }
}

/// 按 nonce 打开「完成事件」并 SetEvent，告诉守门进程"事情已经办完"。
///
/// 两条路径共用：代启动成功（gater 收到后退出）、清理残留完成（gater 收到后
/// 自己启动目标程序）。事件名由守门进程生成、两进程约定，找不到就静默跳过——
/// 说明守门进程已经退出或超时，此时再报错也没有意义。
fn signal_relay(nonce: &str) {
    let en = wide(nonce);
    unsafe {
        if let Ok(ev) = OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(en.as_ptr())) {
            let _ = SetEvent(ev);
            let _ = CloseHandle(ev);
        }
    }
}

/// 处理托盘菜单命令。
fn handle_tray_command(hwnd: HWND, cmd: usize) {
    match cmd {
        tray::CMD_OPEN_CONSOLE => open_console(),
        // 组件画廊：非模态窗口，open() 建完窗就返回，消息由托盘主循环派发
        // （unsafe 只覆盖建窗这一段，照 CMD_EXIT 分支的既有写法）。
        tray::CMD_GALLERY => unsafe { gallery::open() },
        tray::CMD_RELOCK_ALL => {
            let mut guard = RUNTIME.lock().unwrap();
            if let Some(rt) = guard.as_mut() {
                rt.sm.relock_all();
                logln!("[tray] 已重新锁定全部应用");
            }
        }
        tray::CMD_EXIT => {
            unsafe {
                tray::remove_icon(hwnd);
                let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(hwnd);
            }
            // ★ 直接结束进程，而不是只靠 PostQuitMessage 退出消息循环。
            //
            // 【为什么必须这么做】
            // 如果此刻主控台正开着，本条托盘消息其实是在主控台的**嵌套消息循环**里
            // 被派发的（主线程卡在 console::open 的内层 GetMessage 上）。
            // DestroyWindow 触发的 PostQuitMessage(0) 只会让**最内层**那个循环退出，
            // WM_QUIT 被内层消费后，外层主循环再也收不到退出信号，于是继续阻塞——
            // 表现就是：所有窗口都关了，进程却赖在后台不死（僵尸进程）。
            //
            // 退出路径没有需要异步落盘的状态（配置在点「保存」时已同步写盘），
            // 因此这里用 process::exit 立即、确定地终止整个进程最稳妥。
            std::process::exit(0);
        }
        _ => {}
    }
}

/// 「主控台正在打开/已打开」的原子旗标。
///
/// 【为什么需要】open_console 现在在独立线程里跑，"反复双击 exe / 连点托盘"
/// 会同时拉起多个线程。console::open 内部虽有用 FindWindow 的复用检查，
/// 但两个线程同时通过检查就会叠出两个窗口。用 CAS 抢旗，抢不到的直接退出。
static CONSOLE_OPENING: AtomicBool = AtomicBool::new(false);

/// 是否已经因为「无凭据」（Hello 不可用 + 没设密码）自动弹过主控台。
///
/// 【为什么需要】遮罩解锁每次用户切到受保护应用都会触发一次认证。
/// 如果每次无凭据都去弹主控台，用户每切一次窗口就被骚扰一次。
/// 只在本次进程生命期内提示一次；用户设置好密码后自然不会再走这个分支。
static NO_CRED_HINTED: AtomicBool = AtomicBool::new(false);

/// 打开主控台（入口，立即返回）。
///
/// ★★ 认证与窗口整体放到**独立线程**执行。
///
/// 【为什么必须这样（2026-11-01 实测踩坑，勿回退）】
/// 旧版在主线程的窗口过程里**同步**执行：设置自锁弹 Windows Hello——
/// 提权进程里的 Hello 弹窗一旦没能显示出来，`join()` 永久阻塞，主线程就死了：
/// 主控台打不开、托盘点不动、代启动请求全部超时、连"退出"都关不掉，
/// 用户被整个软件锁在外面（Word 陷入"验证→超时→再验证"的循环）。
/// 挪到独立线程后，无论认证弹窗卡多久，主线程的消息循环永远健在：
/// 托盘可点、relay 可响应、用户随时能从托盘退出。认证线程自己卡死只影响那一次。
fn open_console() {
    // CAS 抢旗：已有打开流程在进行就不再重复触发（防多线程叠窗口）。
    if CONSOLE_OPENING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        logln!("[console] 已有主控台打开流程在进行，忽略本次请求");
        return;
    }
    thread::spawn(|| {
        // RAII：本线程无论从哪个分支退出，都把旗标清掉，允许下次打开。
        struct FlagGuard;
        impl Drop for FlagGuard {
            fn drop(&mut self) {
                CONSOLE_OPENING.store(false, Ordering::SeqCst);
            }
        }
        let _guard = FlagGuard;
        open_console_impl();
    });
}

/// 主控台的实际打开流程（在独立线程运行，阻塞到窗口关闭为止）。
fn open_console_impl() {
    if !authorize_settings() {
        logln!("[console] 设置自锁：认证未通过，主控台未打开");
        return;
    }
    // 取出当前配置的副本（不要持锁太久，避免死锁）
    let current = {
        let guard = RUNTIME.lock().unwrap();
        match guard.as_ref() {
            Some(rt) => rt.cfg.clone(),
            None => return,
        }
    };

    // 打开主控台（内部有自己的模态消息循环）
    let new_cfg = console::open(&current);

    // 保存成功 → 先给主线程的隐藏消息窗口投一条 Toast 消息。
    // 【为什么在这里投、而不是主控台自己弹】主控台窗口一关、本线程随即结束，
    // 它建的 Toast 窗口会随线程一起被系统销毁，动画根本播不完；
    // 主线程循环常驻，交给它弹才活得到 3 秒后自毁（见 WM_HELLOGATE_TOAST 注释）。
    if new_cfg.is_some() {
        if let Some(host) = find_existing_instance() {
            unsafe {
                let _ = PostMessageW(Some(host), WM_HELLOGATE_TOAST, WPARAM(0), LPARAM(0));
            }
        }
    }

    if let Some(new_cfg) = new_cfg {
        logln!("[console] 配置已更新，重新载入运行时");
        // 更新运行时配置 + 状态机策略
        let mut guard = RUNTIME.lock().unwrap();
        if let Some(rt) = guard.as_mut() {
            rt.sm.reconfigure(new_cfg.grace_mode, new_cfg.grace_seconds);
            rt.cfg = new_cfg;
        }
    }
}

/// ★ 设置自锁的认证关卡（用户一针见血：主控台不设防，密码栏就是摆设）。
///
/// 【为什么必须】主控台是"改锁的地方"——不认证就能进，被锁的人点开托盘
/// 就能把受保护应用移出列表、清空主密码，整套锁瞬间形同虚设。
///
/// 【规则】与解锁同一套路由（auth::route_verification），只是保护对象换成设置界面：
/// - 配置为「自设密码」→ 密码框（含「忘记密码」恢复入口）；
/// - 配置为「系统验证」→ Hello，不可用则回退主密码框；
/// - 仍在使用**出厂默认密码** → 直接放行，见下方说明；
/// - 既没设主密码、Hello 也不可用 → 改要 Windows 账户密码。
fn authorize_settings() -> bool {
    // 凭据快照：认证方式 + 密码存储值（读完立即释放锁，认证弹窗期间不能持锁）
    let (method, secrets) = {
        let guard = RUNTIME.lock().unwrap();
        match guard.as_ref() {
            Some(rt) => (
                rt.cfg.auth_method.clone(),
                password::AuthSecrets {
                    pwd_hash: rt.cfg.master_password_hash.clone(),
                    pwd_salt: rt.cfg.master_password_salt.clone(),
                    // 是否仍为出厂默认密码（决定密码框副标题与放行策略）
                    pwd_is_default: crate::config::is_default_password(
                        &rt.cfg.master_password_hash,
                        &rt.cfg.master_password_salt,
                    ),
                },
            ),
            None => return true,
        }
    };

    // ★ 出厂默认密码状态：直接放行，不做认证。
    //
    // 【为什么放行】默认密码 123456 是公开写在代码/文档里的，弹窗验证它
    // 没有任何安全收益，反而会把"第一次打开主控台"变成一道过不去的墙
    // （用户此时根本不知道默认密码是哪来的）。真正的安全从用户把密码
    // 改成自己的那一刻开始；在那之前，主控台顶部会一直显示醒目提示。
    if secrets.pwd_is_default {
        logln!("[console] 当前为出厂默认密码 → 直接放行打开主控台（界面会提示尽快修改）");
        return true;
    }

    // ★ 诊断日志：设置自锁的认证历史上在提权进程里出现过"Hello 弹不出来、
    //   join 永久阻塞"的事故（2026-11-01）。打点每一步的结果，出问题时日志
    //   能直接看到卡在哪个环节；即使再卡死，也只是认证线程卡死，
    //   主线程健在（见 open_console 的线程化说明），托盘仍可退出。
    //
    // ★ 未设置主密码时，**不直接放行**（那等于谁双击都能关掉锁），
    //   也不再傻等 Hello 超时（这台机器可能根本没有 Hello，一等就是 30 秒）。
    //   做法：先用 3 秒快速探测 Hello；能用就正常验 Hello，不能用就
    //   改要 Windows 账户密码——无论如何，进门的人必须是这台电脑的主人。
    if !secrets.has_password() {
        let route = if auth::hello_available_quick() {
            logln!("[console] 未设置主密码，探测到 Hello 可用 → 验 Hello");
            auth::route_verification(&method, "HelloGate 设置", &secrets)
        } else {
            logln!("[console] 未设置主密码，且 Hello 不可用 → 改用 Windows 账户密码");
            auth::AuthRoute::NoCredentials
        };
        if route == auth::AuthRoute::Passed {
            logln!("[console] 设置自锁：认证通过，打开主控台");
            return true;
        }
        // Hello 不可用 / 用户取消 / 验不过：统一用 Windows 账户密码兜底。
        // 这条路径同时也是"没有 Hello 的机器"唯一能进设置的方式。
        let ok = password::prompt_account_password(auth::verify_account_password)
            == password::PromptOutcome::Unlocked;
        logln!(
            "[console] Windows 账户密码验证{}",
            if ok { "通过，打开主控台" } else { "未通过，拒绝打开" }
        );
        return ok;
    }

    logln!("[console] 设置自锁：开始认证（按配置的认证方式路由）");
    let route = auth::route_verification(&method, "HelloGate 设置", &secrets);
    match route {
        auth::AuthRoute::Passed => {
            logln!("[console] 设置自锁：认证通过，打开主控台");
            true
        }
        auth::AuthRoute::NoCredentials => {
            // 走到这里说明前面已经给过 Windows 账户密码的机会，仍然没通过：
            // 严格拒绝（宁可让主人多重试一次，也不让陌生人关掉锁）。
            logln!("[console] 无可用凭据且账户密码未通过，拒绝打开主控台");
            false
        }
        other => {
            // 取消 / 次数耗尽 / 出错：一律不放行（宁可多拦一次，不可漏放一次）。
            logln!("[console] 设置自锁认证未通过（{other:?}）");
            false
        }
    }
}

// ============================================================================
// 会话事件工作线程
// ============================================================================

/// 启动会话事件消费线程：收到锁屏/睡眠事件时重锁所有应用。
fn start_session_worker(rx: Receiver<session::SessionEvent>) {
    thread::spawn(move || {
        for ev in rx {
            let mut guard = RUNTIME.lock().unwrap();
            let Some(rt) = guard.as_mut() else { continue };
            // 只有配置里开启「锁屏/睡眠后重锁」时才处理
            if !rt.cfg.relock_on_session_change {
                continue;
            }
            match ev {
                session::SessionEvent::Locked | session::SessionEvent::Suspend => {
                    rt.sm.relock_all();
                    // 若遮罩正显示，也一并隐藏（用户已离开，回来应重新验证）。
                    // 本线程不是遮罩属主线程，只能举旗请事件线程代劳。
                    overlay::request_hide();
                    logln!("[session] 检测到离开事件（{ev:?}），已重锁全部应用");
                }
                session::SessionEvent::Unlocked | session::SessionEvent::Resume => {
                    // 回到电脑前不自动解锁，保持锁定状态，等用户主动验证。
                    logln!("[session] 用户已返回（{ev:?}），保持锁定");
                }
            }
        }
    });
}

// ============================================================================
// Hook 线程：监听前台窗口切换
// ============================================================================

/// 启动 Hook 线程。
fn start_hook_thread() {
    let (tx, rx) = mpsc::channel::<hook::RawEvent>();

    // 安装钩子（必须在将要运行消息循环的线程里安装）
    thread::spawn(move || {
        let _guard = match hook::install(tx) {
            Ok(g) => g,
            Err(e) => {
                logln!("[hook] 安装事件钩子失败: {e}");
                return;
            }
        };
        logln!("[hook] SetWinEventHook 已安装（EVENT_SYSTEM_FOREGROUND）");

        // 事件消费：必须在同一线程？不——回调经消息队列派发到本线程，
        // 但我们把事件 send 到 channel，由另一个线程处理，避免拖慢消息循环。
        // 这里本线程直接跑消息循环即可。
        hook::run_message_loop();
    });

    // 事件处理线程。
    //
    // ★ 这条线程是遮罩窗口的**属主线程**（show_overlay 在这里被调用），
    // 所以它必须自己泵消息：窗口的 WM_PAINT（画内容）、背景擦除等消息
    // 都排在它的队列里，没有消息循环就永远没人处理——遮罩会"存在但一个像素都不画"，
    // 用户看到的仍是目标窗口内容。这就是之前"遮罩没挡住"的真正根因。
    thread::spawn(move || {
        use std::sync::mpsc::RecvTimeoutError;
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
        };

        loop {
            // 1. 处理本线程窗口消息（遮罩绘制全靠这一步）
            unsafe {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }

            // 2. 其他线程举旗要隐藏遮罩，由属主线程在这里真正执行
            overlay::pump_hide_request();

            // 2.5 遮罩跟随：目标窗口若被拖动/缩放，把遮罩同步挪过去。
            //     【为什么必须做】遮罩只在弹出的那一刻贴合目标矩形；此后用户
            //     拖动标题栏或按 Win+方向键，窗口挪走了遮罩却留在原地，
            //     内容就暴露了。每帧对比一次矩形（开销可忽略），变了就挪。
            overlay::track_target_move();

            // 3. 最多等 16ms（约一帧）取下一个窗口事件：
            //    兼顾响应速度（事件来了几乎立刻处理）与 CPU 占用（空转频率低）。
            match rx.recv_timeout(std::time::Duration::from_millis(16)) {
                Ok(ev) => handle_hook_event(ev),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
}

/// 处理单个窗口事件：判断是否受保护 → 是否需要弹遮罩。
fn handle_hook_event(ev: hook::RawEvent) {
    // 只处理「前台切换」事件（窗口创建事件暂不处理，避免过于频繁）。
    if ev.kind != hook::WinEventKind::Foreground {
        return;
    }

    let hwnd = ev.to_hwnd();

    // 由窗口句柄识别进程
    let Some((pid, path, name)) = process::identify(hwnd) else {
        return;
    };

    // 读取配置（加锁，读完即释放）
    let (is_protected, display_name) = {
        let guard = RUNTIME.lock().unwrap();
        let Some(rt) = guard.as_ref() else { return };
        (rt.cfg.is_protected(&name), rt.cfg.display_name_of(&name))
    };

    // ★ 修复「弹密码框时遮罩被隐藏、内容暴露」的 bug（用户截图所见）。
    //
    // 验证弹窗（我们自己的密码框 / 系统 Hello 对话框）不是受保护应用的窗口，
    // 它一出现就抢走前台 → 系统发来"切到非受保护应用"事件。
    // 若照常往下处理，遮罩会被当成"用户已离开"而隐藏，计算器内容就在输密码时露出来了。
    // 正确做法：遮罩正盖着的应用若"验证进行中"，这次前台切换视作"没离开"——
    // 不隐藏遮罩、不清宽限期，直接返回。
    if !is_protected {
        let auth_busy = overlay::current_target()
            .map(|t| *AUTH_GUARD.lock().unwrap() == Some(t))
            .unwrap_or(false);
        if auth_busy {
            return;
        }
    }

    // ★ 统一处理「离开上一个受保护应用」的一次性宽限期失效。
    //
    // 【这里修复了一个安全漏洞】
    // 旧逻辑只在「切到非受保护应用」时才调用 on_leave 让上一个应用的一次性宽限期失效。
    // 于是存在这样一条绕过路径：用户从已解锁的受保护应用 A，**直接**切到另一个受保护
    // 应用 B（中间没经过任何普通窗口）——A 的一次性宽限期记录不会被清除。等用户再切回
    // A 时，A 仍被判为「在宽限期内」，于是直接放行、还把盖在 B 上的遮罩一并隐藏，
    // 结果 B 的遮罩消失、内容暴露。
    //
    // 正确做法：只要前台应用发生变化，就先让「上一个停留的受保护应用」的一次性宽限期失效，
    // 不管新来的是不是受保护应用。
    let prev_protected = {
        let mut guard = RUNTIME.lock().unwrap();
        match guard.as_mut() {
            Some(rt) => {
                let prev = rt.last_protected.take();
                // 顺带清理过期记录，避免时间型模式下 map 无限增长
                rt.sm.purge_expired();
                prev
            }
            None => return,
        }
    };
    if let Some(prev) = &prev_protected {
        // 切走的就是同一个应用（重复前台事件）时不能失效，否则宽限期形同虚设。
        if prev != &name {
            let mut guard = RUNTIME.lock().unwrap();
            if let Some(rt) = guard.as_mut() {
                rt.sm.on_leave(prev);
            }
        }
    }

    if !is_protected {
        // 不是受保护应用：用户已离开所有受保护应用，若遮罩还在，一并隐藏。
        // （last_protected 上面已 take 清空）
        overlay::request_hide();
        return;
    }

    // 走到这里：当前前台是受保护应用，记录它供「将来切走」时使用。
    {
        let mut guard = RUNTIME.lock().unwrap();
        if let Some(rt) = guard.as_mut() {
            rt.last_protected = Some(name.clone());
        }
    }

    // 已在宽限期内：无需弹遮罩（但确保遮罩没残留）。
    let is_unlocked = {
        let guard = RUNTIME.lock().unwrap();
        guard
            .as_ref()
            .map(|rt| rt.sm.is_unlocked(&name, pid))
            .unwrap_or(false)
    };
    if is_unlocked {
        overlay::request_hide();
        return;
    }

    // 遮罩已经在显示，且盖的**就是当前这个应用**：说明验证流程已在进行，
    // 不重复弹、也不重复发起 Windows Hello（否则会叠加多个验证弹窗）。
    if overlay::is_showing() && overlay::current_target().as_deref() == Some(name.as_str()) {
        return;
    }

    // ★ 堵住「验证弹窗叠加」竞态（日志实测过：同一应用连弹了 3 个 Hello）。
    // 从"决定弹遮罩"到"遮罩真的显示出来"之间有个短窗口，同一应用的前台事件
    // 风暴会挤穿上面 is_showing() 的检查。所以先占下 AUTH_GUARD 的"坑"再弹遮罩，
    // 后到的重复事件在坑位检查这里就被挡掉。
    {
        let mut g = AUTH_GUARD.lock().unwrap();
        if g.as_deref() == Some(name.as_str()) {
            return; // 这个应用的验证已在进行（遮罩可能都还没画出来）
        }
        *g = Some(name.clone());
    }

    logln!("[hook] 受保护应用切到前台: {name} (PID={pid})");
    logln!("       路径: {path}");

    // 弹出遮罩。若遮罩已存在但盖着别的应用（A→B 直接切换），这里会把同一个
    // 遮罩窗口移到 B 的矩形上并更新「盖着谁」，从而堵住 B 不被遮挡的漏洞。
    if let Err(e) = overlay::show_overlay(hwnd, &name) {
        // 遮罩没弹成，把刚占的坑撤掉——否则这个应用会被永远当成"验证中"。
        let mut g = AUTH_GUARD.lock().unwrap();
        if g.as_deref() == Some(name.as_str()) {
            *g = None;
        }
        logln!("[overlay] 显示遮罩失败: {e}");
        return;
    }

    // 在独立线程执行验证，避免阻塞事件处理线程
    let name_for_auth = name.clone();
    thread::spawn(move || {
        // RAII 兜底：本线程无论从哪个分支退出（含提前 return），都清掉验证标记。
        // 只有标记仍属于"我"时才清——若期间用户已切到另一个应用 B 并开始了新验证，
        // 不能把 B 的标记误清掉。
        struct AuthGuard(String);
        impl Drop for AuthGuard {
            fn drop(&mut self) {
                let mut g = AUTH_GUARD.lock().unwrap();
                if g.as_deref() == Some(self.0.as_str()) {
                    *g = None;
                }
            }
        }
        let _auth_guard = AuthGuard(name_for_auth.clone());
        // 不再 sleep 等待：验证弹窗越早出现，用户能瞄到内容的窗口越短。
        // （旧版这里睡 120ms 是"等遮罩先画出来"，实测没有必要，纯拖慢体验。）

        // 凭据快照：认证方式 + 密码/恢复码存储值。读完立即释放锁——
        // 验证弹窗是长交互（用户可能发呆几分钟），绝不能持锁弹窗。
        let (method, secrets) = {
            let guard = RUNTIME.lock().unwrap();
            match guard.as_ref() {
                Some(rt) => (
                    rt.cfg.auth_method.clone(),
                    password::AuthSecrets {
                        pwd_hash: rt.cfg.master_password_hash.clone(),
                        pwd_salt: rt.cfg.master_password_salt.clone(),
                        // 是否仍为出厂默认密码（决定密码框副标题与放行策略）
                        pwd_is_default: crate::config::is_default_password(
                            &rt.cfg.master_password_hash,
                            &rt.cfg.master_password_salt,
                        ),
                    },
                ),
                None => return,
            }
        };

        // 按配置的认证方式路由验证：系统验证（Hello）/ 自设密码，含忘记密码恢复。
        // （统一逻辑见 auth::route_verification，三个入口共用同一套路由。）
        let route = auth::route_verification(&method, &display_name, &secrets);
        match route {
            auth::AuthRoute::Passed => {
                grant_unlock(&name_for_auth, pid);
            }
            auth::AuthRoute::Failed => {
                logln!("[auth] 验证未通过，遮罩保持显示");
            }
            auth::AuthRoute::NoCredentials => {
                // Hello 不可用 + 未设密码：**遮罩本身绝不放行**（宁可锁住，不可漏放）。
                //
                // 但这里必须给用户一条出路：旧版只写一行日志就结束，用户对着
                // 一整块实色遮罩完全不知道该干什么（实测就是这样卡死的）。
                // 现在主动把主控台调出来——主控台在「无凭据」状态是放行的
                // （首次配置场景），用户可以在里面设置密码。
                //
                // 防抖：只在第一次遇到时自动弹，避免每切一次窗口就弹一次。
                logln!("[auth] 未设置主密码 → 遮罩保持显示，引导用户去主控台设置");
                if !NO_CRED_HINTED.swap(true, Ordering::SeqCst) {
                    logln!("[auth] 首次遇到无凭据状态，自动打开主控台引导设置密码");
                    open_console();
                }
            }
        }
    });
}

/// 把某个受保护应用标记为已解锁（写入状态机），并在遮罩仍盖着它时隐藏遮罩。
///
/// 【为什么遮罩要再确认一次】验证弹窗期间用户可能已切到另一个受保护应用 B
/// （遮罩已移到 B 上）；B 还没验证过，绝不能因为 A 验证通过就把 B 的遮罩关掉。
/// （这段逻辑旧版在 Verified 分支和主密码分支各写了一份，现抽成公共函数。）
fn grant_unlock(name: &str, pid: u32) {
    logln!("[auth] 验证通过，解锁「{name}」");
    // 标记为已解锁（写入状态机），并绑定这个进程实例的 PID
    {
        let mut guard = RUNTIME.lock().unwrap();
        if let Some(rt) = guard.as_mut() {
            rt.sm.mark_unlocked(name, pid);
        }
    }
    if overlay::current_target().as_deref() == Some(name) {
        overlay::request_hide();
    }
}

// ============================================================================
// 工具函数
// ============================================================================

/// 运行主线程消息循环（阻塞）。
fn run_message_loop() {
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// 把 Rust 字符串转成以 NUL 结尾的 UTF-16 宽字符串。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// 保留 w! 宏的显式引用，避免未使用导入告警（未来动态注册表/资源路径会用到）。
#[allow(dead_code)]
fn _anchor() -> PCWSTR {
    w!("HelloGate")
}
