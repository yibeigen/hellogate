//! 自绘的应用列表控件（替代系统 ListView）。
//!
//! # 为什么要自己写一个列表
//! 系统 `SysListView32` 是 Windows 95 时代的设计：灰白 3D 表头、方框勾选框、
//! 系统滚动条、白色直角边框。即使把主题设成 Explorer，它仍然是"报表"长相，
//! 放在现代卡片界面里一眼就露馅——这是主控台最显旧的元素。
//!
//! 这里用一个普通的子窗口自己绘制每一行：
//! - 没有列头（应用名 + 进程文件名分两行显示，更像现代列表而不是表格）；
//! - 行高更宽松（64 逻辑像素，现代化批次从 56 加高），有悬停高亮、选中高亮；
//! - 勾选框是圆角小方块，和界面其它勾选框完全一致；
//! - 行与行之间只有一条极浅的分隔线，没有表格网格。
//!
//! # 数据放在哪
//! 列表的行数据保存在本模块的线程局部变量里（`ROWS`），控件只负责画和收点击。
//! 主控台保存配置时直接读 `snapshot()`，不需要再向控件发一堆 `LVM_*` 消息。
//!
//! 【为什么不用 ListView 的“虚拟数据”方案】那仍然要依赖 ListView 的绘制与滚动，
//! 改不掉它的外观；而这里的行数最多也就十几条，一个 `Vec` 完全够用。

use std::cell::{Cell, RefCell};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreatePen, CreateSolidBrush, DeleteObject, EndPaint, IntersectClipRect,
    InvalidateRect, LineTo, MoveToEx, Polygon, SelectObject, HDC, HFONT, PAINTSTRUCT, PS_SOLID,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetClientRect, LoadCursorW, RegisterClassW,
    IDC_ARROW, CS_HREDRAW, CS_VREDRAW, WM_ERASEBKGND, WM_LBUTTONDOWN, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_PAINT, WNDCLASSW, WINDOW_EX_STYLE, WS_CHILD, WS_VISIBLE,
    WM_DESTROY,
};

/// `WM_MOUSELEAVE` 的消息号（winuser.h 里有，windows crate 没有单独导出这个常量）。
const WM_MOUSELEAVE: u32 = 0x02A3;

use crate::ui::{
    draw_check_box, draw_text_center, draw_text_left, fill_round, fill_solid, Palette,
};

/// 列表窗口类名（主控台通过它创建/查找）。
const LIST_CLASS: &str = "HelloGateAppListClass";

/// 一行应用数据。
#[derive(Clone)]
struct Row {
    /// 显示名（如 "chrome"）。
    display: String,
    /// 进程文件名（如 "chrome.exe"），用于去重和保存。
    file_name: String,
    /// 勾选 = 启用拦截；取消 = 这个程序不拦。
    enabled: bool,
}

thread_local! {
    /// 列表的全部行数据。整个进程只有一个主控台，所以一份就够了。
    static ROWS: RefCell<Vec<Row>> = const { RefCell::new(Vec::new()) };
    /// 当前被鼠标选中的行（-1 = 没有）。
    static SELECTED: Cell<i32> = const { Cell::new(-1) };
    /// 鼠标当前悬停的行（-1 = 不在任何行上）。
    static HOVER: Cell<i32> = const { Cell::new(-1) };
    /// 纵向滚动偏移（像素，0 = 顶部）。
    static SCROLL: Cell<i32> = const { Cell::new(0) };
    /// 本窗口的 DPI（96 = 100%），创建时由主控台传入。
    static LIST_DPI: Cell<i32> = const { Cell::new(96) };
    /// 主字体 / 副字体（创建时由主控台传入，保证和界面其它文字一致）。
    static LIST_FONT: Cell<isize> = const { Cell::new(0) };
    static LIST_SMALL_FONT: Cell<isize> = const { Cell::new(0) };
}

