use serde_json::{Value, json};

/// Shared machine-readable failure envelope before dispatcher normalization.
pub(crate) fn error(code: &str, message: impl Into<String>) -> Value {
    json!({
        "success": false,
        "error_code": code,
        "message": message.into()
    })
}
