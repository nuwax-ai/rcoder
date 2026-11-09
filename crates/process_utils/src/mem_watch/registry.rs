//! 会话注册表：谁是被监控的 agent 根进程，以及"启动窗口"保护的进程集合。
//!
//! 按 `project_id` 索引（spawn 时必有，`process_prompt` 处也能拿到），
//! 而不是按 pid——否则封窗钩子挂不上。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Instant;

use super::proc_tree::{ProcInfo, descendants};
use super::sys;

/// 一个被监控的 agent 会话。
pub struct Root {
    pub project: String,
    pub pid: u32,
    /// `/proc/<pid>/stat` 的 starttime，用于识别 PID 复用。
    pub born: u64,
    /// root 的 exe（`/proc/<pid>/exe`）。
    pub exe: PathBuf,
    /// root 的 `cmdline[1]` 的 basename，例如 `claude-code-acp-ts`。
    pub script_hint: String,
    pub spawned_at: Instant,
    sealed: AtomicBool,
    protected: Mutex<HashSet<u32>>,
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
            protected: Mutex::new(HashSet::new()),
        }
    }

    pub fn is_sealed(&self) -> bool {
        self.sealed.load(Ordering::Relaxed)
    }

    /// 把一批进程加入永久保护（启动窗口快照）。
    pub fn protect(&self, pids: impl IntoIterator<Item = u32>) {
        let mut guard = self.protected.lock().unwrap_or_else(|e| e.into_inner());
        guard.extend(pids);
    }

    pub fn protected(&self) -> HashSet<u32> {
        self.protected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 封窗：先加入最终快照，再翻转标志（顺序不能反，否则有竞态）。
    pub fn seal_with(&self, snapshot: impl IntoIterator<Item = u32>) {
        self.protect(snapshot);
        self.sealed.store(true, Ordering::Release);
    }

    /// 规则 4/5：`exe` 等于 root 的 exe，或路径里包含 root 的脚本名。
    pub fn matches_exe(&self, exe: &Path) -> bool {
        if exe == self.exe {
            return true;
        }
        if !self.script_hint.is_empty() {
            if let Some(text) = exe.to_str() {
                return text.contains(&self.script_hint);
            }
        }
        false
    }
}

/// 会话注册表。
#[derive(Default)]
pub struct Registry {
    roots: RwLock<HashMap<String, Arc<Root>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// spawn 后注册。读取 `/proc` 失败（进程秒退）返回 `None`。
    pub fn register(&self, project: &str, pid: u32) -> Option<Arc<Root>> {
        let info = sys::read_pid(pid)?;
        let exe = sys::read_exe(pid).unwrap_or_default();
        let argv = sys::read_cmdline(pid);
        let script_hint = argv
            .get(1)
            .and_then(|arg| Path::new(arg).file_name())
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();

        let root = Arc::new(Root::new(project, pid, info.starttime, exe, script_hint));
        let mut guard = self.roots.write().unwrap_or_else(|e| e.into_inner());
        guard.insert(project.to_owned(), Arc::clone(&root));
        Some(root)
    }

    /// 测试与无 `/proc` 场景使用。
    pub fn insert(&self, root: Root) -> Arc<Root> {
        let root = Arc::new(root);
        let mut guard = self.roots.write().unwrap_or_else(|e| e.into_inner());
        guard.insert(root.project.clone(), Arc::clone(&root));
        root
    }

    pub fn unregister(&self, project: &str) {
        let mut guard = self.roots.write().unwrap_or_else(|e| e.into_inner());
        guard.remove(project);
    }

