//! Stable machine-parseable error codes for the wire envelope.
//!
//! Handlers keep returning `Result<_, String>` (messages stay byte-compatible
//! with the TS daemon); classification happens once at the packaging boundary
//! (`process_request` → `error_response`) so scripts and AI agents can branch
//! on `errorCode` / exit code instead of regex-matching English prose.

/// Wire values are snake_case strings under the `errorCode` response field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorCode {
    /// Selector never matched / never became visible before the deadline.
    /// Note: includes "matched but stayed hidden" — not strictly DOM absence.
    SelectorNotFound,
    /// 1-based tab index out of range, or no tabs open.
    TabNotFound,
    /// A wait (`wait-url`, `wait-network-idle`) or CDP RPC hit its deadline.
    Timeout,
    /// User-supplied or injected JS threw inside `Runtime.evaluate`.
    EvalError,
    /// Chromium is (re)starting; the CDP client is not attached yet.
    CdpNotReady,
    /// Secrets vault locked, missing, or a crypto/lookup failure.
    VaultError,
    /// Malformed request JSON, unknown action, or missing/invalid params.
    InvalidRequest,
    /// Result exceeded the `--max-chars` clamp and cannot be truncated
    /// safely (non-string eval results — truncated JSON would be corrupt).
    OutputTooLarge,
    /// Daemon socket unreachable, spawn failed, or drain in progress.
    DaemonUnreachable,
    /// Anything not classified above.
    Internal,
}

impl ErrorCode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ErrorCode::SelectorNotFound => "selector_not_found",
            ErrorCode::TabNotFound => "tab_not_found",
            ErrorCode::Timeout => "timeout",
            ErrorCode::EvalError => "eval_error",
            ErrorCode::CdpNotReady => "cdp_not_ready",
            ErrorCode::VaultError => "vault_error",
            ErrorCode::InvalidRequest => "invalid_request",
            ErrorCode::OutputTooLarge => "output_too_large",
            ErrorCode::DaemonUnreachable => "daemon_unreachable",
            ErrorCode::Internal => "internal",
        }
    }
}

