//! 会话注册表：谁是被监控的 agent 根进程，以及"启动窗口"保护的进程集合。
//!
//! **按 pid 索引，按 project_id 分组**：一个 project 可能同时有多个 agent 进程
//! （并发上限配置允许），按 project 做 key 会互相覆盖，导致先注册的会话失去保护。
//! `mark_session_active(project)` 会封窗该 project 下的所有根进程。
//!
//! 受保护集合记录的是 `(pid, starttime)`：只用 pid 会让「pid 被复用后的新进程」
//! 继承保护，从而永远杀不掉。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Instant;

use super::proc_tree::{ProcInfo, descendants};
use super::sys;

/// 取 `root` 所有后代的 `(pid, starttime)`，用作启动窗口快照。
fn descendant_births(table: &HashMap<u32, ProcInfo>, root: u32) -> Vec<(u32, u64)> {
    descendants(table, root)
        .into_iter()
        .filter_map(|pid| table.get(&pid).map(|info| (pid, info.starttime)))
        .collect()
}

/// 一个被监控的 agent 会话。
pub struct Root {
    pub project: String,
    pub pid: u32,
    /// `/proc/<pid>/stat` 的 starttime，用于识别 PID 复用。
    pub born: u64,
    /// root 的 exe（`/proc/<pid>/exe`）。
    pub exe: PathBuf,
    /// root 的 `cmdline[1]` 的 basename，例如 `claude-code-acp-ts`；
    /// `cmdline[1]` 是参数（以 `-` 开头）时为空。
    pub script_hint: String,
    pub spawned_at: Instant,
    sealed: AtomicBool,
    /// pid → starttime；只有 starttime 一致才算"受保护"。
    protected: Mutex<HashMap<u32, u64>>,
}

impl Root {
    pub fn new(
        project: impl Into<String>,
        pid: u32,
        born: u64,
        exe: PathBuf,
        script_hint: String,
    ) -> Self {
        Self {
            project: project.into(),
            pid,
            born,
            exe,
            script_hint,
            spawned_at: Instant::now(),
            sealed: AtomicBool::new(false),
            protected: Mutex::new(HashMap::new()),
        }
    }

    pub fn is_sealed(&self) -> bool {
        self.sealed.load(Ordering::Relaxed)
    }

    /// 把一批进程（pid + starttime）加入永久保护。
    pub fn protect(&self, pids: impl IntoIterator<Item = (u32, u64)>) {
        let mut guard = self.protected.lock().unwrap_or_else(|e| e.into_inner());
        guard.extend(pids);
    }

    pub fn protected(&self) -> HashMap<u32, u64> {
        self.protected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 该 pid 是否受保护；**starttime 必须一致**，否则说明 pid 已被复用。
    pub fn is_protected(&self, pid: u32, starttime: u64) -> bool {
        self.protected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&pid)
            .is_some_and(|born| *born == starttime)
    }

    /// 封窗：先加入最终快照，再翻转标志（顺序不能反，否则有竞态）。
    pub fn seal_with(&self, snapshot: impl IntoIterator<Item = (u32, u64)>) {
        self.protect(snapshot);
        self.sealed.store(true, Ordering::Release);
    }

    /// 规则 4/5：`exe` 等于 root 的 exe，或路径里包含 root 的脚本名。
    pub fn matches_exe(&self, exe: &Path) -> bool {
        if !self.exe.as_os_str().is_empty() && exe == self.exe {
            return true;
        }
        !self.script_hint.is_empty()
            && exe
                .to_str()
                .is_some_and(|text| text.contains(&self.script_hint))
    }
}

/// 会话注册表（pid → Root）。
#[derive(Default)]
pub struct Registry {
    roots: RwLock<HashMap<u32, Arc<Root>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// spawn 后注册。读取 `/proc` 失败（进程秒退）返回 `None`。
    pub fn register(&self, project: &str, pid: u32) -> Option<Arc<Root>> {
        let info = sys::read_pid(pid)?;
        let exe = sys::read_exe(pid).unwrap_or_default();
        let script_hint = sys::read_cmdline(pid)
            .get(1)
            .filter(|arg| !arg.starts_with('-'))
            .and_then(|arg| Path::new(arg).file_name())
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();

        let root = Arc::new(Root::new(project, pid, info.starttime, exe, script_hint));
        let mut guard = self.roots.write().unwrap_or_else(|e| e.into_inner());
        guard.insert(pid, Arc::clone(&root));
        Some(root)
    }

