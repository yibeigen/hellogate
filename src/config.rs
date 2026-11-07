//! 配置模块（Phase 4）
//!
//! 职责：
//! 1. 定义用户配置结构（受保护应用列表、宽限期策略、自启开关等）。
//! 2. 用 **Windows DPAPI** 加密后落盘，防止配置被随意篡改（例如有人偷偷把
//!    某个受保护应用从列表里删掉来绕过锁定）。
//!
//! # 为什么用 DPAPI 而不是 AES + 主密码？
//! - **零密码负担**：DPAPI 的密钥由 Windows 自动派生并绑定当前用户账户
//!   （`CryptProtectData` 用用户登录凭据派生的密钥），用户无需记任何密码。
//! - **防篡改足够**：本项目的配置（哪些应用被锁）不是高敏机密，防的是「随手改配置绕过」，
//!   DPAPI 完全够用；真要防有心人，还得配合文件 ACL + 进程保护，那是另一个话题。
//! - **迁移到别的机器/账户后自动失效**：这是特性而非缺陷——配置与用户绑定更安全。
//!
//! # DPAPI 是什么？
//! Data Protection API 是 Windows 提供的加密服务。`CryptProtectData` 把明文加密成
//! 一段二进制 blob（加密密钥藏在系统里，只有同一用户能解开）；`CryptUnprotectData`
//! 反向解密。整个过程中我们**从不接触、也无需保存任何密钥**。

use std::path::PathBuf;

use crate::logln;
use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{LocalFree, HLOCAL};
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
};

/// 宽限期模式（对应 DESIGN_AND_ROADMAP.md §4 的「双模宽限期」）。
///
/// 用户解锁某个应用后，可以在一段时间内自由切换而无需重复验证。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraceMode {
    /// 一次性：解锁后仅本次有效，下次切回来仍需验证（最严格）。
    Once,
    /// 时间型：解锁后 N 分钟内免验证（如 5 分钟）。
    Timed,
    /// 会话型：解锁后直到注销/重启前都免验证（最宽松）。
    Session,
}

impl Default for GraceMode {
    fn default() -> Self {
        GraceMode::Once
    }
}

