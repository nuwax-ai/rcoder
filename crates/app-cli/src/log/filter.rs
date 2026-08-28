//! 日志记录过滤与时间戳处理（从 service.rs 拆出）。

use std::collections::VecDeque;

use chrono::{DateTime, FixedOffset, Utc};

use super::model::{LogQueryRequest, LogRecord, MAX_TAIL_PER_SOURCE};

pub(super) fn push_record(
    records: &mut VecDeque<LogRecord>,
    record: LogRecord,
    request: &LogQueryRequest,
    tail_limit: Option<usize>,
    record_limit: Option<usize>,
) -> bool {
    if !matches_filters(
        request,
        record.timestamp.as_deref(),
        record.level.as_deref(),
        &record.message,
    ) {
        return false;
    }
    records.push_back(record);
    if let Some(limit) = tail_limit {
        if records.len() > limit {
            records.pop_front();
        }
        false
    } else {
        records.len() >= record_limit.unwrap_or(MAX_TAIL_PER_SOURCE)
    }
}

fn matches_filters(
    request: &LogQueryRequest,
    timestamp: Option<&str>,
    level: Option<&str>,
    message: &str,
) -> bool {
    if let Some(keyword) = &request.keyword
        && !message.contains(keyword)
    {
        return false;
    }
    if !request.levels.is_empty()
        && !level.is_some_and(|value| {
            request
                .levels
                .iter()
                .any(|expected| expected.eq_ignore_ascii_case(value))
        })
    {
        return false;
    }
    if request.since.is_some() || request.until.is_some() {
        let Some(timestamp) = timestamp.and_then(parse_timestamp) else {
            return false;
        };
        if let Some(since) = request.since.as_deref().and_then(parse_timestamp)
            && timestamp < since
        {
            return false;
        }
        if let Some(until) = request.until.as_deref().and_then(parse_timestamp)
            && timestamp > until
        {
            return false;
        }
    }
    true
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::<FixedOffset>::parse_from_rfc3339(value)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

pub(super) fn compare_timestamps(left: Option<&str>, right: Option<&str>) -> std::cmp::Ordering {
    match (
        left.and_then(parse_timestamp),
        right.and_then(parse_timestamp),
    ) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => left.cmp(&right),
    }
}
