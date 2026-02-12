use std::path::{Path, PathBuf};

/// Unicode whitespace characters that appear in macOS system-generated
/// filenames (e.g. U+202F narrow no-break space before AM/PM in screenshots).
/// These are visually indistinguishable from ASCII space but cause path
/// mismatches when an LLM reproduces the filename with a regular space.
const UNICODE_WHITESPACE: &[char] = &[
    '\u{00A0}', // no-break space
    '\u{202F}', // narrow no-break space (macOS screenshots)
    '\u{2007}', // figure space
    '\u{2009}', // thin space
    '\u{200A}', // hair space
    '\u{2002}', // en space
    '\u{2003}', // em space
    '\u{205F}', // medium mathematical space
    '\u{3000}', // ideographic space
];

/// Replace Unicode whitespace characters with ASCII space.
pub fn normalize_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if UNICODE_WHITESPACE.contains(&c) {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

/// Try to resolve a path that may have had its Unicode whitespace replaced
/// with ASCII space (e.g. by an LLM that can't reproduce invisible chars).
///
/// Scans the parent directory for an entry whose name matches the requested
/// filename after normalizing Unicode whitespace on both sides.
///
/// Returns `Some(actual_path)` if a unique match is found, `None` otherwise.
pub fn try_resolve_unicode_path(path: &str) -> Option<PathBuf> {
    let p = Path::new(path);
    let parent = p.parent()?;
    let target_name = p.file_name()?.to_string_lossy();
    let normalized_target = normalize_whitespace(&target_name);

    // If the target already exists exactly, no fuzzy matching needed
    if p.exists() {
        return None;
    }

    // Parent must exist for us to scan it
    let entries = std::fs::read_dir(parent).ok()?;

    for entry in entries.flatten() {
        let entry_name = entry.file_name().to_string_lossy().to_string();
        if normalize_whitespace(&entry_name) == normalized_target {
            return Some(entry.path());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_whitespace_ascii_unchanged() {
        assert_eq!(normalize_whitespace("hello world"), "hello world");
    }

    #[test]
    fn test_normalize_narrow_no_break_space() {
        let input = "Screenshot 2026-02-10 at 2.55.22\u{202F}PM.png";
        let expected = "Screenshot 2026-02-10 at 2.55.22 PM.png";
        assert_eq!(normalize_whitespace(input), expected);
    }

    #[test]
    fn test_normalize_multiple_types() {
        let input = "a\u{00A0}b\u{2003}c\u{3000}d";
        assert_eq!(normalize_whitespace(input), "a b c d");
    }

    #[test]
    fn test_resolve_nonexistent_parent_returns_none() {
        assert!(try_resolve_unicode_path("/nonexistent_dir_12345/file.txt").is_none());
    }

    #[test]
    fn test_resolve_exact_match_returns_none() {
        // If the file exists exactly, return None (no fuzzy match needed)
        let tmp = std::env::temp_dir();
        let file = tmp.join("beakr_unicode_test_exact.txt");
        std::fs::write(&file, "test").unwrap();
        assert!(try_resolve_unicode_path(file.to_str().unwrap()).is_none());
        std::fs::remove_file(&file).ok();
    }

    #[test]
    fn test_resolve_unicode_whitespace_match() {
        let tmp = std::env::temp_dir();
        // Create a file with U+202F in the name
        let real_name = "beakr_test_2.55.22\u{202F}PM.txt";
        let real_path = tmp.join(real_name);
        std::fs::write(&real_path, "test").unwrap();

        // Try to resolve the ASCII-space version
        let ascii_name = "beakr_test_2.55.22 PM.txt";
        let ascii_path = tmp.join(ascii_name);
        let resolved = try_resolve_unicode_path(ascii_path.to_str().unwrap());

        std::fs::remove_file(&real_path).ok();

        assert!(resolved.is_some());
        assert_eq!(resolved.unwrap(), real_path);
    }
}
