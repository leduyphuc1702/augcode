pub use jcode_core::util::*;

use std::path::Path;

pub fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .map(|value| {
            let trimmed = value.trim();
            !trimmed.is_empty() && trimmed != "0" && !trimmed.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

pub fn is_rust_test_process() -> bool {
    cfg!(test)
        || std::env::current_exe()
            .ok()
            .as_deref()
            .map(is_rust_test_exe_path)
            .unwrap_or(false)
}

fn is_rust_test_exe_path(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if parent.file_name().and_then(|name| name.to_str()) != Some("deps") {
        return false;
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.rsplit_once('-'))
        .map(|(_, hash)| hash.len() >= 8 && hash.chars().all(|ch| ch.is_ascii_hexdigit()))
        .unwrap_or(false)
}

pub fn system_open_suppressed_for_tests() -> bool {
    is_rust_test_process() || env_truthy("JCODE_TEST_SESSION")
}

/// Read an HTTP error body without hiding failures behind an empty string.
///
/// This is useful after a non-success status when the response is about to be
/// converted into an error. If reading the body itself fails, the returned text
/// preserves that failure so callers can include it in their error message.
pub async fn http_error_body(response: reqwest::Response, context: &str) -> String {
    match response.text().await {
        Ok(body) => body,
        Err(err) => format!("<failed to read {context} response body: {err}>"),
    }
}

/// Format an anyhow error including its full cause chain.
///
/// This preserves actionable upstream details such as HTTP status/body instead of
/// only showing the outermost context message.
pub fn format_error_chain(err: &anyhow::Error) -> String {
    let mut parts = Vec::new();
    for cause in err.chain() {
        let text = cause.to_string();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        if parts.last().is_some_and(|prev: &String| prev == trimmed) {
            continue;
        }
        parts.push(trimmed.to_string());
    }

    match parts.len() {
        0 => "unknown error".to_string(),
        1 => parts.remove(0),
        _ => parts.join(": "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_error_chain_includes_nested_causes() {
        let err =
            anyhow::anyhow!("HTTP 400: invalid argument").context("Gemini generateContent failed");
        assert_eq!(
            format_error_chain(&err),
            "Gemini generateContent failed: HTTP 400: invalid argument"
        );
    }

    #[test]
    fn test_format_error_chain_deduplicates_repeated_messages() {
        let err = anyhow::anyhow!("same").context("same");
        assert_eq!(format_error_chain(&err), "same");
    }

    #[test]
    fn rust_test_exe_path_detects_target_deps_test_binary_only() {
        assert!(is_rust_test_exe_path(Path::new(
            "/repo/target/debug/deps/auth_login_flow-0123456789abcdef"
        )));
        assert!(is_rust_test_exe_path(Path::new(
            "/repo/target/debug/deps/jcode-05b165982fe3383d"
        )));
        assert!(!is_rust_test_exe_path(Path::new(
            "/repo/target/debug/jcode"
        )));
        assert!(!is_rust_test_exe_path(Path::new(
            "/repo/target/debug/deps/jcode"
        )));
    }

    #[test]
    fn system_open_is_suppressed_in_unit_tests() {
        assert!(system_open_suppressed_for_tests());
    }
}
