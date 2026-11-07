//! 构建脚本：固定嵌入 manifest 的请求执行级别为 asInvoker。
//!
//! # 为什么必须显式写死 asInvoker
//! HelloGate 的「管理员模式」不靠 manifest 强制提权（requireAdministrator），
//! 而是运行时用 ShellExecuteW("runas") 自举——这样才有机会弹一次 UAC 后常驻，
//! 并且被 IFEO 重定向拉起的守门子进程（`hellogate.exe --gate`）能保持普通权限，
//! 正常弹 Windows Hello / 密码框。
//!
//! 若 manifest 写成 requireAdministrator，守门子进程会被系统强制提权：
//! 每次启动受保护程序都闪一下 UAC，体验完全不可接受。
//! 所以这里显式钉死 asInvoker，杜绝任何环境下的默认值差异。
fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("msvc") {
        // 仅 MSVC 工具链支持 /MANIFEST 链接器参数（GNU 工具链没有这套）。
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTUAC:level='asInvoker'");
    }

    // 把「编译时刻」作为环境变量嵌进程序（编译期求值，运行期只读）。
    //
    // 【为什么需要它】排查问题时反复遇到"用户跑的是哪个版本"说不清：
    // 常驻实例是老进程、磁盘上的 exe 已经换了新代码，界面却还是旧样子。
    // 启动日志里打出这串编译时间，一眼就能对上"进程启动时间 < exe 编译时间"
    // 这类陈旧进程问题（见 main.rs 启动分隔线处的用法）。
    let build_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=HELLOGATE_BUILD_TS={build_time}");

    // ---- 品牌资源：把图标与文件属性编进 exe（仅 Windows 产物需要）----
    //
    // winres 是"构建期"工具：它调用系统自带的 rc.exe 把 .ico / 版本信息
    // 编译成 .res 文件交给链接器，产物运行时不依赖任何东西。
    //
    // 图标由 tools\make_icon.ps1 生成（多尺寸 PNG 条目 ICO）。
    // winres 会把它登记为 MAINICON 组、资源 ID = 1——
    // 这就是运行时用 LoadIconW(模块句柄, MAKEINTRESOURCE(1)) 能取到它的原因；
    // 同时资源管理器/桌面/任务栏也直接认这个 ID=1 作为 exe 图标。
    if target.contains("msvc") {
        // 图标文件变了要重跑 build.rs（cargo 默认只监听 src/ 与 build.rs）。
        println!("cargo:rerun-if-changed=assets/hellogate.ico");

        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/hellogate.ico");
        // 以下字段进 exe 的「右键 → 属性 → 详细信息」，排查版本时一眼可辨。
        // winres 0.1 的字符串属性走通用的 set(键, 值)：
        //   "ProductName"   = 产品名（任务管理器"详细信息"页也读它）
        //   "FileDescription" = 文件描述
        res.set("ProductName", "HelloGate");
        res.set("FileDescription", "HelloGate - 非侵入式 Windows 应用锁");
        // 资源语言 0x0804 = 简体中文（zh-CN）：
        // 不设的话资源块按默认语言登记，中文系统上属性页可能显示成英文字段。
        res.set_language(0x0804);
        if let Err(e) = res.compile() {
            // rc.exe 来自 Windows SDK：报这个错通常说明当前环境缺 SDK 工具链，
            // 或 assets/hellogate.ico 不存在（先跑 tools\make_icon.ps1）。
            panic!("winres 资源编译失败（检查 rc.exe 是否可用、assets/hellogate.ico 是否存在）: {e}");
        }
    }
}
