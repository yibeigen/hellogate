//! 遮罩层模块（Phase 3 P0-2）
//!
//! 职责：在受保护窗口**正上方**创建一层置顶、无焦点、贴合矩形的**不透明**遮罩，
//! 用于挡住窗口内容并展示解锁提示，**不杀死目标进程**。
//!
//! 对应 DESIGN_AND_ROADMAP.md §2.3。关键实现点：
//! - 窗口样式 `WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW`：
//!   置顶、不抢焦点、不进 Alt+Tab。
//! - **遮罩必须不透明**（实色背景刷 + 自绘提示）：
//!   旧版曾尝试用未文档化 API `SetWindowCompositionAttribute` 做亚克力毛玻璃，
//!   但在 Win11 上该调用静默失败，加上 `WS_EX_LAYERED` 未配套设置透明度
//!   会导致窗口**完全透明**（和主控台踩过的同一个坑），结果遮罩形同虚设、
//!   文档内容一览无余。隐私工具的第一要求是"真的挡住"，
//!   所以彻底放弃花哨模糊，改用 100% 可靠的实色遮罩。
//! - 目标窗口刚被切到前台时会有约 100~200ms 的绘制空窗（事件钩子的固有延迟），
//!   这是非侵入式方案的物理极限，商用锁软件同样如此；我们把验证唤起提速到即时。

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowRect, IsWindow,
    LoadCursorW, RegisterClassW, SetWindowPos, ShowWindow, CS_HREDRAW, CS_VREDRAW, HWND_TOPMOST,
    IDC_ARROW, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, SW_SHOWNOACTIVATE, WNDCLASSW, WM_DESTROY,
    WM_PAINT, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

// ============================================================================
// 遮罩窗口的创建与管理
// ============================================================================

/// 遮罩窗口的类名（注册窗口类时使用）。
const OVERLAY_CLASS: &str = "HelloGateOverlayClass";

/// 当前遮罩窗口句柄。0 表示无遮罩。
/// 用原子类型存储，方便工作线程与窗口过程之间共享。
static OVERLAY_HWND: AtomicIsize = AtomicIsize::new(0);

/// 遮罩当前盖的是**哪个应用**（小写 exe 文件名）。
///
/// 【为什么要记这个】
/// 遮罩是全局唯一的一个窗口，它可以从「盖着应用 A」直接移动到「盖着应用 B」。
/// 而 Windows Hello 验证线程是异步的：A 的验证弹窗还没关掉时，用户可能已经
/// 切到了 B，遮罩也随之移到了 B 上面。这时 A 的验证若通过，**不能**把盖在
/// B 上的遮罩关掉（B 还没验证过！）。靠这个记录就能判断：
/// 「当前遮罩盖的还是不是我验证的那个应用」——是才关，不是就不动。
static OVERLAY_TARGET: Mutex<Option<String>> = Mutex::new(None);

/// 遮罩当前贴合的**目标窗口句柄**（0 = 无）。
///
/// 【为什么要记】目标窗口在遮罩显示期间可能被拖动或调整大小（拖标题栏、
/// Win+方向键贴边、改分辨率都会）。遮罩若停在原矩形上，窗口挪走后内容
/// 就露出来了。事件处理线程每个循环周期调用 [`track_target_move`]，
/// 发现目标矩形变了就把遮罩挪过去，保证"遮罩永远压在内容上"。
static OVERLAY_TARGET_HWND: AtomicIsize = AtomicIsize::new(0);

/// 注册遮罩窗口类（进程内只需注册一次）。
fn register_overlay_class() -> Result<(), String> {
    // 注意：宽字符串必须先用变量持有，再取指针。
    // 如果写成 `PCWSTR(wide(..).as_ptr())`，临时 Vec 会在语句结束时被释放，
    // 指针立刻悬垂（use-after-free），这是非常隐蔽的内存 bug。
    let class_name = wide(OVERLAY_CLASS);

    unsafe {
        let hinstance = GetModuleHandleW(None).map_err(|e| e.to_string())?;

        // 背景刷必须是**实色不透明**画刷：系统每次擦除背景都会用它铺满整个遮罩，
        // 这是"挡住内容"的最后保险——就算自绘逻辑出任何岔子，窗口也绝不透明。
        // 颜色 0x00F0F0F0 = RGB(240,240,240)，浅灰白，和主控台风格统一。
        use windows::Win32::Graphics::Gdi::{CreateSolidBrush, HBRUSH};
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(overlay_wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH(CreateSolidBrush(COLORREF(0x00F0F0F0)).0),
            // 品牌图标（exe 内嵌 ID=1 资源）：遮罩本身不显示图标，
            // 但窗口类带上它，任何系统场景取窗口图标时都拿到品牌图形。
            // WNDCLASSW 无 hIconSm，小图标自动回退 hIcon。
            hIcon: crate::ui::load_app_icon(),
            ..Default::default()
        };

        // 重复注册会失败（返回 0），但这不是错误，忽略即可。
        let _ = RegisterClassW(&wc);
        Ok(())
    }
}

