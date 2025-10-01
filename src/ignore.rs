use anyhow::Result;
use glob::Pattern;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::{debug, trace};

/// Handles .plexifyignore files with gitignore-style pattern matching
#[derive(Debug, Clone)]
pub struct IgnoreFilter {
    /// Map from directory path to the ignore patterns that apply from that directory
    patterns_by_dir: HashMap<PathBuf, Vec<IgnorePattern>>,
    /// The root directory for the ignore filter
    root: PathBuf,
}

/// A single ignore pattern with its metadata
#[derive(Debug, Clone)]
struct IgnorePattern {
    /// The glob pattern
    pattern: Pattern,
    /// The original string pattern
    original: String,
    /// Whether this is a negation pattern (starts with !)
    negation: bool,
    /// Whether this pattern should match directories only (ends with /)
    directory_only: bool,
}

impl IgnoreFilter {
    /// Create a new ignore filter starting from the given root directory
    pub fn new(root: PathBuf) -> Result<Self> {
        let mut filter = Self {
            patterns_by_dir: HashMap::new(),
            root,
        };

        // Load all .plexifyignore files in the tree
        filter.load_ignore_files()?;

        Ok(filter)
    }

    /// Load all .plexifyignore files in the directory tree
    fn load_ignore_files(&mut self) -> Result<()> {
        use walkdir::WalkDir;

        for entry in WalkDir::new(&self.root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.is_file() && path.file_name() == Some(".plexifyignore".as_ref()) {
                self.load_ignore_file(path)?;
            }
        }

        debug!(
            "Loaded .plexifyignore patterns from {} directories",
            self.patterns_by_dir.len()
        );

        Ok(())
    }

    /// Load patterns from a single .plexifyignore file
    fn load_ignore_file(&mut self, ignore_file: &Path) -> Result<()> {
        let dir = ignore_file.parent().unwrap_or(&self.root).to_path_buf();
        let content = fs::read_to_string(ignore_file)?;

        let patterns: Vec<IgnorePattern> = content
            .lines()
            .enumerate()
            .filter_map(|(line_num, line)| {
                let trimmed = line.trim();

                // Skip empty lines and comments
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    return None;
                }

                match IgnorePattern::new(trimmed) {
                    Ok(pattern) => Some(pattern),
                    Err(e) => {
                        debug!(
                            "Invalid pattern '{}' in {}:{}: {}",
                            trimmed,
                            ignore_file.display(),
                            line_num + 1,
                            e
                        );
                        None
                    }
                }
            })
            .collect();

        if !patterns.is_empty() {
            debug!(
                "Loaded {} patterns from {}",
                patterns.len(),
                ignore_file.display()
            );
            self.patterns_by_dir.insert(dir, patterns);
        }