/// 单个受保护应用的配置项。
// PartialEq：主控台关窗时要把「界面当前状态」和「打开时的原始配置」做对比，
// 判断是否存在未保存的修改（见 console.rs 的 request_close）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProtectedApp {
    /// exe 文件名（小写，例如 `notepad.exe`）。这是匹配的主键。
    pub file_name: String,
    /// 展示给用户看的友好名称（例如「记事本」）。
    pub display_name: String,
    /// 完整的 exe 绝对路径（用于展示与未来的签名校验）。
    /// 允许为空：因为同一个 exe 可能从不同路径启动，按文件名匹配更实用。
    #[serde(default)]
    pub full_path: String,
    /// 是否启用保护。
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// 应用的全部配置（会整体加密后落盘）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// 配置结构版本号，便于未来平滑升级（迁移旧配置）。
    #[serde(default = "default_version")]
    pub version: u32,
    /// 受保护应用列表。
    #[serde(default)]
    pub apps: Vec<ProtectedApp>,
    /// 宽限期模式。
    #[serde(default)]
    pub grace_mode: GraceMode,
    /// 时间型宽限期的时长（秒）。仅在 `grace_mode == Timed` 时生效。
    #[serde(default = "default_grace_seconds")]
    pub grace_seconds: u64,
    /// 是否在锁屏/睡眠唤醒后**重新锁定**所有已解锁应用。
    #[serde(default = "default_true")]
    pub relock_on_session_change: bool,
    /// 是否随系统开机自启。
    #[serde(default)]
    pub autostart: bool,
    /// 主密码的 PBKDF2-SHA256 派生值（64 位小写十六进制），空串 = 未设置。
    ///
    /// 【为什么需要它】
    /// Windows Hello（指纹/人脸/PIN）不是每台电脑都配了。旧版在 Hello 不可用时
    /// 直接放行——等于锁失效。现在改为：Hello 不可用 → 要求输入主密码。
    ///
    /// 【为什么只存派生值，见 DESIGN_AND_ROADMAP.md §3.2】
    /// - 绝不保存明文，也不用 DPAPI 那种**可逆**加密（竞品 SecureAppLocker 的缺陷：
    ///   同机任意进程都能解出明文密码）。
    /// - 用 PBKDF2-SHA256 迭代 10 万次：即使配置泄露，离线暴力猜密码的成本
    ///   也被放大十万倍。
    /// - 解锁时把用户输入加盐派生后与这里比对。
    #[serde(default)]
    pub master_password_hash: String,
    /// 主密码盐（32 位小写十六进制 = 16 字节随机），与派生值成对存储。
    ///
    /// 【盐的作用】让同一密码在这台机器和那台机器上得到完全不同的存储值，
    /// 攻击者预先算好的「常见密码表」（彩虹表）就失效了。
    /// 盐不是机密，可以明文存（这里仍会随配置一起被 DPAPI 加密，属于额外一层）。
    #[serde(default)]
    pub master_password_salt: String,
    /// 解锁时优先使用哪种验证方式。
    ///
    /// - `"hello"`（默认）：优先弹 Windows Hello，不可用时回退主密码（旧行为）；
    /// - `"password"`：优先弹自设密码框（用户明确选择"不用系统验证"）。
    ///
    /// 【为什么用 String 而不是枚举】旧配置文件里没有这个字段，serde 反序列化时
    /// 用默认值 "hello" 补齐即可，天然向后兼容；取值只在这两个字符串里出，
    /// 读的地方统一用 `== "password"` 判断，不会出现第三种值。
    #[serde(default = "default_auth_method")]
    pub auth_method: String,
    /// ★ 总开关：是否启用「打开受保护软件之前的验证」（IFEO 启动拦截）。
    ///
    /// 【为什么需要它】用户要求这个验证能一键关掉：比如临时要跑个批处理、
    /// 或者调试某个程序时反复验证太烦。关掉后 HelloGate 会**删除所有启动拦截
    /// 登记**，受保护程序双击即开，和没装 HelloGate 完全一样（见 ifeo.rs 的 sync）。
    ///
    /// 默认 `true`：装了锁就该上锁，用户不需要锁时自己去主控台取消勾选。
    #[serde(default = "default_true")]
    pub guard_enabled: bool,
    /// 运行中窗口遮罩（高级开关，默认 `false`）。
    ///
    /// 【为什么默认关】用户明确要求"只在打开软件前验证"：程序一旦跑起来
    /// 就不再反复弹验证/盖遮罩。遮罩链路（前台窗口钩子）是历史上卡死事故的
    /// 主要来源，默认不启动它，也就没有这条风险路径。
    /// 保留字段是为了让代码里的遮罩实现仍可被引用（高级用户可手动开启）。
    #[serde(default)]
    pub overlay_enabled: bool,
    /// ★ 授权码是否已激活（一次性，激活后永久生效，不再询问）。
    ///
    /// 【为什么放配置里】配置整体走 DPAPI 加密落盘，绑定当前 Windows 账户，
    /// 用户随手改文件改不掉；激活状态跟着配置走，重装软件后需要重新输入
    /// 授权码（这是预期行为，授权码在公众号「艺杯羹」随时可领）。
    ///
    /// 【为什么默认 false】`#[serde(default)]` 让老配置文件（没有这个字段）
    /// 加载后自动视为"未激活"，首次升级启动时会弹一次激活框。
    #[serde(default)]
    pub license_activated: bool,
}