/// 遮罩窗口的窗口过程。
///
/// 目前只处理两类消息：
/// - `WM_PAINT`：绘制解锁卡片（PoC 阶段先画一个简单提示，Phase 4 换成真正的 UI）。
/// - `WM_DESTROY`：清理全局句柄。
unsafe extern "system" fn overlay_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            // PoC 阶段：用最简单的 GDI 文本提示。
            // 真正的 Bento Grid 解锁卡片将在 Phase 4 用 WebView 或自绘实现。
            draw_placeholder_text(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            OVERLAY_HWND.store(0, Ordering::SeqCst);
            OVERLAY_TARGET_HWND.store(0, Ordering::SeqCst);
            // 遮罩没了，"盖着谁"自然也要清空，否则下次判断会拿到过期名字。
            *OVERLAY_TARGET.lock().unwrap() = None;
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 在遮罩中央绘制占位提示文字（PoC 用，非最终 UI）。
///
/// 【为什么要按 DPI 缩放】
/// 这里的字号、卡片大小、文字偏移原本全是写死的物理像素。在 100% 缩放的屏上
/// 没问题，但在 150%/200% 的高分屏上，遮罩窗口本身是真实物理像素（很大），
/// 而文字还按 96 DPI 的小尺寸画，结果就是「一张大灰幕布上飘着几个小字」，
/// 而且居中偏移算错、字会明显偏左偏上。所以所有尺寸统一乘以 dpi/96。
unsafe fn draw_placeholder_text(hwnd: HWND) {
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, EndPaint, GetStockObject as GetStock,
        Rectangle, SelectObject, SetBkMode, SetTextColor, TextOutW, CLEARTYPE_QUALITY,
        CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, FF_DONTCARE, FW_SEMIBOLD, NULL_PEN,
        OUT_DEFAULT_PRECIS, PAINTSTRUCT, TRANSPARENT,
    };
    use windows::Win32::UI::HiDpi::GetDpiForWindow;

    // 当前窗口所在显示器的 DPI（96 = 100%）。取不到就按 96 兜底。
    let dpi = GetDpiForWindow(hwnd).max(96) as f32;
    let s = dpi / 96.0;
    // 把「96 DPI 逻辑像素」换算成真实像素的小工具。
    let px = |logical: f32| (logical * s).round() as i32;

    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let mut rect = RECT::default();
    let _ = GetClientRect(hwnd, &mut rect);
    let cx = (rect.right - rect.left) / 2;
    let cy = (rect.bottom - rect.top) / 2;

    // 深灰文字，透明背景（让亚克力透出来）
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(0x00303030));

    // 字体名同样要先用变量持有宽字符串，避免临时值悬垂。
    let face = wide("Microsoft YaHei UI");
    // CreateFontW 参数顺序（windows crate 签名）：
    //   高度, 宽度, 倾斜角, 方向角, 字重, 斜体, 下划线, 删除线,
    //   字符集, 输出精度, 裁剪精度, 输出质量, 字族, 字体名
    // FW_SEMIBOLD 是 FONT_WEIGHT 新类型，`.0` 得到底层 i32（600）。
    let font = CreateFontW(
        -px(32.0), 0, 0, 0, FW_SEMIBOLD.0 as i32, 0, 0, 0,
        DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, CLEARTYPE_QUALITY,
        FF_DONTCARE.0.into(), PCWSTR(face.as_ptr()),
    );
    let old_font = SelectObject(hdc, font.into());

    // 画一条矩形作为「解锁卡片」的占位区域（Phase 4 换成真正的 Bento 卡片 UI）。
    // 这里用 NULL_PEN（不描边）+ 浅灰画刷填充。卡片尺寸同样按 DPI 缩放。
    let pen = GetStock(NULL_PEN);
    let old_pen = SelectObject(hdc, pen);
    let brush = CreateSolidBrush(COLORREF(0x00E8E8EC));
    let old_brush = SelectObject(hdc, brush.into());
    let _ = Rectangle(hdc, cx - px(320.0), cy - px(80.0), cx + px(320.0), cy + px(80.0));

    // 绘制提示文字。TextOutW 的字符串参数不需要 NUL 结尾，直接传 &[u16]。
    // 验证是自动唤起的，文案不再引导"点击"，如实告知用户正在发生什么。
    let title: Vec<u16> = "此应用已被 HelloGate 锁定".encode_utf16().collect();
    let subtitle: Vec<u16> =
        "请在弹出的 Windows Hello 窗口完成验证".encode_utf16().collect();
    let _ = TextOutW(hdc, cx - px(170.0), cy - px(50.0), &title);
    let _ = TextOutW(hdc, cx - px(210.0), cy + px(10.0), &subtitle);

    // 恢复 GDI 对象并释放，避免句柄泄漏。
    SelectObject(hdc, old_font);
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(font.into());
    let _ = DeleteObject(brush.into());

    let _ = EndPaint(hwnd, &ps);
}

