//! Internal recovery pagination, separate from execution and HTTP contracts.

/// A bounded raw storage page can contain no decodable records and still have
/// a successor page. Its cursor must advance from the last raw identity; an
/// invalid record remains protected and never becomes absence or write authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserAppRecoveryScanPage<T> {
    pub records: Vec<T>,
    pub next_after: Option<String>,
    pub exhausted: bool,
}