/// 把 96 DPI 下的逻辑像素换算成真实像素。
fn sc(logical: i32) -> i32 {
    // DPI 来源不改（见 ui::scale 文档与决策 D2）：继续读本窗口创建时主控台
    // 传进来的 LIST_DPI thread_local，保证和列表行高/内边距用同一份基准。
    crate::ui::scale(logical, LIST_DPI.with(|d| d.get()))
}

/// 创建列表控件。
///
/// # 参数
/// - `parent`：主控台窗口。
/// - `x/y/w/h`：**已换算好的物理像素**位置与大小。
/// - `dpi`：当前缩放基准（`SYSTEM_DPI` 的值），列表内部的行高/间距靠它换算。
/// - `font` / `small_font`：主字体（16px）与副字体（14px）。
pub unsafe fn create(
    parent: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    dpi: i32,
    font: HFONT,
    small_font: HFONT,
) -> HWND {
    // 记录缩放与字体，后续所有绘制/命中判定都依赖它们。
    LIST_DPI.with(|d| d.set(dpi));
    LIST_FONT.with(|f| f.set(font.0 as isize));
    LIST_SMALL_FONT.with(|f| f.set(small_font.0 as isize));
    SELECTED.with(|s| s.set(-1));
    HOVER.with(|h| h.set(-1));
    SCROLL.with(|s| s.set(0));

    let class_name = wide(LIST_CLASS);
    let hinstance = GetModuleHandleW(None).unwrap_or_default();

    // 注册窗口类。重复注册会失败，但这里只在主控台打开时调用一次，忽略返回值即可。
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(list_wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        ..Default::default()
    };
    let _ = RegisterClassW(&wc);

    CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(class_name.as_ptr()),
        PCWSTR::null(),
        WS_CHILD | WS_VISIBLE,
        x,
        y,
        w,
        h,
        Some(parent),
        None,
        Some(hinstance.into()),
        None,
    )
    .unwrap_or_default()
}

/// 追加一行。勾选框默认按调用方传入的状态显示。
pub fn add_row(display: &str, file_name: &str, enabled: bool) {
    ROWS.with(|r| {
        r.borrow_mut().push(Row {
            display: display.to_string(),
            file_name: file_name.to_string(),
            enabled,
        });
    });
}

/// 判断某个进程文件名是否已经在列表里（添加时去重用）。
pub fn contains_file(file_name: &str) -> bool {
    ROWS.with(|r| {
        r.borrow()
            .iter()
            .any(|row| row.file_name.eq_ignore_ascii_case(file_name))
    })
}

/// 当前选中行的下标（-1 = 没有选中）。
///
/// 【为什么暴露这个】「移除」按钮改成了两段式确认（方案 D · 危险操作：
/// 点第一下只预备、再点才真删）。主控台要判断"现在选中的还是不是
/// 刚才预备删除的那一行"——选中一变就得重新预备，否则会一下删掉
/// 没被确认过的行。
pub fn selected_index() -> i32 {
    SELECTED.with(|s| s.get())
}

/// 删除当前选中的行，返回是否真的删掉了（没选中时返回 false，调用方据此提示用户）。
pub unsafe fn remove_selected(hwnd: HWND) -> bool {
    let sel = SELECTED.with(|s| s.get());
    if sel < 0 {
        return false;
    }
    let removed = ROWS.with(|r| {
        let mut rows = r.borrow_mut();
        let idx = sel as usize;
        if idx >= rows.len() {
            return false;
        }
        rows.remove(idx);
        true
    });
    // 删完后选中状态清空，并把滚动位置收回合法范围（否则删光了还停在半空）。
    SELECTED.with(|s| s.set(-1));
    clamp_scroll(hwnd);
    let _ = InvalidateRect(Some(hwnd), None, true);
    removed
}

/// 读取全部行数据（保存配置时用）。返回 `(显示名, 进程文件名, 是否启用)`。
pub fn snapshot() -> Vec<(String, String, bool)> {
    ROWS.with(|r| {
        r.borrow()
            .iter()
            .map(|row| (row.display.clone(), row.file_name.clone(), row.enabled))
            .collect()
    })
}