fn default_auth_method() -> String {
    // ★ 默认就用「自设密码」。
    //
    // 【为什么改】用户明确要求"默认设置密码"：开箱即用、不依赖 Windows Hello。
    // Hello 在管理员权限的常驻进程里偶发弹不出来（系统行为），把它当默认
    // 验证方式等于把首次使用的门槛放在一个不稳定的环节上。密码框是我们
    // 自己的窗口，永远弹得出来，最可靠。
    "password".to_string()
}

/// 出厂的默认密码。
///
/// # 为什么要有默认密码
/// 用户要求"默认设置密码"：装好就能用，不用先折腾"去哪设密码"。
/// 这个值只用于**首次开箱**，主控台顶部会明确提示「默认密码 123456，请修改」。
///
/// # 安全说明（必须诚实说明）
/// 默认密码是公开的，**它不提供任何保护强度**。它的作用只是让软件开箱可用，
/// 真正的安全来自用户把它改成自己的密码。界面上会一直提示这一点，
/// 直到用户改掉为止。
pub const DEFAULT_PASSWORD: &str = "123456";

/// 生成「默认密码」对应的（派生值, 盐）。
fn default_password_credentials() -> (String, String) {
    let salt = crate::password::generate_salt_hex();
    if salt.is_empty() {
        // 随机源异常（几乎不可能）：返回空凭据，让上层走"未设置密码"路径，
        // 而不是存一个可预测的假盐。
        return (String::new(), String::new());
    }
    let hash = crate::password::derive_password_hex(DEFAULT_PASSWORD, &salt);
    (hash, salt)
}

/// 判断配置里的密码是不是「出厂默认密码」。
///
/// 用途：主控台的提示文案（默认密码要一直提醒用户改），以及允许用户在
/// 默认密码状态下直接打开主控台（否则第一次使用会卡在"要密码"，
/// 而用户此时根本还不知道默认密码是哪来的）。
pub fn is_default_password(hash: &str, salt: &str) -> bool {
    if hash.is_empty() || salt.is_empty() {
        return false;
    }
    crate::password::derive_password_hex(DEFAULT_PASSWORD, salt) == hash
}

fn default_version() -> u32 {
    1
}
fn default_grace_seconds() -> u64 {
    300 // 默认 5 分钟
}

impl Default for Config {
    fn default() -> Self {
        // ★ 必须先算好一对（盐 + 派生值）再写进结构体。
        // 【踩坑提醒】如果在这里分别调用两次生成函数，会拿到**两个不同的盐**，
        // 导致"用 A 盐算的哈希配 B 盐存储"——密码永远验证不通过。
        let (pwd_hash, pwd_salt) = default_password_credentials();
        Self {
            version: 1,
            // 默认给一个示例，方便用户第一眼看到效果（Phase 4 可删）。
            apps: vec![ProtectedApp {
                file_name: "notepad.exe".to_string(),
                display_name: "记事本".to_string(),
                full_path: String::new(),
                enabled: true,
            }],
            grace_mode: GraceMode::Once,
            grace_seconds: 300,
            relock_on_session_change: true,
            autostart: false,
            // 默认就用出厂密码：装好即用，用户随后可在主控台改成自己的密码。
            master_password_hash: pwd_hash,
            master_password_salt: pwd_salt,
            // 默认走「自设密码」（见 default_auth_method 的说明）。
            auth_method: default_auth_method(),
            // 默认启用「打开前验证」——装了锁就上锁。
            guard_enabled: true,
            // 默认不做运行中遮罩：本产品只做启动前验证。
            overlay_enabled: false,
            // 默认未激活：首次启动会弹授权码激活框（见 license.rs）。
            license_activated: false,
        }
    }
}

impl Config {
    /// 判断某个 exe 文件名（小写）是否在受保护列表中且已启用。
    pub fn is_protected(&self, file_name_lower: &str) -> bool {
        self.apps
            .iter()
            .any(|a| a.enabled && a.file_name.eq_ignore_ascii_case(file_name_lower))
    }

