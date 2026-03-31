//! Session 文件扫描模块
//!
//! 扫描 ~/.claude/projects/ 目录，验证 session 文件是否存在
//! 作为官方 listSessions API 的降级方案
//!
//! 特性：
//! - 使用 moka 缓存扫描结果（TTL 3分钟兜底）
//! - 使用 notify 监听文件系统变化，自动刷新缓存
//! - 防抖机制：100ms 内的多个事件合并为一次刷新
//! - 文件变化时清除缓存，下次查询时重新扫描

use moka::future::Cache;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// 全局 session 文件存在性缓存
/// Key: project_path, Value: 该项目下的 session_id 集合
static FILE_SCAN_CACHE: OnceLock<Cache<String, HashSet<String>>> = OnceLock::new();

/// 文件监听器（全局单例）
static FILE_WATCHER: OnceLock<Mutex<Option<RecommendedWatcher>>> = OnceLock::new();

/// 监听器是否已启动
static WATCHER_STARTED: AtomicBool = AtomicBool::new(false);

/// 防抖时间（毫秒）
const DEBOUNCE_MS: u64 = 100;

/// 获取或初始化缓存（TTL 3分钟兜底，主要由文件监听触发刷新）
fn get_file_scan_cache() -> &'static Cache<String, HashSet<String>> {
    FILE_SCAN_CACHE.get_or_init(|| {
        Cache::builder()
            .time_to_live(Duration::from_secs(180)) // 3分钟兜底 TTL
            .build()
    })
}

/// Claude 配置目录
fn get_claude_config_dir() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/home/user"))
                .join(".claude")
        })
}

/// 获取 projects 目录路径
fn get_projects_dir() -> PathBuf {
    get_claude_config_dir().join("projects")
}

/// 编码项目路径为目录名
/// 规则：将 `/` 替换为 `-`
fn encode_project_path(project_path: &str) -> String {
    project_path.replace('/', "-")
}

/// 从文件路径中提取项目目录名
/// 例如：~/.claude/projects/-home-user-project/abc123.jsonl -> -home-user-project
fn extract_project_dir_from_path(file_path: &Path) -> Option<String> {
    let projects_dir = get_projects_dir();

    // 检查文件是否在 projects 目录下
    if !file_path.starts_with(&projects_dir) {
        return None;
    }

    // 获取相对于 projects 目录的路径
    if let Ok(relative) = file_path.strip_prefix(&projects_dir) {
        // 第一个组件就是项目目录名
        if let Some(first_component) = relative.components().next() {
            return Some(first_component.as_os_str().to_string_lossy().to_string());
        }
    }

    None
}

/// 扫描项目目录，获取所有 session_id（异步版本）
///
/// 使用 tokio::fs 异步 I/O，避免阻塞 Tokio 工作线程
async fn scan_project_sessions(project_path: &str) -> HashSet<String> {
    let projects_dir = get_projects_dir();
    let encoded_path = encode_project_path(project_path);

    let mut session_ids = HashSet::new();

    if !projects_dir.exists() {
        debug!(
            "🔍 [文件扫描] projects 目录不存在: {}",
            projects_dir.display()
        );
        return session_ids;
    }

    // 使用异步 I/O 遍历目录
    let Ok(mut dir_entries) = tokio::fs::read_dir(&projects_dir).await else {
        warn!(
            "🔍 [file message ] unable to message projectdirectory: {}",
            projects_dir.display()
        );
        return session_ids;
    };

    while let Ok(Some(entry)) = dir_entries.next_entry().await {
        let dir_name = entry.file_name();

        // 匹配编码后的路径（精确或带哈希后缀）
        // 使用 OsStr 避免不必要的 String 分配
        if dir_name
            .to_string_lossy()
            .starts_with(encoded_path.as_str())
        {
            let Ok(mut files) = tokio::fs::read_dir(entry.path()).await else {
                warn!(
                    "🔍 [file message ] unable to message sessiondirectory: {}",
                    entry.path().display()
                );
                continue;
            };

            while let Ok(Some(file)) = files.next_entry().await {
                if let Some(filename) = file.file_name().to_str() {
                    if filename.ends_with(".jsonl") {
                        // 提取 session_id（去掉 .jsonl 后缀）
                        let session_id = filename.trim_end_matches(".jsonl").to_string();
                        session_ids.insert(session_id);
                    }
                }
            }
        }
    }

    debug!(
        "🔍 [文件扫描] 扫描完成: project_path={}, 找到 {} 个 session",
        project_path,
        session_ids.len()
    );

    session_ids
}

/// 清除指定项目目录相关的缓存
///
/// 由于 moka 不支持按前缀遍历/删除，采用 invalidate_all 策略
/// 下次查询时会用原始 project_path 重新扫描
async fn invalidate_project_cache(project_dir_name: &str) {
    let cache = get_file_scan_cache();

    debug!(
        "🔄 [文件监听] 检测到项目目录变化: {}，清除缓存",
        project_dir_name
    );

    // 由于无法从目录名还原 project_path，直接清除所有缓存
    // 这是一个权衡：简单但可能影响其他项目的缓存
    // 实际影响较小，因为缓存会在下次查询时重建
    cache.invalidate_all();
}

