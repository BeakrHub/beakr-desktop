mod deny_list;
mod path_validator;

pub use deny_list::is_denied;
pub use path_validator::validate_path;

/// Security errors for path validation.
#[derive(Debug)]
#[allow(dead_code)]
pub enum SecurityError {
    OutOfScope(String),
    DeniedFile(String),
    ResolutionFailed(String),
}

impl std::fmt::Display for SecurityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfScope(p) => write!(f, "Path is outside scoped folders: {p}"),
            Self::DeniedFile(p) => write!(f, "Access denied — sensitive file: {p}"),
            Self::ResolutionFailed(e) => write!(f, "Path resolution failed: {e}"),
        }
    }
}

impl std::error::Error for SecurityError {}
