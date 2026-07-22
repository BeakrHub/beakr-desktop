//! Directories excluded from file search as build / dependency / cache
//! **noise** — deliberately separate from the security deny-list (`security::
//! is_denied`), which blocks *sensitive* files. The agent's whole job is
//! finding the user's files, so recall is the priority: unambiguous generated
//! directories are excluded everywhere, but ambiguous generic names (a folder
//! literally called `build` or `dist`) are only excluded when a sibling
//! manifest proves we are inside a real build tree.
//!
//! `.git`, `node_modules`, `.venv`, `__pycache__`, `.terraform`, `.ssh`,
//! `.aws`, `.gnupg` are already handled by the security deny-list and are not
//! repeated here.
//!
//! `.gitignore` is intentionally NOT consulted: for a find-my-files agent it
//! routinely hides exactly what the user is looking for (a gitignored
//! `notes/`, `data/`, local config), the opposite of what a code search wants.

use std::path::Path;

/// Unambiguous generated / dependency / cache directories — excluded in every
/// folder, git or not. Almost all are hidden dotfiles (never searched by name)
/// or unmistakable dependency dirs; collision with real user content is ~zero.
const ALWAYS_EXCLUDED: &[&str] = &[
    // JS/TS build + cache
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".output",
    ".turbo",
    ".parcel-cache",
    ".vite",
    ".nyc_output",
    "bower_components",
    "jspm_packages",
    "web_modules",
    // generic hidden caches
    ".cache",
    ".sass-cache",
    ".gradle",
    ".tmp",
    // Python caches / packaging
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".nox",
    ".ipynb_checkpoints",
    ".eggs",
    // other VCS metadata (.git is in the security deny-list)
    ".svn",
    ".hg",
    "CVS",
    // IDE
    ".idea",
];

/// Ambiguous generic names: real build output inside a project, but also
/// plausible user folders (`~/Documents/build/`, `~/Documents/vendors/`).
/// Excluded ONLY when one of the marker files sits beside the directory,
/// proving it is a build tree. `bin`/`obj` are intentionally omitted — too
/// high a collision rate to risk for a general agent.
const CONDITIONAL_EXCLUDED: &[(&str, &[&str])] = &[
    ("dist", &["package.json", "tsconfig.json"]),
    ("build", &["package.json", "build.gradle", "pom.xml", "CMakeLists.txt"]),
    ("out", &["package.json", "tsconfig.json"]),
    ("target", &["Cargo.toml", "pom.xml", "build.gradle"]),
    ("coverage", &["package.json"]),
    ("vendor", &["composer.json", "Gemfile", "go.mod"]),
];

/// Whether `dir` (a directory path) should be skipped by search as build/cache
/// noise. Directories only — call it when you already know the entry is a dir.
pub fn is_excluded_dir(dir: &Path) -> bool {
    let name = match dir.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return false,
    };

    if ALWAYS_EXCLUDED.contains(&name) {
        return true;
    }
    // Python packaging metadata dirs, e.g. `beakr.egg-info`.
    if name.ends_with(".egg-info") {
        return true;
    }

    // Risky generics: exclude only inside a proven build tree.
    if let Some((_, markers)) = CONDITIONAL_EXCLUDED.iter().find(|(n, _)| *n == name) {
        if let Some(parent) = dir.parent() {
            return markers.iter().any(|marker| parent.join(marker).exists());
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn always_excluded_names() {
        for name in [".next", ".turbo", ".pytest_cache", "bower_components", ".idea"] {
            assert!(is_excluded_dir(&PathBuf::from("/project").join(name)), "{name}");
        }
        assert!(is_excluded_dir(&PathBuf::from("/project/beakr.egg-info")));
        // A normal source dir is never excluded.
        assert!(!is_excluded_dir(&PathBuf::from("/project/src")));
        assert!(!is_excluded_dir(&PathBuf::from("/Users/me/Documents/notes")));
    }

    #[test]
    fn generic_names_need_a_sibling_marker() {
        // These paths do not exist on disk, so the marker check is false ->
        // NOT excluded. This is the recall-preserving default: a bare `build`
        // or `dist` folder in Documents stays searchable.
        assert!(!is_excluded_dir(&PathBuf::from("/Users/me/Documents/build")));
        assert!(!is_excluded_dir(&PathBuf::from("/Users/me/Documents/dist")));
        assert!(!is_excluded_dir(&PathBuf::from("/Users/me/Documents/vendors")));
    }
}