    pub fn get(&self, project: &str) -> Option<Arc<Root>> {
        self.roots
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(project)
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
    pub fn snapshot_bootstrap(&self, table: &HashMap<u32, ProcInfo>) -> usize {
        let mut added = 0usize;
        for root in self.all() {
            if root.is_sealed() {
                continue;
            }
            let before = root.protected().len();
            root.protect(descendants(table, root.pid));
            added += root.protected().len().saturating_sub(before);
        }
        added
    }

    /// 封窗：用当前进程表做**同步最终快照**，然后关闭窗口。
    ///
    /// 为什么必须同步快照：首条消息可能落在两次采样之间，此时 agent 的子进程
    /// 还没被任何 tick 看到；不补这一次，它会在窗口关闭后立刻变成可杀候选。
    pub fn seal(&self, project: &str, table: &HashMap<u32, ProcInfo>) -> bool {
        let Some(root) = self.get(project) else {
            return false;
        };
        if root.is_sealed() {
            return false;
        }
        root.seal_with(descendants(table, root.pid));
        true
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
    if let Some(registry) = global() {
        drop(registry.register(project, pid));
    }
}

/// 会话结束时调用；未安装注册表时 no-op。
pub fn try_unregister(project: &str) {
    if let Some(registry) = global() {
        registry.unregister(project);
    }
}

/// 首条用户消息到达时调用；未安装注册表时 no-op。
pub fn mark_session_active(project: &str) {
    let Some(registry) = global() else {
        return;
    };
    // 已封窗就不必再扫 /proc（`process_prompt` 每次都会调这里）
    match registry.get(project) {
        Some(root) if !root.is_sealed() => {}
        _ => return,
    }
    let table = sys::read_all();
    registry.seal(project, &table);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem_watch::proc_tree::ProcInfo;

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

    fn root(project: &str, pid: u32, born: u64) -> Root {
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
        registry.insert(root("p1", 100, 1000));

        let added = registry.snapshot_bootstrap(&table());
        assert!(added >= 2, "root 的后代应全部进保护集");

        let sealed = registry.get("p1").unwrap();
        let protected = sealed.protected();
        assert!(protected.contains(&101));
        assert!(protected.contains(&102));
        assert!(
            !protected.contains(&100),
            "root 自己不进保护集（另有规则排除）"
        );
    }

    #[test]
    fn seal_takes_final_snapshot_before_closing() {
        let registry = Registry::new();
        registry.insert(root("p1", 100, 1000));

        // 模拟：首条消息在两次采样之间到达，之前一次快照都没做
        let mut late = table();
        late.insert(103, info(103, 100, 1003)); // 消息到达瞬间刚起的进程
        assert!(registry.seal("p1", &late));

        let sealed = registry.get("p1").unwrap();
        assert!(sealed.is_sealed());
        assert!(
            sealed.protected().contains(&103),
            "封窗前的同步快照必须包含新进程"
        );
    }

    #[test]
    fn snapshot_stops_after_seal() {
        let registry = Registry::new();
        registry.insert(root("p1", 100, 1000));
        registry.seal("p1", &table());

        let mut later = table();
        later.insert(104, info(104, 100, 1004));
        assert_eq!(
            registry.snapshot_bootstrap(&later),
            0,
            "封窗后不再吸收新进程"
        );
    }

    #[test]
    fn matches_exe_covers_sibling_binary() {
        let r = root("p1", 100, 1000);
        assert!(r.matches_exe(Path::new("/usr/local/bin/node")));
        assert!(r.matches_exe(Path::new(
            "/usr/local/lib/node_modules/claude-code-acp-ts/node_modules/x/claude"
        )));
        assert!(!r.matches_exe(Path::new("/usr/bin/python3")));
    }

    #[test]
    fn prune_removes_reused_or_dead_roots() {
        let registry = Registry::new();
        registry.insert(root("dead", 200, 5000));
        registry.insert(root("reused", 201, 6000));
        registry.insert(root("alive", 202, 7000));

        let mut t = HashMap::new();
        t.insert(201, info(201, 1, 9999)); // starttime 变了 ⇒ 复用
        t.insert(202, info(202, 1, 7000)); // 正常

        assert_eq!(registry.prune_dead(&t), 2);
        assert!(registry.get("alive").is_some());
        assert!(registry.get("dead").is_none());
        assert!(registry.get("reused").is_none());
    }
}