/// Classify an error message into a stable code. Single choke point — every
/// daemon failure flows through here, so eval exceptions, CDP RPC timeouts,
/// and handler-formatted strings all land on the same taxonomy.
///
/// Precedence matters: the injected-JS convention `Selector not found:` rides
/// inside a `Runtime.evaluate: ...` exception message, so selector/tab checks
/// run before the eval-prefix check; the eval-prefix check runs before the
/// generic "timed out" so a page's own error prose can't masquerade as an RPC
/// timeout.
pub(crate) fn classify_error_code(message: &str) -> ErrorCode {
    let m = message.to_lowercase();
    if m.contains("selector not found:")
        || m.contains("selector miss")
        || m.contains("not visible after")
        || m.contains("no element with text")
    {
        ErrorCode::SelectorNotFound
    } else if m.contains("tab not found:") || m.contains("no browser tabs are open") {
        ErrorCode::TabNotFound
    } else if m.starts_with("runtime.evaluate:") || m.starts_with("runtime.callfunctionon:") {
        ErrorCode::EvalError
    } else if m.contains("timed out") {
        ErrorCode::Timeout
    } else if m.contains("cdp client not initialized") {
        ErrorCode::CdpNotReady
    } else if m.contains("vault") || m.contains("tabd_vault_key") {
        ErrorCode::VaultError
    } else if m.contains("invalid json in request")
        || m.contains("unsupported action:")
        || m.contains("missing '")
        || m.contains("invalid '")
        || m.contains("is required")
        || m.contains("unsupported patterntype")
        || m.contains("file not found")
    {
        ErrorCode::InvalidRequest
    } else if m.contains("result too large") {
        ErrorCode::OutputTooLarge
    } else if m.contains("shutting down (drain in progress)") {
        ErrorCode::DaemonUnreachable
    } else {
        ErrorCode::Internal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selector_paths_classify_as_selector_not_found() {
        // Injected JS throw (click/type/get-html), wrapped by the eval prefix.
        assert_eq!(
            classify_error_code("Runtime.evaluate: Error: Selector not found: .missing"),
            ErrorCode::SelectorNotFound
        );
        // hover/scroll/check/type-secret "selector miss" convention.
        assert_eq!(
            classify_error_code("Runtime.evaluate: Error: hover: selector miss"),
            ErrorCode::SelectorNotFound
        );
        // wait_for_selector_visible deadline expiry.
        assert_eq!(
            classify_error_code("selector .login not visible after 30000ms"),
            ErrorCode::SelectorNotFound
        );
        // click --text expiry — both bare (Rust-generated) and wrapped in an
        // eval prefix must beat the eval_error branch.
        assert_eq!(
            classify_error_code("no element with text \"Sign in\" found after 30000ms"),
            ErrorCode::SelectorNotFound
        );
        assert_eq!(
            classify_error_code("Runtime.evaluate: Error: no element with text \"x\" found"),
            ErrorCode::SelectorNotFound
        );
    }

    #[test]
    fn tab_resolution_classifies_as_tab_not_found() {
        assert_eq!(
            classify_error_code("Tab not found: 9"),
            ErrorCode::TabNotFound
        );
        assert_eq!(
            classify_error_code("No browser tabs are open"),
            ErrorCode::TabNotFound
        );
    }

    #[test]
    fn eval_prefix_beats_timeout_prose() {
        // A page throwing its own "timed out" prose is an eval error, not an
        // RPC timeout.
        assert_eq!(
            classify_error_code("Runtime.evaluate: Error: request timed out"),
            ErrorCode::EvalError
        );
    }

    #[test]
    fn deadline_messages_classify_as_timeout() {
        assert_eq!(
            classify_error_code("wait-url timed out after 30000ms (pattern=x type=exact)"),
            ErrorCode::Timeout
        );
        assert_eq!(
            classify_error_code("Timed out waiting for network idle (3 pending requests)"),
            ErrorCode::Timeout
        );
        assert_eq!(
            classify_error_code("cdp rpc 'Page.captureScreenshot' timed out after 30000ms"),
            ErrorCode::Timeout
        );
        assert_eq!(
            classify_error_code("Network.getCookies timed out after 5s"),
            ErrorCode::Timeout
        );
    }

    #[test]
    fn infra_states_classify() {
        assert_eq!(
            classify_error_code("cdp client not initialized"),
            ErrorCode::CdpNotReady
        );
        assert_eq!(
            classify_error_code("TABD_VAULT_KEY env not set; secrets unavailable"),
            ErrorCode::VaultError
        );
        assert_eq!(
            classify_error_code("vault open failed: bad passphrase"),
            ErrorCode::VaultError
        );
        assert_eq!(
            classify_error_code("daemon is shutting down (drain in progress)"),
            ErrorCode::DaemonUnreachable
        );
    }

    #[test]
    fn request_shape_errors_classify_as_invalid_request() {
        assert_eq!(
            classify_error_code("invalid JSON in request"),
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            classify_error_code("unsupported action: dom.frobnicate"),
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            classify_error_code("missing 'selector' (string)"),
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            classify_error_code("secretId is required"),
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            classify_error_code("unsupported patternType 'fuzzy' (expected exact|glob|regex)"),
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            classify_error_code("invalid 'action' (expected accept|dismiss, got 'maybe')"),
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            classify_error_code("file not found: /tmp/missing.csv"),
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn oversized_results_classify_as_output_too_large() {
        assert_eq!(
            classify_error_code(
                "eval result too large (600000 chars > 500000); narrow the expression or pass --max-chars 0"
            ),
            ErrorCode::OutputTooLarge
        );
    }

    #[test]
    fn unknown_messages_fall_back_to_internal() {
        assert_eq!(
            classify_error_code("cdp websocket closed"),
            ErrorCode::Internal
        );
        assert_eq!(classify_error_code(""), ErrorCode::Internal);
    }
}
