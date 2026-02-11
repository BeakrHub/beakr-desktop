use std::path::Path;

/// Filename patterns and directory names that are always blocked.
const DENIED_EXTENSIONS: &[&str] = &[".key", ".pem", ".p12", ".pfx", ".jks"];

const DENIED_PREFIXES: &[&str] = &[".env", "id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"];

const DENIED_DIRS: &[&str] = &[
    ".git",
    ".ssh",
    ".aws",
    ".gnupg",
    "node_modules",
    "__pycache__",
    ".venv",
    ".terraform",
];

const DENIED_EXACT: &[&str] = &[
    ".gitconfig",
    ".npmrc",
    ".pypirc",
    "credentials.json",
    "service-account.json",
];

/// Check whether a path should be blocked from access.
///
/// For `list_files`, denied entries are silently filtered out.
/// For `read_file` and `file_info`, an error is returned.
pub fn is_denied(path: &Path) -> bool {
    // Check each component of the path for denied directories
    for component in path.components() {
        let name = component.as_os_str().to_string_lossy();

        // Denied directory names
        for dir in DENIED_DIRS {
            if name == *dir {
                return true;
            }
        }
    }

    // Check filename
    let file_name = match path.file_name() {
        Some(n) => n.to_string_lossy(),
        None => return false,
    };
    let lower = file_name.to_lowercase();

    // Denied exact names
    for exact in DENIED_EXACT {
        if lower == *exact {
            return true;
        }
    }

    // Denied prefixes (e.g., .env, .env.local, .env.production)
    for prefix in DENIED_PREFIXES {
        if lower == *prefix || lower.starts_with(&format!("{prefix}.")) {
            return true;
        }
    }

    // Denied extensions
    for ext in DENIED_EXTENSIONS {
        if lower.ends_with(ext) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_env_file_denied() {
        assert!(is_denied(Path::new("/home/user/project/.env")));
        assert!(is_denied(Path::new("/home/user/project/.env.local")));
        assert!(is_denied(Path::new("/home/user/project/.env.production")));
    }

    #[test]
    fn test_key_files_denied() {
        assert!(is_denied(Path::new("/home/user/.ssh/id_rsa")));
        assert!(is_denied(Path::new("/home/user/.ssh/id_ed25519")));
        assert!(is_denied(Path::new("/home/user/cert.pem")));
        assert!(is_denied(Path::new("/home/user/key.p12")));
    }

    #[test]
    fn test_denied_directories() {
        assert!(is_denied(Path::new("/project/.git/config")));
        assert!(is_denied(Path::new("/project/node_modules/pkg/index.js")));
        assert!(is_denied(Path::new("/home/user/.ssh/known_hosts")));
        assert!(is_denied(Path::new("/home/user/.aws/credentials")));
    }

    #[test]
    fn test_normal_files_allowed() {
        assert!(!is_denied(Path::new("/home/user/document.pdf")));
        assert!(!is_denied(Path::new("/home/user/project/src/main.rs")));
        assert!(!is_denied(Path::new("/home/user/notes.txt")));
    }
}