    /// 取友好名称（找不到就返回原名）。
    pub fn display_name_of(&self, file_name_lower: &str) -> String {
        self.apps
            .iter()
            .find(|a| a.file_name.eq_ignore_ascii_case(file_name_lower))
            .map(|a| a.display_name.clone())
            .unwrap_or_else(|| file_name_lower.to_string())
    }
}

/// 配置文件路径：`%APPDATA%\HelloGate\config.dat`
///
/// 放在用户漫游目录下，符合 Windows 应用惯例（配置跟随用户，不污染安装目录）。
pub fn config_path() -> PathBuf {
    // %APPDATA% 一般是 C:\Users\<用户名>\AppData\Roaming
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("HelloGate").join("config.dat")
}

/// 系统级备份路径：`%ProgramData%\HelloGate\config.dat`。
///
/// 【为什么要有备份】主配置放在 %APPDATA%（用户自己的目录），被保护的用户
/// （比如家里的小孩）随手就能删掉它——配置一没，保护列表就回退成默认，
/// 锁等于被绕过了。而 ProgramData 下的文件由管理员进程创建后，普通用户
/// **只读、不可删改**（Windows 默认 ACL），正好用来放一份"删不掉的底账"。
/// 读取时主配置缺失/损坏就回落到这份备份，删主配置无法解除保护。
///
/// 【诚实声明】这套备份防的是"删文件、改文件"这类手滑与低强度篡改；
/// 同一 Windows 账户内拥有代码执行能力的攻击者仍可用自己的 DPAPI 伪造配置
/// （DPAPI 绑定的是账户不是进程）。要对抗这种强度需要完整性校验 + 提权读取，
/// 属于后续版本的范畴（见 README 安全说明）。
fn backup_path() -> Option<PathBuf> {
    let base = std::env::var_os("ProgramData")?;
    Some(PathBuf::from(base).join("HelloGate").join("config.dat"))
}

/// 旧品牌名遗留路径，仅用于一次性迁移：`%APPDATA%\WinGate\config.dat`。
///
/// 【为什么存在】本项目原名 WinGate，后改名 HelloGate，配置目录随之从
/// `WinGate` 变成 `HelloGate`。老用户机器上的既有配置还躺在旧目录里，
/// 加载时要先把旧文件迁到新目录，用户才不会"一觉醒来配置没了"。
/// 这里的字面量 `"WinGate"` 是**故意保留**的旧路径，不是品牌替换遗漏。
fn old_appdata_config_path() -> PathBuf {
    // %APPDATA% 一般是 C:\Users\<用户名>\AppData\Roaming
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("WinGate").join("config.dat")
}

/// 旧品牌名遗留路径，仅用于一次性迁移：`%ProgramData%\WinGate\config.dat`
/// （系统级备份的旧位置，保留理由同 `old_appdata_config_path`）。
fn old_programdata_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("ProgramData")?;
    Some(PathBuf::from(base).join("WinGate").join("config.dat"))
}

