use std::path::{Path, PathBuf};

use super::SecurityError;

/// Validate that `path` resolves to a location within one of the `scoped_folders`.
///
/// 1. Canonicalize the path (resolves symlinks, `..`, etc.)
/// 2. Check that the canonical path starts with at least one scoped folder
///
/// Returns the canonicalized path on success.
pub fn validate_path(path: &str, scoped_folders: &[String]) -> Result<PathBuf, SecurityError> {
    if scoped_folders.is_empty() {
        return Err(SecurityError::OutOfScope(
            "No scoped folders configured".to_string(),
        ));
    }

    let target = Path::new(path);

    // Canonicalize resolves symlinks and relative components
    let canonical = std::fs::canonicalize(target).map_err(|e| {
        SecurityError::ResolutionFailed(format!("{path}: {e}"))
    })?;

    for folder in scoped_folders {
        let folder_canonical = match std::fs::canonicalize(folder) {
            Ok(p) => p,
            Err(_) => continue, // Folder doesn't exist — skip
        };

        if canonical.starts_with(&folder_canonical) {
            return Ok(canonical);
        }
    }

    Err(SecurityError::OutOfScope(
        canonical.display().to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_valid_path_in_scope() {
        let tmp = env::temp_dir();
        let scoped = vec![tmp.display().to_string()];
        let test_path = tmp.join("test_file_beakr");

        // Create a temp file so canonicalize works
        std::fs::write(&test_path, "test").unwrap();
        let result = validate_path(test_path.to_str().unwrap(), &scoped);
        std::fs::remove_file(&test_path).ok();

        assert!(result.is_ok());
    }

    #[test]
    fn test_path_outside_scope() {
        // Use a nonexistent scoped folder so any real path falls outside scope.
        // The path just needs to exist on the OS for canonicalize to succeed,
        // but it doesn't matter since the scoped folder won't match.
        let scoped = vec![env::temp_dir().join("beakr_test_nonexistent").display().to_string()];
        // Use temp_dir itself as the target — it exists on all platforms
        let target = env::temp_dir().join("beakr_oob_test");
        std::fs::write(&target, "test").unwrap();
        let result = validate_path(target.to_str().unwrap(), &scoped);
        std::fs::remove_file(&target).ok();
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_scoped_folders() {
        let target = env::temp_dir().display().to_string();
        let result = validate_path(&target, &[]);
        assert!(result.is_err());
    }
}