        Ok(())
    }

    /// Check if a path should be ignored
    /// Returns true if the path should be ignored, false otherwise
    pub fn should_ignore(&self, path: &Path) -> bool {
        let relative_path = match path.strip_prefix(&self.root) {
            Ok(rel) => rel,
            Err(_) => {
                // Path is not under root, don't ignore
                return false;
            }
        };

        // Convert to forward slashes for consistent matching
        let path_str = relative_path.to_string_lossy().replace("\\", "/");
        let is_dir = path.is_dir();

        trace!(
            "Checking if path should be ignored: {} (is_dir: {})",
            path_str,
            is_dir
        );

        // First check if any parent directory is ignored
        if !is_dir {
            let mut current_parent = path.parent();
            while let Some(parent) = current_parent {
                if parent != self.root && self.should_ignore(parent) {
                    trace!(
                        "Path '{}' ignored because parent directory is ignored",
                        path_str
                    );
                    return true;
                }
                current_parent = parent.parent();
            }
        }

        // Check patterns from all applicable directories, starting from root to specific
        let mut ignored = false;

        // Get all directories that could have patterns affecting this path
        let mut applicable_dirs: Vec<_> = self
            .patterns_by_dir
            .keys()
            .filter(|dir| {
                // Include if the pattern directory is an ancestor of the path
                path.starts_with(dir) || dir == &&self.root
            })
            .collect();

        // Sort by depth (root first, then deeper directories)
        applicable_dirs.sort_by_key(|dir| dir.components().count());

        for dir in applicable_dirs {
            if let Some(patterns) = self.patterns_by_dir.get(dir) {
                // Calculate relative path from this pattern directory
                let pattern_relative_path = if dir == &self.root {
                    path_str.clone()
                } else {
                    match path.strip_prefix(dir) {
                        Ok(rel) => rel.to_string_lossy().replace("\\", "/"),
                        Err(_) => path_str.clone(), // Fallback to full relative path
                    }
                };

                for pattern in patterns {
                    if pattern.matches(&pattern_relative_path, is_dir)
                        || pattern.matches(&path_str, is_dir)
                    {
                        ignored = !pattern.negation;
                        trace!(
                            "Pattern '{}' from {} {} path '{}'",
                            pattern.original,
                            dir.display(),
                            if ignored { "ignores" } else { "includes" },
                            path_str
                        );
                    }
                }
            }
        }

        trace!(
            "Final decision for '{}': {}",
            path_str,
            if ignored { "IGNORE" } else { "INCLUDE" }
        );
        ignored
    }

    /// Check if a directory should be skipped during traversal
    /// This is an optimized version for directory-level checking that doesn't
    /// perform parent directory lookups to avoid infinite recursion during walkdir
    pub fn should_skip_dir(&self, path: &Path) -> bool {
        if !path.is_dir() {
            return false;
        }

        let relative_path = match path.strip_prefix(&self.root) {
            Ok(rel) => rel,
            Err(_) => {
                // Path is not under root, don't skip
                return false;
            }
        };

        // Convert to forward slashes for consistent matching
        let path_str = relative_path.to_string_lossy().replace("\\", "/");

        trace!("Checking if directory should be skipped: {}", path_str);

        // Check patterns from all applicable directories, starting from root to specific
        let mut ignored = false;

        // Get all directories that could have patterns affecting this path
        let mut applicable_dirs: Vec<_> = self
            .patterns_by_dir
            .keys()
            .filter(|dir| {
                // Include if the pattern directory is an ancestor of the path
                path.starts_with(dir) || dir == &&self.root
            })
            .collect();

        // Sort by depth (root first, then deeper directories)
        applicable_dirs.sort_by_key(|dir| dir.components().count());

        for dir in applicable_dirs {
            if let Some(patterns) = self.patterns_by_dir.get(dir) {
                // Calculate relative path from this pattern directory
                let pattern_relative_path = if dir == &self.root {
                    path_str.clone()
                } else {
                    match path.strip_prefix(dir) {
                        Ok(rel) => rel.to_string_lossy().replace("\\", "/"),
                        Err(_) => path_str.clone(), // Fallback to full relative path
                    }
                };

                for pattern in patterns {
                    if pattern.matches(&pattern_relative_path, true)
                        || pattern.matches(&path_str, true)
                    {
                        ignored = !pattern.negation;
                        trace!(
                            "Pattern '{}' from {} {} directory '{}'",
                            pattern.original,
                            dir.display(),
                            if ignored { "ignores" } else { "includes" },
                            path_str
                        );
                    }
                }
            }
        }

        trace!(
            "Directory skip decision for '{}': {}",
            path_str,
            if ignored { "SKIP" } else { "CONTINUE" }
        );
        ignored
    }
}

impl IgnorePattern {
    /// Create a new ignore pattern from a string
    fn new(pattern_str: &str) -> Result<Self> {
        let mut pattern_str = pattern_str.trim();

        // Check for negation
        let negation = pattern_str.starts_with('!');
        if negation {
            pattern_str = &pattern_str[1..];
        }

        // Check for directory-only pattern
        let directory_only = pattern_str.ends_with('/');
        if directory_only {
            pattern_str = &pattern_str[..pattern_str.len() - 1];
        }

        // Convert gitignore patterns to glob patterns
        let glob_pattern = convert_gitignore_to_glob(pattern_str);

        let pattern = Pattern::new(&glob_pattern)
            .map_err(|e| anyhow::anyhow!("Invalid glob pattern: {}", e))?;

        Ok(Self {
            pattern,
            original: pattern_str.to_string(),
            negation,
            directory_only,
        })
    }

    /// Check if this pattern matches the given path
    fn matches(&self, path: &str, is_dir: bool) -> bool {
        // If this is a directory-only pattern and the path is not a directory, no match
        if self.directory_only && !is_dir {
            return false;
        }

        // Try matching the full path
        if self.pattern.matches(path) {
            return true;
        }

        // Also try matching just the filename for patterns that don't contain '/'
        if !self.original.contains('/') {
            if let Some(filename) = Path::new(path).file_name() {
                if let Some(filename_str) = filename.to_str() {
                    return self.pattern.matches(filename_str);
                }
            }
        }

        false
    }
}