/// 显示（或更新）遮罩，使其**精确贴合**目标窗口的屏幕矩形。
///
/// # 参数
/// - `target`：受保护的目标窗口句柄。
/// - `app_name`：目标应用的小写 exe 文件名。记下它，遮罩才能告诉别人
///   「我现在盖的是谁」——用于修复「A 切到 B 时 B 没被挡住」的漏洞（见 OVERLAY_TARGET）。
///
/// # 返回
/// - `Ok(())`：遮罩已显示。
/// - `Err(String)`：创建失败。
pub fn show_overlay(target: HWND, app_name: &str) -> Result<(), String> {
    // 0. 把句柄换算成「顶层根窗口」。
    //    前台事件给出的 hwnd 可能是应用内部的子窗口（如 Word 的文档编辑区），
    //    直接按子窗口矩形盖遮罩，会漏掉标题栏、功能区、侧边栏——文档内容照样可见。
    //    GA_ROOT 沿父子关系向上找到真正的顶层大窗口，盖它就等于盖住一切。
    let target = unsafe { windows::Win32::UI::WindowsAndMessaging::GetAncestor(target, windows::Win32::UI::WindowsAndMessaging::GA_ROOT) };

    // 1. 取目标窗口的屏幕坐标矩形
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(target, &mut rect).map_err(|e| e.to_string())?;
    }
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;

    // 2. 如果遮罩已存在，直接移动并重新显示即可（避免闪烁）。
    //    这正是「从应用 A 切到应用 B」的路径：同一个遮罩窗口，盖到 B 的矩形上，
    //    并把「当前盖着谁」更新为 B。
    let existing = OVERLAY_HWND.load(Ordering::SeqCst);
    if existing != 0 {
        let hwnd = HWND(existing as *mut _);
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                rect.left,
                rect.top,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
        // 记住"现在盖着哪个窗口"，供 track_target_move 跟随移动/缩放。
        // （target 在函数开头已被换成 GA_ROOT 的顶层窗口，句柄就是最终目标。）
        OVERLAY_TARGET_HWND.store(target.0 as isize, Ordering::SeqCst);
        *OVERLAY_TARGET.lock().unwrap() = Some(app_name.to_string());
        return Ok(());
    }

    // 3. 首次创建：注册窗口类
    register_overlay_class()?;

    let ex_style = WS_EX_TOPMOST      // 置顶
        | WS_EX_NOACTIVATE            // ★ 不抢焦点
        | WS_EX_TOOLWINDOW;           // ★ 不出现在 Alt+Tab / 任务栏
    // 注意：**不能**加 WS_EX_LAYERED——分层窗口必须配套调用 SetLayeredWindowAttributes
    // 才有不透明度，否则整个窗口完全透明（遮罩直接隐身，内容照样看见）。

    unsafe {
        let hinstance = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        // 同样先用变量持有宽字符串，再取指针，避免临时值悬垂。
        let class_name = wide(OVERLAY_CLASS);
        let window_title = wide("HelloGate Overlay");
        let hwnd = CreateWindowExW(
            ex_style,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(window_title.as_ptr()),
            WS_POPUP, // 无边框弹出窗口
            rect.left,
            rect.top,
            width,
            height,
            None, // 无父窗口（独立置顶窗口）
            None,
            Some(hinstance.into()),
            None,
        )
        .map_err(|e| e.to_string())?;

        if hwnd.is_invalid() {
            return Err("CreateWindowExW 返回无效句柄".to_string());
        }

        // 4. 无焦点显示（SW_SHOWNOACTIVATE 不会抢走目标窗口的焦点）
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        OVERLAY_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
        // 同上：记录目标窗口句柄，供移动/缩放跟随。
        OVERLAY_TARGET_HWND.store(target.0 as isize, Ordering::SeqCst);
        *OVERLAY_TARGET.lock().unwrap() = Some(app_name.to_string());
    }

    Ok(())
}

/// 「请求隐藏遮罩」的标志位。
///
/// 【为什么不直接各线程调用 hide_overlay？】
/// 遮罩窗口创建在「事件处理线程」上，而 Win32 有一条硬规则：
/// **窗口只能由它的属主线程销毁**。Hello 认证线程、会话监听线程
/// 直接 DestroyWindow 会静默失败（遮罩永远关不掉）或句柄泄漏。
/// 所以其他线程只举旗（request_hide），真正动手由属主线程的循环代劳。
static HIDE_REQUESTED: AtomicBool = AtomicBool::new(false);

