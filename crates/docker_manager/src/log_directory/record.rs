//! RCoder 为容器独占新建的日志挂载记录。
//!
//! 创建容器时，RCoder 给每一条指向可回收日志域的挂载独占新建一个目录
//! （`agent_container_starter::mounts`）。哪些挂载是这么来的，只有创建的那一刻知道得确切：之后
//! 从外面看，一个带数据的目录可以和日志目录长得一模一样——名字带时间戳、挂载点碰巧相同。
//! 所以创建的时候就把它们记在容器的 label [`LOG_MOUNTS_LABEL`] 上，每条是（容器内挂载点，
//! RCoder 新建的那个目录）。
//!
//! builder 重启出替代容器时，只给记录里的挂载换新目录；替代容器带上自己的记录，换过源的记新
//! 目录。归档把这个 label 一起存下来。升级之前建的容器没有这份记录，见
//! `runtime::docker_builder_restart::log_directory`。

use serde::{Deserialize, Serialize};

/// 容器 label：RCoder 为这个容器独占新建目录的日志挂载
pub(crate) const LOG_MOUNTS_LABEL: &str = "rcoder.io/log-mounts";

/// 一条日志挂载：RCoder 为这个容器在 `source` 独占新建了目录，把它挂在容器的 `target`
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct LogMountRecord {
    /// 容器内的挂载点
    pub(crate) target: String,
    /// RCoder 独占新建的目录（宿主机路径，Docker 看到的写法）
    pub(crate) source: String,
}

/// 记成 label 的值。里面只有字符串，序列化不会失败；真失败了就是空记录——换源只认记录，空
/// 记录等于一条都不换，是安全的方向
pub(crate) fn encode_log_mounts(records: &[LogMountRecord]) -> String {
    serde_json::to_string(records).unwrap_or_default()
}

/// 从 label 读回记录。读不出来（不是这个格式）是 `None`
pub(crate) fn decode_log_mounts(label: &str) -> Option<Vec<LogMountRecord>> {
    serde_json::from_str(label).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(target: &str, source: &str) -> LogMountRecord {
        LogMountRecord {
            target: target.into(),
            source: source.into(),
        }
    }

    #[test]
    fn a_record_survives_the_round_trip_through_the_label() {
        let records = [
            record(
                "/app/container-logs",
                "/srv/logs/container/runner-a-20261029035324",
            ),
            record(
                "/app/audit-logs",
                "/srv/logs/container/runner-a-audit-20261029035324",
            ),
        ];

        let label = encode_log_mounts(&records);

        assert_eq!(decode_log_mounts(&label), Some(records.to_vec()));
        assert!(
            label.starts_with('['),
            "the label is a JSON array, whatever the field order: {label}"
        );
    }

    #[test]
    fn a_label_that_is_not_a_record_reads_as_none() {
        assert_eq!(decode_log_mounts(""), None);
        assert_eq!(decode_log_mounts("runner-a"), None);
        assert_eq!(decode_log_mounts("{\"target\":42}"), None);
    }
}
