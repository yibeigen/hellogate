//! 状态机模块（Phase 4）
//!
//! 职责：记录「哪些应用当前处于已解锁（宽限期）状态」，并处理宽限期到期、
//! 锁屏/睡眠唤醒后的自动重锁。
//!
//! 对应 DESIGN_AND_ROADMAP.md §4「智能状态机」与 §3.3「双模宽限期」。
//!
//! # 核心概念
//! - **锁定（Locked）**：应用切到前台时，弹出遮罩要求验证。
//! - **已解锁（Unlocked）**：用户刚验证过，在宽限期内切回该应用**不再**弹遮罩。
//! - **宽限期（Grace Period）**：解锁后的免验证时长，有三种模式：
//!   - `Once`：仅本次，切走即失效（最严格）
//!   - `Timed`：N 分钟内有效
//!   - `Session`：本次登录会话内一直有效（最宽松）
//! - **重锁（Relock）**：锁屏 / 睡眠唤醒后，清空所有已解锁记录，回到全锁定状态。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::config::GraceMode;

/// 单个应用在内存中的解锁状态。
#[derive(Debug, Clone)]
struct UnlockRecord {
    /// 解锁发生的时刻（用于 `Timed` 模式计算是否过期）。
    unlocked_at: Instant,
    /// 解锁那一刻的进程 ID（PID）。
    ///
    /// 【为什么必须记 PID】
    /// 宽限期的本意是豁免「切走再切回来」这同一个窗口，而不是豁免一切。
    /// 如果只按文件名记状态，用户把程序**关掉再重开**（比如微信退出后再登录），
    /// 新开的进程也会落在宽限期内直接放行——锁就形同虚设了。
    /// 记下 PID 后：重开 = 新进程 = 新 PID ≠ 记录里的 PID → 判定未解锁，重新验证。
    pid: u32,
}

/// 状态机：管理所有应用的解锁记录。
pub struct StateMachine {
    /// key = 小写 exe 文件名，value = 解锁记录。
    /// 只有「当前处于宽限期」的应用才会出现在这里。
    unlocked: HashMap<String, UnlockRecord>,
    /// 宽限期模式（来自配置）。
    mode: GraceMode,
    /// 时间型宽限期的时长（秒）。
    grace_seconds: u64,
}

impl StateMachine {
    /// 创建状态机。
    pub fn new(mode: GraceMode, grace_seconds: u64) -> Self {
        Self {
            unlocked: HashMap::new(),
            mode,
            grace_seconds,
        }
    }

    /// 用新配置刷新状态机的策略（用户改了设置后调用）。
    pub fn reconfigure(&mut self, mode: GraceMode, grace_seconds: u64) {
        self.mode = mode;
        self.grace_seconds = grace_seconds;
    }

    /// 标记某个应用「刚刚解锁成功」，并绑定到当前这个进程实例（PID）。
    ///
    /// `Once` 模式同样要写记录：它的语义是「本次前台停留期间有效，切走即失效」，
    /// 切走的清理由 `on_leave` 负责；写入记录是为了让「正停留在该应用」
    /// 期间反复收到的前台事件不重复弹遮罩。
    pub fn mark_unlocked(&mut self, file_name_lower: &str, pid: u32) {
        self.unlocked.insert(
            file_name_lower.to_string(),
            UnlockRecord {
                unlocked_at: Instant::now(),
                pid,
            },
        );
    }

    /// 查询某个应用当前是否处于宽限期（免验证）。
    ///
    /// # 参数
    /// - `file_name_lower`：小写 exe 文件名。
    /// - `pid`：该应用**此刻**的进程 ID（从前台窗口现查的）。
    ///
    /// # 返回
    /// - `true`：在宽限期内且是解锁时的同一个进程实例，无需弹遮罩。
    /// - `false`：需要验证（包括「关闭后重开的新实例」这种情况）。
    pub fn is_unlocked(&self, file_name_lower: &str, pid: u32) -> bool {
        let Some(rec) = self.unlocked.get(file_name_lower) else {
            return false;
        };

        // PID 对不上 = 程序被关掉后重新打开了。
        // 新进程和当初解锁的那个毫无关系，宽限期不能延续给它。
        if rec.pid != pid {
            return false;
        }

        match self.mode {
            // 一次性：只要还在记录里就算解锁（记录会在切走时被 consume 清除）
            GraceMode::Once => true,
            // 时间型：检查是否超过 grace_seconds
            GraceMode::Timed => rec.unlocked_at.elapsed() < Duration::from_secs(self.grace_seconds),
            // 会话型：始终有效，直到重锁
            GraceMode::Session => true,
        }
    }

    /// 「消费」一次性宽限期：当用户从受保护应用**切走**时调用。
    ///
    /// 对于 `Once` 模式，切走即失效，下次回来需要重新验证。
    /// 其他模式不受影响。
    pub fn on_leave(&mut self, file_name_lower: &str) {
        if self.mode == GraceMode::Once {
            self.unlocked.remove(file_name_lower);
        }
    }

    /// 重锁：清空所有解锁记录（锁屏 / 睡眠唤醒 / 手动重锁时调用）。
    pub fn relock_all(&mut self) {
        self.unlocked.clear();
    }

    /// 清理已过期的记录（时间型模式下的定期维护，避免 map 无限增长）。
    pub fn purge_expired(&mut self) {
        if self.mode != GraceMode::Timed {
            return;
        }
        let ttl = Duration::from_secs(self.grace_seconds);
        self.unlocked.retain(|_, rec| rec.unlocked_at.elapsed() < ttl);
    }
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new(GraceMode::Once, 300)
    }
}