    /// 测试与无 `/proc` 场景使用。
    pub fn insert(&self, root: Root) -> Arc<Root> {
        let root = Arc::new(root);
        let mut guard = self.roots.write().unwrap_or_else(|e| e.into_inner());
        guard.insert(root.pid, Arc::clone(&root));
        root
    }

    /// 会话结束：只移除该 pid（同 project 的其它并发会话不受影响）。
    pub fn unregister(&self, project: &str, pid: u32) {
        let mut guard = self.roots.write().unwrap_or_else(|e| e.into_inner());
        if guard.get(&pid).is_some_and(|root| root.project == project) {
            guard.remove(&pid);
        }
    }

    pub fn get(&self, project: &str) -> Option<Arc<Root>> {
        self.roots
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .find(|root| root.project == project)
            .cloned()
    }

    pub fn get_pid(&self, pid: u32) -> Option<Arc<Root>> {
        self.roots
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&pid)
            .cloned()
    }

    pub fn all(&self) -> Vec<Arc<Root>> {
        self.roots
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    pub fn root_pids(&self) -> Vec<u32> {
        self.all().iter().map(|root| root.pid).collect()
    }

    /// 未封窗的会话：把当前后代加入保护（由 sampler 每 tick 调用）。
    ///
    /// 返回新增的 pid 数（用于日志/测试）。
    pub fn snapshot_bootstrap(&self, table: &HashMap<u32, ProcInfo>) -> usize {
        let mut added = 0usize;
        for root in self.all() {
            if root.is_sealed() {
                continue;
            }
            let before = root.protected().len();
            root.protect(descendant_births(table, root.pid));
            added += root.protected().len().saturating_sub(before);
        }
        added
    }

    /// 封窗某个 project 下**所有**未封窗的会话。
    ///
    /// 为什么必须同步快照：首条消息可能落在两次采样之间，此时 agent 的子进程
    /// 还没被任何 tick 看到；不补这一次，它会在窗口关闭后立刻变成可杀候选。
    pub fn seal_project(&self, project: &str, table: &HashMap<u32, ProcInfo>) -> usize {
        let mut sealed = 0usize;
        for root in self.all() {
            if root.project != project || root.is_sealed() {
                continue;
            }
            root.seal_with(descendant_births(table, root.pid));
            sealed += 1;
        }
        sealed
    }

    /// 清理已退出或被复用的 root。返回清理数量。
    pub fn prune_dead(&self, table: &HashMap<u32, ProcInfo>) -> usize {
        let mut guard = self.roots.write().unwrap_or_else(|e| e.into_inner());
        let before = guard.len();
        guard.retain(|_, root| {
            table
                .get(&root.pid)
                .is_some_and(|info| info.starttime == root.born)
        });
        before - guard.len()
    }
}

static GLOBAL: OnceLock<Arc<Registry>> = OnceLock::new();

/// 安装全局注册表（`agent_runner_run()` 启动看门狗时调用一次）。
pub fn install() -> Arc<Registry> {
    GLOBAL.get_or_init(|| Arc::new(Registry::new())).clone()
}

pub fn global() -> Option<Arc<Registry>> {
    GLOBAL.get().cloned()
}

/// spawn 后调用；未安装注册表（其它二进制/测试）时静默 no-op。
pub fn try_register(project: &str, pid: u32) {
    let Some(registry) = global() else {
        return;
    };
    if registry.register(project, pid).is_none() {
        // 进程可能在 register 前就退出；只影响该会话，不打断 spawn。
        tracing::debug!(event = "mem_watch.register_skipped", project, pid);
    }
}

/// 会话结束时调用；未安装注册表时 no-op。
pub fn try_unregister(project: &str, pid: u32) {
    if let Some(registry) = global() {
        registry.unregister(project, pid);
    }
}