/// 读取配置。主配置缺失/损坏时自动回落到系统级备份；都不可用才用默认配置。
///
/// # 为什么这样设计
/// - 保证程序总能启动（配置损坏不该让安全工具本身瘫痪）；
/// - 但**不能**让"删掉配置文件"变成解除保护的手段：主配置没了/坏了，
///   先去 ProgramData 找管理员写下的备份，找到就用它（顺带把主配置补回来），
///   两处都拿不到才回退默认，并写日志说明。
pub fn load() -> Config {
    // ★ 品牌改名一次性迁移（WinGate → HelloGate）：
    // 老用户的配置还在旧品牌名目录里，这里先把"旧有新无"的文件复制过来，
    // 再走正常加载。选择**复制而不是移动**：保留旧目录原件作回退——万一新
    // 版本有问题，用户退回旧版还能找回原配置。
    // 复制失败不致命：只记日志，照常走原有「主配置 → 备份 → 默认」加载链。
    let new_main = config_path();
    let old_main = old_appdata_config_path();
    if !new_main.exists() && old_main.exists() {
        // 先确保新目录 %APPDATA%\HelloGate 存在（复制不会自动建目录）
        if let Some(dir) = new_main.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::copy(&old_main, &new_main) {
            Ok(_) => logln!(
                "[config] 品牌迁移：已复制旧配置 {} → {}",
                old_main.display(),
                new_main.display()
            ),
            Err(e) => logln!("[config] 品牌迁移：复制旧主配置失败（不致命，继续正常加载）: {e}"),
        }
    }
    // 系统级备份同理：新备份缺失且旧 %ProgramData%\WinGate\config.dat 存在时复制过来
    if let Some(new_bak) = backup_path() {
        if let Some(old_bak) = old_programdata_config_path() {
            if !new_bak.exists() && old_bak.exists() {
                if let Some(dir) = new_bak.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                match std::fs::copy(&old_bak, &new_bak) {
                    Ok(_) => logln!(
                        "[config] 品牌迁移：已复制旧备份 {} → {}",
                        old_bak.display(),
                        new_bak.display()
                    ),
                    Err(e) => {
                        logln!("[config] 品牌迁移：复制旧系统级备份失败（不致命）: {e}")
                    }
                }
            }
        }
    }

    let mut cfg = load_inner();

    // ★ 迁移：老配置里没有主密码（字段为空）→ 自动补上出厂默认密码，
    // 并把验证方式切到「自设密码」。
    //
    // 【为什么要在加载时做】用户要求"默认设置密码"。旧版本的用户配置里
    // 密码是空的，只在 Config::default 里加默认值对他们不起作用——
    // 加载时补一次，立刻就能用，不需要重装/删配置。
    // 之后用户在主控台改了密码，这个分支就再也不会触发。
    if cfg.master_password_hash.is_empty() || cfg.master_password_salt.is_empty() {
        let (h, s) = default_password_credentials();
        if !h.is_empty() {
            cfg.master_password_hash = h;
            cfg.master_password_salt = s;
            cfg.auth_method = "password".to_string();
            logln!(
                "[config] 检测到未设置主密码 → 已启用默认密码 {DEFAULT_PASSWORD}（请尽快在主控台修改）"
            );
        }
    }
    cfg
}

/// 配置加载的实际实现（读主配置 → 读备份 → 回退默认）。
fn load_inner() -> Config {
    let path = config_path();

    // 1. 先试主配置。注意"文件存在但解密/解析失败"也要继续走备份——
    //    存在却坏了往往意味着被人动过手脚，此时更该信管理员写的备份。
    if let Ok(blob) = std::fs::read(&path) {
        match decrypt(&blob).and_then(|plain| {
            serde_json::from_slice::<Config>(&plain)
                .map_err(|e| format!("配置 JSON 解析失败: {e}"))
        }) {
            Ok(cfg) => return cfg,
            Err(e) => {
                logln!("[config] 警告：主配置读取失败（{e}），尝试系统级备份 ...");
            }
        }
    }

    // 2. 主配置缺失或损坏：回落到 ProgramData 备份。
    if let Some(bak) = backup_path() {
        if let Ok(blob) = std::fs::read(&bak) {
            match decrypt(&blob).and_then(|plain| {
                serde_json::from_slice::<Config>(&plain)
                    .map_err(|e| format!("备份 JSON 解析失败: {e}"))
            }) {
                Ok(cfg) => {
                    logln!(
                        "[config] 主配置缺失/损坏，已从系统级备份恢复（{} 个受保护应用）",
                        cfg.apps.len()
                    );
                    // 顺手把主配置补回来，让之后的读取路径恢复正常
                    // （失败不致命，下次启动还会走这条恢复路径）。
                    let _ = save(&cfg);
                    return cfg;
                }
                Err(e) => logln!("[config] 警告：系统级备份也无法读取（{e}）"),
            }
        }
    }

    // 3. 两处都拿不到：回退默认配置。
    //    只有主配置"确实存在过"时才大声报警（首次运行没有配置属正常情况）。
    if path.exists() {
        logln!("[config] 警告：主配置与系统级备份均不可用，已回退到默认配置");
    }
    Config::default()
}