/// 请求列表重绘（新增行、外部改了数据后用）。
pub unsafe fn refresh(hwnd: HWND) {
    let _ = InvalidateRect(Some(hwnd), None, true);
}

/// 清空全部数据（主控台每次重新创建窗口时调用，避免上一次的行残留）。
pub fn clear() {
    ROWS.with(|r| r.borrow_mut().clear());
    SELECTED.with(|s| s.set(-1));
    HOVER.with(|h| h.set(-1));
    SCROLL.with(|s| s.set(0));
}

/// 把 Rust 字符串转成 NUL 结尾的 UTF-16。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 列表窗口过程。
unsafe extern "system" fn list_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        // 背景必须自己擦成白色：默认擦除会让行与行之间的空隙露出父窗口的底色。
        WM_ERASEBKGND => {
            let hdc = HDC(wparam.0 as *mut _);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            fill_solid(hdc, rc, Palette::CARD);
            LRESULT(1)
        }
        WM_PAINT => {
            draw_list(hwnd);
            LRESULT(0)
        }
        // 鼠标移动：更新悬停行；同时登记"鼠标离开"通知，否则悬停高亮会一直留着。
        WM_MOUSEMOVE => {
            let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
            let idx = hit_test(y);
            if HOVER.with(|h| h.get()) != idx {
                HOVER.with(|h| h.set(idx));
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
            let mut tme = TRACKMOUSEEVENT {
                // cbSize 必须填，系统靠它区分结构体版本（不填会直接调用失败）。
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            HOVER.with(|h| h.set(-1));
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        // 点击：左边勾选框区域 = 切换启用状态；其余区域 = 选中整行。
        WM_LBUTTONDOWN => {
            let x = (lparam.0 & 0xFFFF) as i32;
            let y = ((lparam.0 >> 16) & 0xFFFF) as i32;
            let idx = hit_test(y);
            if idx >= 0 {
                // 勾选框的横向范围：左侧内边距 18 + 方块 22 + 一点余量
                // （方块实际画在 item_rc.left+18 = 26 处，命中区从 18 起更宽容）。
                let box_left = sc(18);
                let box_right = box_left + sc(36);
                let hit_box = x >= box_left && x <= box_right;
                ROWS.with(|r| {
                    let mut rows = r.borrow_mut();
                    if let Some(row) = rows.get_mut(idx as usize) {
                        if hit_box {
                            row.enabled = !row.enabled;
                        }
                    }
                });
                if !hit_box {
                    SELECTED.with(|s| s.set(idx));
                }
                let _ = InvalidateRect(Some(hwnd), None, true);
            }
            LRESULT(0)
        }
        // 滚轮：每次滚 3 行（步长随行高 56→64 同步）。行数少时 clamp_scroll 会把偏移夹回 0。
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let step = sc(64) * 3;
            let next = SCROLL.with(|s| s.get()) - delta * step / 120;
            SCROLL.with(|s| s.set(next));
            clamp_scroll(hwnd);
            let _ = InvalidateRect(Some(hwnd), None, true);
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 把客户区 y 坐标换算成行索引；不在任何行上时返回 -1。
unsafe fn hit_test(y: i32) -> i32 {
    let row_h = sc(64);
    let offset = y + SCROLL.with(|s| s.get()) - sc(4);
    if offset < 0 {
        return -1;
    }
    let idx = offset / row_h;
    let count = ROWS.with(|r| r.borrow().len() as i32);
    if idx < count {
        idx
    } else {
        -1
    }
}

/// 滚动偏移不能小于 0，也不能超过"最后一行刚好露出来"。
unsafe fn clamp_scroll(hwnd: HWND) {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let content = sc(8) + ROWS.with(|r| r.borrow().len() as i32) * sc(64);
    let max = (content - rc.bottom).max(0);
    let cur = SCROLL.with(|s| s.get());
    SCROLL.with(|s| s.set(cur.clamp(0, max)));
}

/// 绘制整个列表。
unsafe fn draw_list(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);

    // 白底（这个控件放在白色卡片里，与卡片同色，看不到边界）。
    fill_solid(hdc, rc, Palette::CARD);

    // 主字体（名称）与副字体（文件名）。
    let main_font = LIST_FONT.with(|f| f.get());
    let sub_font = LIST_SMALL_FONT.with(|f| f.get());

    let count = ROWS.with(|r| r.borrow().len());
    if count == 0 {
        // ---- 空状态：居中的「盾牌 + 标题 + 引导语」三件套 ----
        //
        // 【为什么从"一行左对齐灰字"升级成居中图形】原来只有一行贴左边的
        // 小灰字，用户第一眼会以为列表"坏了/没加载出来"，而且左对齐的文字
        // 在大片空白里非常不起眼。图标 + 标题 + 引导语整体居中，是现代应用
        // 设置页通用的空状态语言：先用图形吸引视线，再明确告诉用户
        // "这里现在是空的"以及"下一步该做什么"。
        //
        // 【为什么不内嵌「添加应用」按钮】（规格决策 D1）按钮已经常驻在卡片
        // 标题行右上角，如果这里再放一个，同一功能就有两个入口，用户反而要
        // 犹豫该点哪个。所以这里只用引导文案把视线引过去。

        // 整个空状态块的逻辑高度：盾牌 44 + 间距 14 + 标题行 24 + 间距 8 + 引导行 22
        // （两行文字行高随字号 15→16 / 13→14 各加 2）。
        let total = sc(44) + sc(14) + sc(24) + sc(8) + sc(22);
        // 垂直方向尽量整体居中；列表太矮放不下时至少贴着顶部 8px 起画，
        // 避免算出负坐标画到客户区外面去。
        let shield_top = ((rc.bottom - total) / 2).max(sc(8));
        // 水平方向：以客户区正中（rc.right / 2）为基准，盾牌和两行文字都对齐它。
        let cx = rc.right / 2;
        let shield_left = cx - sc(40) / 2;

        // 1) 盾牌：五个点的直线多边形，纯几何绘制（禁止 emoji，emoji 在 GDI
        //    里既画不出统一风格，也会随系统字体变化长得不一样）。
        //    坐标是 40×44 逻辑盾内的相对位置：
        //    顶边水平 (0,0)→(40,0)，两侧到腰部 (37,24)/(3,24) 各微微内收 3px
        //    （"腰微收"），最后两条斜线收拢到底部尖 (20,44)。
        let pts = [
            POINT { x: shield_left, y: shield_top },
            POINT { x: shield_left + sc(40), y: shield_top },
            POINT { x: shield_left + sc(37), y: shield_top + sc(24) },
            POINT { x: shield_left + sc(20), y: shield_top + sc(44) },
            POINT { x: shield_left + sc(3), y: shield_top + sc(24) },
        ];
        // 填充用品牌浅色、描边用品牌色。
        // 【GDI 资源为什么要这样管】CreateSolidBrush/CreatePen 新建的对象
        // 必须先把旧对象 SelectObject 换回来，才能 DeleteObject 删掉——
        // 正在被 DC 使用的对象删不掉，漏一次就泄漏一次（和 fill_round、
        // draw_check_box 用的是同一套管理模式）。Polygon 会自动用当前画刷
        // 填充、当前画笔描边（包括闭合回起点的那条边）。
        let fill = CreateSolidBrush(COLORREF(Palette::BRAND_SOFT));
        let pen = CreatePen(PS_SOLID, 1, COLORREF(Palette::BRAND));
        let old_brush = SelectObject(hdc, fill.into());
        let old_pen = SelectObject(hdc, pen.into());
        let _ = Polygon(hdc, &pts);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(pen.into());
        let _ = DeleteObject(fill.into());

        // 2) 盾牌内部的白色对勾：MoveToEx + LineTo 两段折线，宽 3 逻辑像素。
        //    三个点 (11,22)→(18,29)→(30,15) 都在 40×44 盾内，横竖方向都大致
        //    居中，视觉上正好落在盾牌中心偏上一点（对勾重心本来就偏上）。
        let check_pen = CreatePen(
            PS_SOLID,
            sc(3).max(1), // 宽度至少 1，否则高 DPI 下可能退化成不可见
            COLORREF(Palette::ON_BRAND),
        );
        let old_pen = SelectObject(hdc, check_pen.into());
        let _ = MoveToEx(hdc, shield_left + sc(11), shield_top + sc(22), None);
        let _ = LineTo(hdc, shield_left + sc(18), shield_top + sc(29));
        let _ = LineTo(hdc, shield_left + sc(30), shield_top + sc(15));
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(check_pen.into());

        // 3) 标题：列表主字体（16px）、正文色，画在盾牌底部下方 14 逻辑像素处，
        //    相对客户区水平居中（draw_text_center 内部用 DT_CENTER|DT_VCENTER）。
        let title_top = shield_top + sc(44) + sc(14);
        if main_font != 0 {
            let _ = SelectObject(hdc, HFONT(main_font as *mut _).into());
        }
        draw_text_center(
            hdc,
            "还没有受保护的应用",
            RECT {
                left: 0,
                right: rc.right,
                top: title_top,
                bottom: title_top + sc(24),
            },
            Palette::TEXT,
        );

        // 4) 引导语：副字体（14px）、弱化灰，标题下方 8 逻辑像素处，水平居中。
        //    按钮已移到卡片标题行右上角，所以文案必须指向那里（旧文案是
        //    "点右上角"开头的一整句，现在拆成标题 + 引导两行更好读）。
        let hint_top = title_top + sc(24) + sc(8);
        if sub_font != 0 {
            let _ = SelectObject(hdc, HFONT(sub_font as *mut _).into());
        }
        draw_text_center(
            hdc,
            "点击右上角「＋ 添加应用」，选择要锁定的程序",
            RECT {
                left: 0,
                right: rc.right,
                top: hint_top,
                bottom: hint_top + sc(22),
            },
            Palette::TEXT_MUTED,
        );

        let _ = EndPaint(hwnd, &ps);
        return;
    }

    // 裁剪到客户区：滚出上方的行不能画到卡片外面去。
    let _ = IntersectClipRect(hdc, 0, 0, rc.right, rc.bottom);

    let row_h = sc(64);
    let scroll = SCROLL.with(|s| s.get());
    let selected = SELECTED.with(|s| s.get());
    let hover = HOVER.with(|h| h.get());

    for (i, row) in ROWS.with(|r| r.borrow().clone()).iter().enumerate() {
        let top = sc(4) + i as i32 * row_h - scroll;
        // 完全不可见的行直接跳过，省掉绘制开销。
        if top > rc.bottom || top + row_h < 0 {
            continue;
        }

        let item_rc = RECT {
            left: sc(8),
            top: top + sc(2),
            right: rc.right - sc(8),
            bottom: top + row_h - sc(2),
        };

        // 行背景：选中 > 悬停 > 透明（白）。用圆角块，比直角高亮更现代。
        let bg = if i as i32 == selected {
            Palette::ROW_SEL
        } else if i as i32 == hover {
            Palette::ROW_HOVER
        } else {
            Palette::CARD
        };
        if bg != Palette::CARD {
            fill_round(hdc, item_rc, bg, sc(16));
        }

        // 勾选框：垂直居中，左侧内边距 18px（现代化批次 14→18）；
        // 方块 20→22，选中态由数据里的 enabled 决定。
        let box_size = sc(22);
        draw_check_box(
            hdc,
            item_rc.left + sc(18),
            item_rc.top + (item_rc.bottom - item_rc.top - box_size) / 2,
            box_size,
            row.enabled,
            sc(5),
        );

        // ---- 两行文字：应用名在上、进程文件名在下，而且**左侧对齐到同一条竖线** ----
        //
        // 【为什么不再"名字靠左、文件名靠右"】
        // 那样两段文字会落在行的两个对角上（左上 / 右下）：眼睛必须横跨整行
        // 才能把 "winword" 和 "winword.exe" 对应起来，读起来像两列毫不相干的信息。
        // 现代列表（GitHub、Linear、Notion 的设置页）都是"主信息在上、
        // 次要信息紧跟其下、共用同一个左边界"——视线只需往下扫半行就够。
        // 文字左边界 = 内边距 18 + 方块 22 + 间距 12 = 52（原 46 = 14+20+12）。
        let text_left = item_rc.left + sc(52);
        let text_right = item_rc.right - sc(12);

        // 第一行：应用名（16px 正文色）。行内容区高 60（64−上下各 2），
        // 名字 24 + 文件名 22 = 46，上留 7 下留 7，整体垂直居中。
        if main_font != 0 {
            let _ = SelectObject(hdc, HFONT(main_font as *mut _).into());
        }
        draw_text_left(
            hdc,
            &row.display,
            RECT {
                left: text_left,
                right: text_right,
                top: item_rc.top + sc(7),
                bottom: item_rc.top + sc(31),
            },
            Palette::TEXT,
        );

        // 第二行：进程文件名（14px 三级灰），紧贴名字下方，左边界与名字完全一致。
        if sub_font != 0 {
            let _ = SelectObject(hdc, HFONT(sub_font as *mut _).into());
        }
        draw_text_left(
            hdc,
            &row.file_name,
            RECT {
                left: text_left,
                right: text_right,
                top: item_rc.top + sc(31),
                bottom: item_rc.top + sc(53),
            },
            Palette::TEXT_MUTED,
        );

        // 分隔线：画在行的下边缘，左右各内缩（14→18，与勾选框内边距对齐），
        // 避免顶到圆角高亮块。
        if i + 1 < count {
            let sep_color = Palette::BORDER_SOFT;
            let pen = CreatePen(PS_SOLID, 1, COLORREF(sep_color));
            let old = SelectObject(hdc, pen.into());
            let _ = windows::Win32::Graphics::Gdi::MoveToEx(
                hdc,
                item_rc.left + sc(18),
                top + row_h,
                None,
            );
            let _ = windows::Win32::Graphics::Gdi::LineTo(hdc, item_rc.right - sc(18), top + row_h);
            SelectObject(hdc, old);
            let _ = DeleteObject(pen.into());
        }
    }

    // ---- 右侧滚动指示条 ----
    //
    // 【为什么要画它】列表视口现在只露 4 行，行数一多内容就在视口外面，
    // 而这里没有系统滚动条。不画任何提示的话，用户会以为"只加进去两行，
    // 第三行丢了"。一条细指示条就能说明"还有内容，滚轮可以往下翻"。
    // 位置贴着右边缘、宽 4px，颜色用最浅的描边灰，不抢任何注意力。
    let content_h = sc(8) + count as i32 * row_h;
    if content_h > rc.bottom {
        let track_top = sc(4);
        let track_h = rc.bottom - sc(8);
        // 滑块高度按"可见比例"算，但至少 24px，否则行数很多时滑块细得看不见。
        let thumb_h = ((track_h as f32) * rc.bottom as f32 / content_h as f32)
            .max(sc(24) as f32) as i32;
        let max_scroll = (content_h - rc.bottom).max(1);
        let thumb_top = track_top + (track_h - thumb_h) * scroll / max_scroll;
        fill_round(
            hdc,
            RECT {
                left: rc.right - sc(6),
                top: thumb_top,
                right: rc.right - sc(2),
                bottom: thumb_top + thumb_h,
            },
            Palette::BORDER_STRONG,
            sc(2),
        );
    }

    let _ = EndPaint(hwnd, &ps);
}
