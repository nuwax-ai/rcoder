//! Validation utilities for converting garde errors to AppError

use crate::AppError;
use garde::Report;

/// 将 Garde Report 转换为 AppError
///
/// # Example
/// ```ignore
/// request.validate().map_err(garde_err_to_app_error)?;
/// ```
pub fn garde_err_to_app_error(report: Report) -> AppError {
    let errors: Vec<String> = report
        .iter()
        .map(|(path, err)| format!("{}: {}", path, err.message()))
        .collect();
    let message = errors.join("; ");
    AppError::validation_error(&message)
}