/// 首条用户消息到达时调用；未安装注册表时 no-op。
pub fn mark_session_active(project: &str) {
    let Some(registry) = global() else {
        return;
    };
    // 已封窗就不必再扫 /proc（`process_prompt` 每次都会调这里）
    let pending = registry
        .all()
        .into_iter()
        .any(|root| root.project == project && !root.is_sealed());
    if !pending {
        return;
    }
    let table = sys::read_all();
    registry.seal_project(project, &table);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(pid: u32, ppid: u32, starttime: u64) -> ProcInfo {
        ProcInfo {
            pid,
            ppid,
            comm: format!("p{pid}"),
            rss_bytes: 0,
            starttime,
        }
    }

    fn table() -> HashMap<u32, ProcInfo> {
        let mut t = HashMap::new();
        t.insert(100, info(100, 1, 1000)); // root
        t.insert(101, info(101, 100, 1001)); // agent 本体
        t.insert(102, info(102, 101, 1002)); // 命令
        t
    }

    fn root_of(project: &str, pid: u32, born: u64) -> Root {
        Root::new(
            project,
            pid,
            born,
            PathBuf::from("/usr/local/bin/node"),
            "claude-code-acp-ts".into(),
        )
    }

    #[test]
    fn bootstrap_snapshot_protects_descendants() {
        let registry = Registry::new();
        registry.insert(root_of("p1", 100, 1000));

        assert!(registry.snapshot_bootstrap(&table()) >= 2);

        let root = registry.get("p1").unwrap();
        assert!(root.is_protected(101, 1001));
        assert!(root.is_protected(102, 1002));
        assert!(!root.is_protected(100, 1000), "root 自己不进保护集");
    }

    #[test]
    fn protection_is_bound_to_starttime() {
        let registry = Registry::new();
        registry.insert(root_of("p1", 100, 1000));
        registry.snapshot_bootstrap(&table());

        let root = registry.get("p1").unwrap();
        // 同一个 pid、不同 starttime ⇒ 是复用后的新进程，不能被保护
        assert!(!root.is_protected(101, 9999));
    }

    #[test]
    fn seal_takes_final_snapshot_before_closing() {
        let registry = Registry::new();
        registry.insert(root_of("p1", 100, 1000));

        // 模拟：首条消息在两次采样之间到达，之前一次快照都没做
        let mut late = table();
        late.insert(103, info(103, 100, 1003));
        assert_eq!(registry.seal_project("p1", &late), 1);

        let root = registry.get("p1").unwrap();
        assert!(root.is_sealed());
        assert!(
            root.is_protected(103, 1003),
            "封窗前的同步快照必须包含新进程"
        );
    }

    #[test]
    fn snapshot_stops_after_seal() {
        let registry = Registry::new();
        registry.insert(root_of("p1", 100, 1000));
        registry.seal_project("p1", &table());

        let mut later = table();
        later.insert(104, info(104, 100, 1004));
        assert_eq!(
            registry.snapshot_bootstrap(&later),
            0,
            "封窗后不再吸收新进程"
        );
    }

    #[test]
    fn seals_only_the_requested_project() {
        let registry = Registry::new();
        registry.insert(root_of("p1", 100, 1000));
        registry.insert(root_of("p2", 200, 2000));

        let mut t = table();
        t.insert(200, info(200, 1, 2000));
        assert_eq!(registry.seal_project("p1", &t), 1);

        assert!(registry.get("p1").unwrap().is_sealed());
        assert!(
            !registry.get("p2").unwrap().is_sealed(),
            "其它 project 不能被误封"
        );
    }

    #[test]
    fn concurrent_sessions_of_same_project_are_independent() {
        let registry = Registry::new();
        registry.insert(root_of("p1", 100, 1000));
        registry.insert(root_of("p1", 200, 2000)); // 同 project 的第二个 agent

        let mut t = table();
        t.insert(200, info(200, 1, 2000));
        assert_eq!(
            registry.root_pids().len(),
            2,
            "同 project 多会话不能互相覆盖"
        );

        registry.unregister("p1", 200);
        assert!(registry.get_pid(100).is_some());
        assert!(registry.get_pid(200).is_none());
    }

    #[test]
    fn matches_exe_covers_sibling_binary() {
        let root = root_of("p1", 100, 1000);
        assert!(root.matches_exe(Path::new("/usr/local/bin/node")));
        assert!(root.matches_exe(Path::new(
            "/usr/local/lib/node_modules/claude-code-acp-ts/node_modules/x/claude"
        )));
        assert!(!root.matches_exe(Path::new("/usr/bin/python3")));
    }

    #[test]
    fn prune_removes_reused_or_dead_roots() {
        let registry = Registry::new();
        registry.insert(root_of("dead", 200, 5000));
        registry.insert(root_of("reused", 201, 6000));
        registry.insert(root_of("alive", 202, 7000));

        let mut t = HashMap::new();
        t.insert(201, info(201, 1, 9999)); // starttime 变了 ⇒ 复用
        t.insert(202, info(202, 1, 7000)); // 正常

        assert_eq!(registry.prune_dead(&t), 2);
        assert!(registry.get_pid(202).is_some());
        assert!(registry.get_pid(200).is_none());
        assert!(registry.get_pid(201).is_none());
    }
}