/// Convert gitignore-style patterns to glob patterns
fn convert_gitignore_to_glob(pattern: &str) -> String {
    let mut result = String::new();

    // Handle leading slash (absolute from root)
    if let Some(stripped) = pattern.strip_prefix('/') {
        result.push_str(stripped);
    } else {
        // Pattern can match at any level
        result.push_str("**/");
        result.push_str(pattern);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_basic_ignore_patterns() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create .plexifyignore file
        fs::write(root.join(".plexifyignore"), "*.tmp\nDownloads/\ntools").unwrap();

        // Create test directory structure
        fs::create_dir_all(root.join("Downloads")).unwrap();
        fs::create_dir_all(root.join("tools")).unwrap();
        fs::create_dir_all(root.join("Anime")).unwrap();
        fs::write(root.join("test.tmp"), "").unwrap();
        fs::write(root.join("video.mkv"), "").unwrap();

        let filter = IgnoreFilter::new(root.to_path_buf()).unwrap();

        // Should ignore
        assert!(filter.should_ignore(&root.join("test.tmp")));
        assert!(filter.should_ignore(&root.join("Downloads")));
        assert!(filter.should_ignore(&root.join("tools")));

        // Should not ignore
        assert!(!filter.should_ignore(&root.join("video.mkv")));
        assert!(!filter.should_ignore(&root.join("Anime")));
    }

    #[test]
    fn test_nested_ignore_files() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create root .plexifyignore
        fs::write(root.join(".plexifyignore"), "*.tmp").unwrap();

        // Create nested directory with its own .plexifyignore
        fs::create_dir_all(root.join("Series")).unwrap();
        fs::write(root.join("Series/.plexifyignore"), "old/\n!important.mkv").unwrap();

        // Create test files
        fs::create_dir_all(root.join("Series/old")).unwrap();
        fs::write(root.join("test.tmp"), "").unwrap();
        fs::write(root.join("Series/show.mkv"), "").unwrap();
        fs::write(root.join("Series/old/episode.mkv"), "").unwrap();
        fs::write(root.join("Series/important.mkv"), "").unwrap();

        let filter = IgnoreFilter::new(root.to_path_buf()).unwrap();

        // Root patterns should apply
        assert!(filter.should_ignore(&root.join("test.tmp")));

        // Nested patterns should apply
        assert!(filter.should_ignore(&root.join("Series/old")));
        assert!(filter.should_ignore(&root.join("Series/old/episode.mkv")));

        // Should not ignore
        assert!(!filter.should_ignore(&root.join("Series/show.mkv")));
        assert!(!filter.should_ignore(&root.join("Series/important.mkv")));
    }

    #[test]
    fn test_directory_only_patterns() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Pattern with trailing slash should only match directories
        fs::write(root.join(".plexifyignore"), "temp/").unwrap();

        fs::create_dir_all(root.join("temp")).unwrap();
        fs::write(root.join("temp_file"), "").unwrap();

        let filter = IgnoreFilter::new(root.to_path_buf()).unwrap();

        // Should ignore directory
        assert!(filter.should_ignore(&root.join("temp")));

        // Should not ignore file with similar name
        assert!(!filter.should_ignore(&root.join("temp_file")));
    }

    #[test]
    fn test_negation_patterns() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        fs::write(root.join(".plexifyignore"), "*.mkv\n!important.mkv").unwrap();

        fs::write(root.join("video.mkv"), "").unwrap();
        fs::write(root.join("important.mkv"), "").unwrap();
        fs::write(root.join("test.mp4"), "").unwrap();

        let filter = IgnoreFilter::new(root.to_path_buf()).unwrap();

        // Should ignore .mkv files
        assert!(filter.should_ignore(&root.join("video.mkv")));

        // Should not ignore important.mkv due to negation
        assert!(!filter.should_ignore(&root.join("important.mkv")));

        // Should not ignore other files
        assert!(!filter.should_ignore(&root.join("test.mp4")));
    }

    #[test]
    fn test_convert_gitignore_to_glob() {
        assert_eq!(convert_gitignore_to_glob("*.tmp"), "**/*.tmp");
        assert_eq!(convert_gitignore_to_glob("/Downloads"), "Downloads");
        assert_eq!(convert_gitignore_to_glob("tools"), "**/tools");
        assert_eq!(convert_gitignore_to_glob("path/to/file"), "**/path/to/file");
    }

    #[test]
    fn test_should_skip_dir() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        // Create .plexifyignore file
        fs::write(root.join(".plexifyignore"), "Downloads/\n*.tmp\ntools").unwrap();

        // Create test directory structure
        fs::create_dir_all(root.join("Downloads")).unwrap();
        fs::create_dir_all(root.join("tools")).unwrap();
        fs::create_dir_all(root.join("Anime")).unwrap();

        let filter = IgnoreFilter::new(root.to_path_buf()).unwrap();

        // Should skip ignored directories
        assert!(filter.should_skip_dir(&root.join("Downloads")));
        assert!(filter.should_skip_dir(&root.join("tools")));

        // Should not skip non-ignored directories
        assert!(!filter.should_skip_dir(&root.join("Anime")));

        // Should not skip root directory
        assert!(!filter.should_skip_dir(root));
    }
}
