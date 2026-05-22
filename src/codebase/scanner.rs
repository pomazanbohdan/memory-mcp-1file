use std::path::{Path, PathBuf};

use ignore::{overrides::OverrideBuilder, WalkBuilder};

use crate::types::Language;

/// Directories that should be skipped entirely during traversal.
/// This prevents the walker from even descending into them (saves I/O).
const SKIP_DIRS: &[&str] = &[
    // Build artifacts
    "build",
    "dist",
    "out",
    "target",
    // JS/Node
    "node_modules",
    ".npm",
    ".yarn",
    ".pnp",
    // Dart/Flutter
    ".dart_tool",
    ".pub-cache",
    // Android/Gradle
    ".gradle",
    ".android",
    // iOS
    "Pods",
    ".symlinks",
    // Python
    "__pycache__",
    ".venv",
    "venv",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    "egg-info",
    // Rust
    ".cargo",
    // IDE/Editor
    ".idea",
    ".vscode",
    ".vs",
    // Cache/temp
    // Coverage/test artifacts
    "coverage",
    ".nyc_output",
    // Container/infra
    ".terraform",
    // Generated
    ".generated",
    ".next",
    ".nuxt",
    ".svelte-kit",
];

pub fn scan_directory(root: &Path) -> crate::Result<Vec<PathBuf>> {
    let mut overrides = OverrideBuilder::new(root);
    // Generated dart files (match at any depth)
    let _ = overrides.add("!**/*.g.dart");
    let _ = overrides.add("!**/*.freezed.dart");
    let _ = overrides.add("!**/*.gr.dart");
    let _ = overrides.add("!**/*.config.dart");
    let _ = overrides.add("!**/*.mocks.dart");
    let _ = overrides.add("!**/*.arb");

    // Skip directories at any nesting depth via overrides
    for dir in SKIP_DIRS {
        let _ = overrides.add(&format!("!**/{dir}/**"));
    }

    let walker = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .add_custom_ignore_filename(".memoryignore")
        .overrides(
            overrides
                .build()
                .unwrap_or_else(|_| ignore::overrides::Override::empty()),
        )
        .filter_entry(|entry| {
            // Early directory pruning — don't even descend into known-junk dirs.
            // This is the critical optimization: avoids stat()/readdir() on millions of files.
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                if let Some(name) = entry.file_name().to_str() {
                    let name_lower = name.to_lowercase();
                    // Skip any directory whose name matches SKIP_DIRS
                    if SKIP_DIRS
                        .iter()
                        .any(|d| d.eq_ignore_ascii_case(&name_lower))
                    {
                        return false;
                    }
                    // Also skip hidden directories (start with .)
                    if name.starts_with('.') && name != "." && name != ".." {
                        return false;
                    }
                }
            }
            true
        })
        .build();

    let mut files = Vec::new();
    for entry in walker.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_file() && !is_ignored_file(path) && is_code_file(path) {
            files.push(path.to_path_buf());
        }
    }

    Ok(files)
}

pub fn is_ignored_file(path: &Path) -> bool {
    let path_str = path.to_string_lossy().to_lowercase();

    // Check if any path component matches a skip directory
    for dir in SKIP_DIRS {
        let pattern1 = format!("/{}/", dir);
        let pattern2 = format!("\\{}\\", dir);
        if path_str.contains(&pattern1) || path_str.contains(&pattern2) {
            return true;
        }
    }

    // Hidden path components (e.g. .secret/token.py or .venv/lib/site.py)
    // should be excluded consistently for both scanner and watcher paths.
    if path.components().any(|component| {
        let part = component.as_os_str().to_string_lossy();
        part.starts_with('.') && part != "." && part != ".."
    }) {
        return true;
    }

    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();

    // Lock files
    if matches!(
        name.as_str(),
        "package-lock.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "composer.lock"
            | "cargo.lock"
            | "pubspec.lock"
            | "gemfile.lock"
            | "poetry.lock"
    ) {
        return true;
    }

    // Generated / minified files
    name.ends_with(".g.dart")
        || name.ends_with(".freezed.dart")
        || name.ends_with(".gr.dart")
        || name.ends_with(".config.dart")
        || name.ends_with(".mocks.dart")
        || name.ends_with(".arb")
        || name.ends_with(".min.js")
        || name.ends_with(".min.css")
        || name.ends_with(".bundle.js")
        || name.ends_with(".map")
        || name.ends_with(".d.ts")
        || path_str.contains("/generated/")
        || path_str.contains("\\generated\\")
}

pub fn is_code_file(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };

    matches!(
        ext.to_lowercase().as_str(),
        "rs" | "py"
            | "js"
            | "ts"
            | "jsx"
            | "tsx"
            | "go"
            | "java"
            | "dart"
            | "c"
            | "cpp"
            | "h"
            | "hpp"
            | "rb"
            | "php"
            | "swift"
            | "kt"
            | "scala"
            | "sh"
            | "bash"
            | "zsh"
    )
}

pub fn detect_language(path: &Path) -> Language {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return Language::Unknown;
    };

    match ext.to_lowercase().as_str() {
        "rs" => Language::Rust,
        "py" => Language::Python,
        "js" | "jsx" => Language::JavaScript,
        "ts" | "tsx" => Language::TypeScript,
        "go" => Language::Go,
        "java" => Language::Java,
        "dart" => Language::Dart,
        _ => Language::Unknown,
    }
}
