//! 资源限制与安全配置（被 docker_manager 等独立引用，域自洽）。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ServiceResourceLimits {
    /// 内存限制（字节，支持浮点数输入）
    ///
    /// 注：字段名 `memory`（对齐 /computer/pod/ensure 基准）；serde alias `memory_limit`
    /// 兼容旧 config.yml 键名与旧 HTTP 请求，反序列化两种写法都接受。
    #[serde(alias = "memory_limit")]
    pub memory: Option<f64>,
    /// CPU 限制（核心数）。alias `cpu_limit` 兼容旧命名。
    #[serde(alias = "cpu_limit")]
    pub cpu: Option<f64>,
    /// 交换空间限制（字节，支持浮点数输入）。alias `swap_limit` 兼容旧命名。
    #[serde(alias = "swap_limit")]
    pub swap: Option<f64>,
    /// PVC 存储空间大小（仅 K8s 模式生效，Docker 模式忽略）
    ///
    /// 格式：`<数字><单位>`，支持 Mi/Gi/Ti（二进制）和 M/G/T（十进制）
    /// 范围：最小 1Gi，最大 100Ti，默认 10Gi
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_size: Option<String>,
    /// 临时存储限制（overlay 可写层，仅 K8s 模式生效）
    ///
    /// 限制容器根文件系统可写层 + emptyDir 等临时存储的写入量（区别于 storage_size 管 PVC）。
    /// 与 storage_size 是两个独立配额，不会合并；格式同 storage_size；未指定时回退到 storage_size 的值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ephemeral_storage_limit: Option<String>,
}

/// 服务容器的安全配置（可选）。仅 Docker 部署模式透传到 bollard HostConfig。
///
/// 字段语义与 Docker `HostConfig` / docker-compose.yml 一致，运维可直接照搬 compose 写法。
/// 合并语义（在 docker_manager 的 `build_host_config` 中应用）：
/// - `ServiceImageConfig.security = None`（未配置 security 块）→ 完全走代码默认逻辑
///   （`privileged=false` + `cap_drop=[NET_RAW,NET_ADMIN]`，受 `ebpf-debug` feature 影响）。
/// - `security = Some`（配置了 security 块）→ 该配置覆盖一切（含 `ebpf-debug`）；
///   块内每个字段 `Some(x)` 用 x，字段未写（`None`）回退到该字段的内置默认。
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ServiceSecurityConfig {
    /// 是否以特权模式运行（默认 false）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub privileged: Option<bool>,
    /// 要添加的内核 capabilities，如 ["SYS_PTRACE"]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cap_add: Option<Vec<String>>,
    /// 要移除的内核 capabilities；显式配置则整体覆盖默认 ["NET_RAW","NET_ADMIN"]（写 `[]` 表示不 drop 任何 cap）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cap_drop: Option<Vec<String>>,
    /// Docker security_opt，如 ["seccomp=unconfined","apparmor=unconfined"]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security_opt: Option<Vec<String>>,
    /// 进程数限制；`0` 或 `-1` 表示无限制
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pids_limit: Option<i64>,
    /// 是否在容器内运行 init 进程（转发信号 + 回收僵尸进程）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub init: Option<bool>,
}

impl ServiceResourceLimits {
    /// 构造资源限制配置
    ///
    /// 所有参数均为 `Option`，未限制的资源传 `None`。
    /// - K8s 模式下 `storage_size` 管 PVC，`ephemeral_storage_limit` 管 overlay 可写层（未指定时回退到 `storage_size`）
    /// - Docker 模式忽略 `storage_size` / `ephemeral_storage_limit`
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        memory: Option<f64>,
        cpu: Option<f64>,
        swap: Option<f64>,
        storage_size: Option<String>,
        ephemeral_storage_limit: Option<String>,
    ) -> Self {
        Self {
            memory,
            cpu,
            swap,
            storage_size,
            ephemeral_storage_limit,
        }
    }

    /// 验证资源限制的合理性
    pub fn validate(&self) -> Result<(), String> {
        // 内存限制（bytes）。阈值用十进制 MB/GB（与 Docker/K8s quantity 习惯一致，
        // 非 MiB/GiB；512×10⁶ 而非 512×2²⁰）：512MB ~ 64GB
        const MIN_MEMORY_BYTES: f64 = 512_000_000.0;
        const MAX_MEMORY_BYTES: f64 = 64_000_000_000.0;
        if let Some(memory) = self.memory {
            if memory < MIN_MEMORY_BYTES {
                return Err("memory_limit must be at least 512MB".to_string());
            }
            if memory > MAX_MEMORY_BYTES {
                return Err("memory_limit cannot exceed 64GB".to_string());
            }
        }

        // CPU 限制：0.5 ~ 32 核
        if let Some(cpu) = self.cpu {
            if cpu < 0.5 {
                return Err("cpu_limit must be at least 0.5 cores".to_string());
            }
            if cpu > 32.0 {
                return Err("cpu_limit cannot exceed 32 cores".to_string());
            }
        }

        // 注:swap 与 memory 的关系校验已移除——改为在 resolve 阶段由
        // [`ServiceResourceLimits::normalize_swap`] 自动规整(swap < memory 时
        // 上调到 memory × 2),避免上游误传 swap<memory 直接阻塞业务。
        // 详见该函数文档。

        Ok(())
    }

    /// 合并资源限制（override_limits 覆盖 self 中的字段）
    pub fn merge_with(&self, override_limits: &ServiceResourceLimits) -> Self {
        Self {
            memory: override_limits.memory.or(self.memory),
            cpu: override_limits.cpu.or(self.cpu),
            swap: override_limits.swap.or(self.swap),
            storage_size: override_limits
                .storage_size
                .clone()
                .or_else(|| self.storage_size.clone()),
            ephemeral_storage_limit: override_limits
                .ephemeral_storage_limit
                .clone()
                .or_else(|| self.ephemeral_storage_limit.clone()),
        }
    }

    /// 规整 swap 上限:若 `swap < memory`,自动上调到 `memory × 2`。
    ///
    /// # 背景
    /// cgroup `memory.memsw.limit`(Docker `--memory-swap`、K8s 同义)是
    /// **memory + swap 的总和**,语义上必须 ≥ memory。上游(如 Backend)偶尔会误传
    /// `swap < memory`(典型场景:把 swap 按核数 `perUserCpuCores × 1GiB` 估算,
    /// 而 memory 按 `perUserMemoryGB × 1GiB` 估算,当核数 < 内存 GB 数时 swap 反而更小)。
    /// 与其在 validate 阶段硬性拒绝阻塞业务,这里按 `memory × 2` 兜底——既满足
    /// cgroup 约束,又留出 1×memory 的交换空间。
    ///
    /// 仅在 memory 与 swap 均 `Some` 且 `swap < memory` 时生效;其余情况原样返回。
    ///
    /// # 返回
    /// `(规整后的 Self, 是否发生了修正)` —— 调用方据 `bool` 决定是否打 warn 日志。
    pub fn normalize_swap(mut self) -> (Self, bool) {
        if let (Some(memory), Some(swap)) = (self.memory, self.swap)
            && swap < memory
        {
            self.swap = Some(memory * 2.0);
            return (self, true);
        }
        (self, false)
    }
}
