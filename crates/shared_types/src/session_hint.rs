//! Classify resume hints without turning preparation state into session identity.
//!
//! The legacy Runner used one reserved state label as a session ID. Recognize
//! that historical value only to recover; new reservations must carry no ID.
//! Established ACP IDs remain opaque and are not restricted to UUIDs.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionHint<'a> {
    Absent,
    LegacyPending,
    Established(&'a str),
}

impl<'a> SessionHint<'a> {
    pub fn classify(value: Option<&'a str>) -> Self {
        match value {
            None | Some("") => Self::Absent,
            Some("pending") => Self::LegacyPending,
            Some(session_id) => Self::Established(session_id),
        }
    }

    pub fn as_session_id(self) -> Option<&'a str> {
        match self {
            Self::Absent | Self::LegacyPending => None,
            Self::Established(session_id) => Some(session_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_and_legacy_state_labels_are_not_resume_identities() {
        assert_eq!(SessionHint::classify(None), SessionHint::Absent);
        assert_eq!(SessionHint::classify(Some("")), SessionHint::Absent);
        assert_eq!(
            SessionHint::classify(Some("pending")),
            SessionHint::LegacyPending
        );
        for value in [None, Some(""), Some("pending")] {
            assert_eq!(SessionHint::classify(value).as_session_id(), None);
        }
    }

    #[test]
    fn established_ids_keep_their_opaque_exact_values() {
        for value in [
            "session-123",
            "pending-session",
            "Pending",
            " pending",
            "opaque/会话:7",
        ] {
            assert_eq!(
                SessionHint::classify(Some(value)),
                SessionHint::Established(value)
            );
            assert_eq!(
                SessionHint::classify(Some(value)).as_session_id(),
                Some(value)
            );
        }
    }
}