/// 启动文件系统监听器
///
/// 监听 ~/.claude/projects/ 目录的变化，当 .jsonl 文件创建/删除时刷新缓存
pub fn start_file_watcher() {
    // 确保只启动一次
    if WATCHER_STARTED.swap(true, Ordering::SeqCst) {
        debug!("[filelisten] listen message already message ");
        return;
    }

    let projects_dir = get_projects_dir();

    // 创建目录（如果不存在）
    if !projects_dir.exists() {
        if let Err(e) = std::fs::create_dir_all(&projects_dir) {
            warn!("[filelisten] unable tocreated projects directory: {}", e);
            WATCHER_STARTED.store(false, Ordering::SeqCst);
            return;
        }
    }

    // 创建异步通道
    let (tx, mut rx) = mpsc::channel::<Event>(100);

    // 创建文件监听器
    let watcher_result = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        match res {
            Ok(event) => {
                // 只关心 .jsonl 文件的创建和删除
                let has_jsonl = event
                    .paths
                    .iter()
                    .any(|p| p.extension().is_some_and(|ext| ext == "jsonl"));

                if has_jsonl {
                    match event.kind {
                        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_) => {
                            if let Err(e) = tx.blocking_send(event) {
                                error!("[filelisten] send message failed: {}", e);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                error!("[filelisten] listenerror: {}", e);
            }
        }
    });

    let mut watcher = match watcher_result {
        Ok(w) => w,
        Err(e) => {
            error!("[filelisten] createdlisten message failed: {}", e);
            WATCHER_STARTED.store(false, Ordering::SeqCst);
            return;
        }
    };

    // 开始监听目录
    if let Err(e) = watcher.watch(&projects_dir, RecursiveMode::Recursive) {
        error!("[filelisten] listendirectoryfailed: {}", e);
        WATCHER_STARTED.store(false, Ordering::SeqCst);
        return;
    }

    // 保存 watcher 到全局变量（防止被 drop）
    let watcher_holder = FILE_WATCHER.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = watcher_holder.lock() {
        *guard = Some(watcher);
    }

    info!(
        "👁️ [filelisten] startinglistendirectory: {}",
        projects_dir.display()
    );

    // 启动异步任务处理事件（带防抖）
    tokio::spawn(async move {
        let mut pending_dirs: HashSet<String> = HashSet::new();
        let mut debounce_timer: Option<tokio::time::Instant> = None;

        loop {
            tokio::select! {
                           // 接收新事件
                           event = rx.recv() => {
                               match event {
                                   Some(event) => {
                                       // 提取变化文件所属的项目目录
                                       for path in &event.paths {
                                           if let Some(project_dir) = extract_project_dir_from_path(path) {
                                               pending_dirs.insert(project_dir);
                                           }
                                       }
                                       // 重置防抖计时器
                                       debounce_timer = Some(tokio::time::Instant::now() + Duration::from_millis(DEBOUNCE_MS));
                                   }
                                   None => {
            warn!("[filelisten] message receive message alreadyclosed");
                                       break;
                                   }
                               }
                           }
                           // 防抖计时器触发
                           _ = async {
                               if let Some(timer) = debounce_timer {
                                   tokio::time::sleep_until(timer).await;
                               } else {
                                   // 没有计时器时永远等待
                                   std::future::pending::<()>().await;
                               }
                           } => {
                               if !pending_dirs.is_empty() {
                                   debug!(
                                       "📁 [文件监听] 防抖触发，刷新 {} 个项目目录",
                                       pending_dirs.len()
                                   );
                                   // 刷新所有待处理的项目目录
                                   for project_dir in pending_dirs.drain() {
                                       invalidate_project_cache(&project_dir).await;
                                   }
                               }
                               debounce_timer = None;
                           }
                       }
        }
    });

    info!("[filelisten] listen message startedcompleted");
}

/// 停止文件系统监听器
#[allow(dead_code)]
pub fn stop_file_watcher() {
    if !WATCHER_STARTED.swap(false, Ordering::SeqCst) {
        return;
    }

    let watcher_holder = FILE_WATCHER.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = watcher_holder.lock() {
        *guard = None;
    }

    info!("🛑 [filelisten] listen message alreadystopped");
}

/// 通过文件扫描检查 session 是否存在（带缓存）
pub async fn check_session_file_exists(session_id: &str, project_path: &str) -> bool {
    // 确保监听器已启动
    start_file_watcher();

    let cache = get_file_scan_cache();

    // 尝试从缓存获取
    if let Some(session_ids) = cache.get(project_path).await {
        let exists = session_ids.contains(session_id);
        debug!(
            "🔍 [文件扫描缓存] session={} -> {}",
            session_id,
            if exists { "存在" } else { "不存在" }
        );
        return exists;
    }

    // 缓存未命中，执行扫描
    debug!(
        "🔍 [文件扫描] 缓存未命中，扫描目录: project_path={}",
        project_path
    );
    let session_ids = scan_project_sessions(project_path).await;
    let exists = session_ids.contains(session_id);

    // 存入缓存
    cache.insert(project_path.to_string(), session_ids).await;

    if exists {
        info!("[file message ] message session file: {}", session_id);
    } else {
        debug!("[file message ] not message session file: {}", session_id);
    }

    exists
}