/// 请求隐藏遮罩：**任何线程**都可以安全调用。
pub fn request_hide() {
    HIDE_REQUESTED.store(true, Ordering::SeqCst);
}

/// 由事件处理线程的消息循环周期性调用：有隐藏请求就真正销毁。
/// 延迟最多一个循环周期（约 16ms），肉眼不可感。
pub fn pump_hide_request() {
    if HIDE_REQUESTED.swap(false, Ordering::SeqCst) {
        hide_overlay();
    }
}

/// 检查目标窗口是否被移动/调整大小，是则把遮罩挪过去（由遮罩属主线程周期调用）。
///
/// 【为什么由属主线程做】SetWindowPos 属于窗口操作，按"谁创建谁管理"的规则
/// 放在遮罩属主线程（事件处理线程）最稳；该线程本来就每 16ms 醒一次，
/// 顺手做两次 GetWindowRect 对比，开销可以忽略。
pub fn track_target_move() {
    let hwnd_val = OVERLAY_HWND.load(Ordering::SeqCst);
    if hwnd_val == 0 {
        return; // 没有遮罩，无事可做
    }
    let target_val = OVERLAY_TARGET_HWND.load(Ordering::SeqCst);
    if target_val == 0 {
        return; // 没有记录目标（理论上不会发生），跳过
    }
    unsafe {
        let target = HWND(target_val as *mut _);
        if !IsWindow(Some(target)).as_bool() {
            return; // 目标窗口已关闭（前台事件马上会来处理遮罩去留）
        }

        // 取目标窗口的当前矩形与遮罩的当前矩形，逐字段比较。
        let mut tr = RECT::default();
        if GetWindowRect(target, &mut tr).is_err() {
            return;
        }
        let overlay = HWND(hwnd_val as *mut _);
        let mut or = RECT::default();
        if GetWindowRect(overlay, &mut or).is_err() {
            return;
        }
        // 矩形没变就什么都不做，避免每帧 SetWindowPos 造成无谓抖动。
        if or.left == tr.left && or.top == tr.top && or.right == tr.right && or.bottom == tr.bottom
        {
            return;
        }

        // 目标动了：把遮罩挪到新矩形上，保持置顶与无焦点。
        let _ = SetWindowPos(
            overlay,
            Some(HWND_TOPMOST),
            tr.left,
            tr.top,
            tr.right - tr.left,
            tr.bottom - tr.top,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
}

/// 隐藏并销毁遮罩窗口（**只能在属主线程调用**，外部请用 request_hide）。
///
/// 销毁后**不需要**手动把焦点还给目标窗口：因为遮罩从未抢过焦点
/// （`WS_EX_NOACTIVATE` + `SW_SHOWNOACTIVATE`），焦点天然还在原窗口上，
/// 这就是「丝滑归还焦点」的实现原理（对应 DESIGN §2.3）。
fn hide_overlay() {
    let existing = OVERLAY_HWND.swap(0, Ordering::SeqCst);
    if existing == 0 {
        return;
    }
    let hwnd = HWND(existing as *mut _);
    unsafe {
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = ShowWindow(hwnd, SW_HIDE);
            let _ = DestroyWindow(hwnd);
        }
    }
}

/// 遮罩当前是否正在显示。
pub fn is_showing() -> bool {
    let existing = OVERLAY_HWND.load(Ordering::SeqCst);
    if existing == 0 {
        return false;
    }
    unsafe { IsWindow(Some(HWND(existing as *mut _))).as_bool() }
}

/// 遮罩当前盖着的应用文件名（None = 没盖任何应用）。
///
/// 给认证线程用：验证通过后，先确认「遮罩现在盖的还是不是我验证的那个应用」，
/// 是才销毁遮罩，避免误关另一个还没验证的应用的遮罩。
pub fn current_target() -> Option<String> {
    OVERLAY_TARGET.lock().unwrap().clone()
}

/// 取当前遮罩窗口句柄（用于消息路由）。
/// PoC 阶段暂未使用，Phase 4 的输入拦截与消息路由会用到，故先保留。
#[allow(dead_code)]
pub fn overlay_hwnd() -> Option<HWND> {
    let existing = OVERLAY_HWND.load(Ordering::SeqCst);
    if existing == 0 {
        None
    } else {
        Some(HWND(existing as *mut _))
    }
}

/// 把 Rust 字符串转成以 NUL 结尾的 UTF-16 宽字符串。
/// （windows crate 的 `w!` 宏只支持字面量，动态字符串需要手动转换）
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