/// 保存配置（先序列化，再 DPAPI 加密，最后落盘）。
pub fn save(cfg: &Config) -> Result<(), String> {
    let path = config_path();

    // 1. 确保目录存在（%APPDATA%\HelloGate）
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建配置目录失败: {e}"))?;
    }

    // 2. 序列化成 JSON 明文
    let plain = serde_json::to_vec_pretty(cfg).map_err(|e| format!("配置序列化失败: {e}"))?;

    // 3. DPAPI 加密
    let blob = encrypt(&plain)?;

    // 4. 原子写入：先写临时文件再改名，避免写到一半崩溃导致配置损坏
    let tmp = path.with_extension("dat.tmp");
    std::fs::write(&tmp, &blob).map_err(|e| format!("写入临时配置失败: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("提交配置失败: {e}"))?;

    // 5. 顺带写一份系统级备份（ProgramData，普通用户删不掉）。
    //    尽力而为：写不了也不影响主配置生效，但要记日志方便排查
    //    "为什么备份没生成"（典型原因：ProgramData 目录 ACL 异常）。
    if let Some(bak) = backup_path() {
        if let Some(dir) = bak.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&bak, &blob) {
            logln!("[config] 警告：系统级备份写入失败: {e}");
        }
    }

    Ok(())
}

// ============================================================================
// DPAPI 加解密（薄封装）
// ============================================================================

/// 用 DPAPI 加密一段明文，返回加密后的 blob。
fn encrypt(plain: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        // 输入：明文数据块（DPAPI 用 CRYPT_INTEGER_BLOB 承载任意二进制）
        let input = CRYPT_INTEGER_BLOB {
            cbData: plain.len() as u32,
            pbData: plain.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();

        // 参数说明：
        // - 第 1 个 None：不额外附加描述文本（加密后不可见，这里不需要）
        // - 第 2 个 None：不使用额外熵（entropy）。若想更安全可传一段固定熵，
        //   但那样配置就与本程序强绑定，迁移更麻烦；PoC 阶段保持简单。
        // - 第 3 个 None：不使用保留参数
        // - 第 4 个 None：不使用提示信息结构
        // - 0：默认标志（密钥绑定当前用户）
        // - &mut output：接收加密结果
        CryptProtectData(&input, None, None, None, None, 0, &mut output)
            .map_err(|e| format!("DPAPI 加密失败: {e}"))?;

        // 把系统分配的输出拷进 Rust 的 Vec，然后释放系统内存
        let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(output.pbData as *mut _)));
        Ok(result)
    }
}

/// 用 DPAPI 解密一段 blob，返回明文。
fn decrypt(blob: &[u8]) -> Result<Vec<u8>, String> {
    unsafe {
        let input = CRYPT_INTEGER_BLOB {
            cbData: blob.len() as u32,
            pbData: blob.as_ptr() as *mut u8,
        };
        let mut output = CRYPT_INTEGER_BLOB::default();

        // 第 2 个参数为输出描述文本指针，这里传 None 表示不关心。
        CryptUnprotectData(&input, None, None, None, None, 0, &mut output)
            .map_err(|e| format!("DPAPI 解密失败（配置可能来自其他用户/机器）: {e}"))?;

        let result = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(output.pbData as *mut _)));
        Ok(result)
    }
}

// 保留 PCWSTR 的显式引用，避免未使用导入告警（DPAPI 的某些重载会用到宽字符串）。
#[allow(dead_code)]
fn _anchor(_: PCWSTR) {}
