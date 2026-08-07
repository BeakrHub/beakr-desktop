use std::path::{Path, PathBuf};

use super::SecurityError;
use crate::unicode;

fn path_for_wire(path: &Path) -> PathBuf {
    dunce::simplified(path).to_path_buf()
}

/// Validate that `path` resolves to a location within one of the `scoped_folders`.
///
/// 1. Canonicalize the path (resolves symlinks, `..`, etc.)
/// 2. Check that the canonical path starts with at least one scoped folder
///
/// If canonicalization fails (file not found), attempts a fuzzy match by
/// normalizing Unicode whitespace in the filename.  macOS uses non-ASCII
/// whitespace in system-generated names (e.g. U+202F before AM/PM in
/// screenshots) which LLMs replace with regular ASCII space.
///
/// Returns the canonicalized path on success.
pub fn validate_path(path: &str, scoped_folders: &[String]) -> Result<PathBuf, SecurityError> {
    if scoped_folders.is_empty() {
        return Err(SecurityError::OutOfScope(
            "No scoped folders configured".to_string(),
        ));
    }

    let target = Path::new(path);

    // Canonicalize resolves symlinks and relative components.
    // If it fails (file not found), try resolving Unicode whitespace mismatches.
    let canonical = match std::fs::canonicalize(target) {
        Ok(p) => p,
        Err(original_err) => {
            if let Some(resolved) = unicode::try_resolve_unicode_path(path) {
                std::fs::canonicalize(&resolved)
                    .map_err(|e| SecurityError::ResolutionFailed(format!("{path}: {e}")))?
            } else {
                return Err(SecurityError::ResolutionFailed(format!(
                    "{path}: {original_err}"
                )));
            }
        }
    };

    for folder in scoped_folders {
        let folder_canonical = match std::fs::canonicalize(folder) {
            Ok(p) => p,
            Err(_) => continue, // Folder doesn't exist — skip
        };

        if canonical.starts_with(&folder_canonical) {
            // Windows canonicalize() returns a verbatim path. Prefer the
            // interoperable drive-letter form when it is safe, but retain the
            // prefix for long/reserved paths that require Win32 verbatim APIs.
            return Ok(path_for_wire(&canonical));
        }
    }

    Err(SecurityError::OutOfScope(canonical.display().to_string()))
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
        let scoped = vec![env::temp_dir()
            .join("beakr_test_nonexistent")
            .display()
            .to_string()];
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

    #[cfg(windows)]
    #[test]
    fn short_windows_path_uses_plain_wire_format() {
        let root = env::temp_dir().join(format!("beakr_wire_path_{}", std::process::id()));
        let file = root.join("notes.md");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&file, "notes").unwrap();

        let result = validate_path(file.to_str().unwrap(), &[root.display().to_string()]).unwrap();

        std::fs::remove_dir_all(&root).ok();
        assert!(
            !result.to_string_lossy().starts_with(r"\\?\"),
            "short path leaked verbatim prefix: {}",
            result.display()
        );
    }

    #[cfg(windows)]
    #[test]
    fn long_windows_path_keeps_verbatim_prefix_for_wire() {
        let segment_a = "a".repeat(126);
        let segment_b = "b".repeat(126);
        let path = PathBuf::from(format!(r"\\?\C:\{segment_a}\{segment_b}\notes.md"));
        assert!(path.as_os_str().len() > 260);

        assert_eq!(path_for_wire(&path), path);
    }
}
