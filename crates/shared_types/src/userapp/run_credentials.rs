//! Capture current Source operation credentials before an owner is reused.
use super::db_admin::StartPgCredential;
use std::env::VarError;

/// Explicit request input wins over inherited configuration. An incomplete
/// inherited pair is an error, never permission to pick a default password.
pub fn resolve_source_run_pg(
    explicit: Option<&StartPgCredential>,
) -> Result<Option<StartPgCredential>, SourceRunCredentialError> {
    resolve_with(explicit, |key| std::env::var(key))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceRunCredentialError {
    IncompleteEnvironment,
    InvalidAccount,
    InvalidPassword,
}

impl std::fmt::Display for SourceRunCredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message(crate::DEFAULT_LOCALE))
    }
}

impl SourceRunCredentialError {
    pub fn message(self, locale: &str) -> String {
        crate::t(
            match self {
                Self::IncompleteEnvironment => "error.source_credentials.incomplete_environment",
                Self::InvalidAccount => "error.source_credentials.invalid_account",
                Self::InvalidPassword => "error.source_credentials.invalid_password",
            },
            locale,
        )
    }
}

impl std::error::Error for SourceRunCredentialError {}

fn validate(pg: StartPgCredential) -> Result<StartPgCredential, SourceRunCredentialError> {
    crate::pg_utils::validate_pg_identifier(&pg.username)
        .map_err(|_| SourceRunCredentialError::InvalidAccount)?;
    if pg.password.is_empty() || pg.password.contains('\0') {
        return Err(SourceRunCredentialError::InvalidPassword);
    }
    Ok(pg)
}

fn resolve_with(
    explicit: Option<&StartPgCredential>,
    mut read: impl FnMut(&str) -> Result<String, VarError>,
) -> Result<Option<StartPgCredential>, SourceRunCredentialError> {
    if let Some(pg) = explicit {
        return validate(pg.clone()).map(Some);
    }
    match (read("POSTGRES_USER"), read("POSTGRES_PASSWORD")) {
        (Ok(username), Ok(password)) => {
            validate(StartPgCredential { username, password }).map(Some)
        }
        (Err(VarError::NotPresent), Err(VarError::NotPresent)) => Ok(None),
        (Ok(_), Err(_)) | (Err(_), Ok(_)) | (Err(_), Err(_)) => {
            Err(SourceRunCredentialError::IncompleteEnvironment)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credential_messages_use_catalogs_with_english_default() {
        for error in [
            SourceRunCredentialError::IncompleteEnvironment,
            SourceRunCredentialError::InvalidAccount,
            SourceRunCredentialError::InvalidPassword,
        ] {
            let default = error.to_string();
            assert!(default.is_ascii());
            assert!(default.starts_with("Database") || default.starts_with("The database"));
            assert_eq!(error.message("unsupported"), default);
            for locale in crate::SUPPORTED_LOCALES {
                assert!(!error.message(locale).starts_with("error."));
            }
        }
        assert!(
            SourceRunCredentialError::InvalidPassword
                .message("zh-CN")
                .contains("密码")
        );
        assert!(
            SourceRunCredentialError::InvalidPassword
                .message("zh-TW")
                .contains("密碼")
        );
    }
    #[test]
    fn source_request_override_never_reads_bad_inherited_credentials() {
        let explicit = StartPgCredential {
            username: "business".into(),
            password: "current-private-input".into(),
        };
        let found = resolve_with(Some(&explicit), |_| panic!("explicit input must win"))
            .unwrap()
            .unwrap();
        assert_eq!(found, explicit);
    }
    #[test]
    fn source_operation_captures_environment_without_exposing_password_errors() {
        let pg = resolve_with(None, |key| {
            Ok(if key == "POSTGRES_USER" {
                "dev"
            } else {
                "private-value"
            }
            .into())
        })
        .unwrap()
        .unwrap();
        assert_eq!(pg.password, "private-value");
        assert!(!format!("{pg:?}").contains("private-value"));
        assert_eq!(
            resolve_with(None, |_| Err(VarError::NotPresent)).unwrap(),
            None
        );
        let error = resolve_with(None, |key| {
            if key == "POSTGRES_USER" {
                Ok("dev".into())
            } else {
                Err(VarError::NotPresent)
            }
        })
        .unwrap_err();
        assert_eq!(error, SourceRunCredentialError::IncompleteEnvironment);
    }
    #[test]
    fn explicit_invalid_credentials_do_not_fall_back() {
        for pg in [
            StartPgCredential {
                username: "bad;role".into(),
                password: "private".into(),
            },
            StartPgCredential {
                username: "dev".into(),
                password: String::new(),
            },
        ] {
            assert!(resolve_with(Some(&pg), |_| panic!("do not fall back")).is_err());
        }
    }
}
