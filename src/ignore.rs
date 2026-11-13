use anyhow::{bail, Context, Result};
use glob::Pattern;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tracing::{debug, trace, warn};
use walkdir::WalkDir;

use crate::paths::to_forward_slashes;
use crate::queue::QueueDirectory;

/// What a walk could not read, naming the path the way every other line does.
/// walkdir's own message renders it natively.
fn unreadable(error: &walkdir::Error) -> String {
    match (error.path(), error.io_error()) {
        (Some(path), Some(io)) => format!("{}: {io}", to_forward_slashes(path)),
        _ => error.to_string(),
    }
}

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
        Self::for_scope(root.clone(), &root)
    }

    /// Create a filter for a run that only walks part of the tree.
    ///
    /// Patterns still belong to the root - a rule written there governs the whole
    /// library, and narrowing a run must not quietly escape it. But only two sets
    /// of `.plexifyignore` files can affect the subtree being walked: the ones in
    /// directories above it, and the ones inside it. Everything else is a sibling
    /// branch whose rules can never match a path under `scan_path`, so walking the
    /// whole library to find them would cost the run its reason for being narrow.
    pub fn for_scope(root: PathBuf, scan_path: &Path) -> Result<Self> {
        let mut filter = Self {
            patterns_by_dir: HashMap::new(),
            root,
        };

        filter.load_ignore_files_above(scan_path)?;
        filter.load_ignore_files_under(scan_path)?;

        debug!(
            "Loaded .plexifyignore patterns from {} directories",
            filter.patterns_by_dir.len()
        );

        Ok(filter)
    }

    /// Load the ignore files in the directories between the root and the scan
    /// path, which govern it without being inside it.
    fn load_ignore_files_above(&mut self, scan_path: &Path) -> Result<()> {
        let root = self.root.clone();
        let mut ancestors: Vec<PathBuf> = scan_path
            .ancestors()
            .skip(1)
            .take_while(|ancestor| ancestor.starts_with(&root))
            .map(|ancestor| ancestor.to_path_buf())
            .collect();
        ancestors.push(root);
        ancestors.reverse();
        ancestors.dedup();

        for ancestor in ancestors {
            self.load_ignore_file(&ancestor)?;
        }

        Ok(())
    }

    /// Load every ignore file inside the subtree being walked.
    ///
    /// A directory that cannot be read may hold a `.plexifyignore`, so it is an
    /// error rather than a gap. An ignored directory is pruned instead of read,
    /// which is also how an unreadable one such as `lost+found` is excluded: its
    /// rules could never apply, because everything beneath it is already ignored.
    /// A directory's own rules are loaded as it is entered, so they decide which
    /// of its children are pruned.
    fn load_ignore_files_under(&mut self, scan_path: &Path) -> Result<()> {
        let mut walk = WalkDir::new(scan_path).follow_links(false).into_iter();
        while let Some(entry) = walk.next() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => bail!(
                    "could not look for .plexifyignore files in {}: {}",
                    to_forward_slashes(scan_path),
                    unreadable(&error)
                ),
            };
            if !entry.file_type().is_dir() {
                continue;
            }

            let dir = entry.path();
            if dir != scan_path && self.should_skip_dir(dir) {
                walk.skip_current_dir();
                continue;
            }

            self.load_ignore_file(dir)?;
        }

        Ok(())
    }

    /// Load the patterns of `dir`'s .plexifyignore, if it has one.
    ///
    /// Only a file that is not there is skipped. Every other failure is an
    /// error: a rule that is silently dropped lets `work` transcode, or
    /// `validate --fix` rename, a file the user excluded.
    fn load_ignore_file(&mut self, dir: &Path) -> Result<()> {
        let ignore_file = dir.join(".plexifyignore");
        let content = match fs::read_to_string(&ignore_file) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            read => read
                .with_context(|| format!("could not read {}", to_forward_slashes(&ignore_file)))?,
        };

        let patterns: Vec<IgnorePattern> = content
            .lines()
            .enumerate()
            .map(|(line_num, line)| (line_num, line.trim()))
            // Skip empty lines and comments
            .filter(|(_, trimmed)| !trimmed.is_empty() && !trimmed.starts_with('#'))
            .map(|(line_num, trimmed)| {
                IgnorePattern::new(trimmed).with_context(|| {
                    format!(
                        "{}:{}: invalid pattern '{}'",
                        to_forward_slashes(&ignore_file),
                        line_num + 1,
                        trimmed
                    )
                })
            })
            .collect::<Result<_>>()?;

        if !patterns.is_empty() {
            debug!(
                "Loaded {} patterns from {}",
                patterns.len(),
                to_forward_slashes(&ignore_file)
            );
            self.patterns_by_dir.insert(dir.to_path_buf(), patterns);
        }

        Ok(())
    }

    /// Whether a rule excludes `path`, or a directory it is inside.
    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        path.ancestors()
            .skip(1)
            .take_while(|ancestor| *ancestor != self.root && ancestor.starts_with(&self.root))
            .any(|ancestor| self.rules_exclude(ancestor, true))
            || self.rules_exclude(path, is_dir)
    }

    /// Whether a walk should prune this directory and everything beneath it.
    ///
    /// Only the directory itself is judged, not its ancestors: a walk that
    /// prunes reaches a directory only through ancestors it has already let in.
    pub fn should_skip_dir(&self, path: &Path) -> bool {
        path.is_dir() && self.rules_exclude(path, true)
    }

    /// Walk `scan_path` for the media files no rule excludes.
    ///
    /// Ignored directories and a work root's queue directories are pruned
    /// rather than walked and discarded. A part of the tree that cannot be read
    /// is a warning naming it, and the walk goes on without it.
    pub fn media_files(&self, scan_path: &Path) -> MediaFiles<'_> {
        MediaFiles {
            filter: self,
            walk: WalkDir::new(scan_path).follow_links(false).into_iter(),
            directories: 0,
            files: 0,
            ignored: 0,
        }
    }

    /// Whether the rules alone exclude `path`. The last rule to match decides,
    /// read from the root's rules down to those of the directory nearest it.
    fn rules_exclude(&self, path: &Path, is_dir: bool) -> bool {
        let relative_path = match path.strip_prefix(&self.root) {
            Ok(rel) => rel,
            Err(_) => {
                // Path is not under root, don't ignore
                return false;
            }
        };

        let path_str = to_forward_slashes(relative_path);

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
                        Ok(rel) => to_forward_slashes(rel),
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
                            to_forward_slashes(dir),
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
}

/// The media files of one walk, and a count of what else it passed.
///
/// The counts are complete once the iterator is exhausted.
pub struct MediaFiles<'a> {
    filter: &'a IgnoreFilter,
    walk: walkdir::IntoIter,
    /// Directories walked beneath the starting one.
    pub directories: usize,
    /// Files no rule excludes, media or not.
    pub files: usize,
    /// Files a rule excludes inside a directory that was walked.
    pub ignored: usize,
}

impl Iterator for MediaFiles<'_> {
    type Item = PathBuf;

    fn next(&mut self) -> Option<PathBuf> {
        loop {
            let entry = match self.walk.next()? {
                Ok(entry) => entry,
                Err(error) => {
                    warn!(
                        "⚠️ Skipped, because it could not be read: {}",
                        unreadable(&error)
                    );
                    continue;
                }
            };
            let path = entry.path();

            // A link is followed only far enough to tell a directory from a
            // file; the walk never descends through one.
            let is_dir = entry.file_type().is_dir() || (entry.path_is_symlink() && path.is_dir());
            if is_dir {
                if entry.depth() == 0 {
                    continue;
                }
                // A work root inside the media root is the mistake people make
                // constantly, and its jobs and half-written encodes are not
                // library files.
                if is_queue_directory(path) || self.filter.should_skip_dir(path) {
                    debug!("🚫 Skipping entire directory: {}", to_forward_slashes(path));
                    self.walk.skip_current_dir();
                } else {
                    self.directories += 1;
                }
                continue;
            }

            if self.filter.is_ignored(path, false) {
                debug!("🚫 Ignoring path: {}", to_forward_slashes(path));
                self.ignored += 1;
                continue;
            }

            self.files += 1;
            if is_media_file(path) {
                return Some(entry.into_path());
            }
        }
    }
}

/// One of the four directories a work root is made of.
fn is_queue_directory(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        QueueDirectory::ALL
            .iter()
            .any(|directory| name == directory.on_disk_name())
    })
}

/// The containers a library's media comes in: what validate names, audit
/// probes and fix moves with its sidecars. Scan converts only some of them,
/// and `JobProcessor::determine_file_type` says which.
const MEDIA_EXTENSIONS: &[&str] = &["mkv", "mp4", "avi", "webm", "mov", "m4v"];

/// Whether `extension`, without its dot and in any case, is a media container.
pub fn is_media_extension(extension: &str) -> bool {
    MEDIA_EXTENSIONS
        .iter()
        .any(|known| extension.eq_ignore_ascii_case(known))
}

fn is_media_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| is_media_extension(&extension.to_string_lossy()))
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
        assert!(filter.is_ignored(&root.join("test.tmp"), false));
        assert!(filter.is_ignored(&root.join("Downloads"), true));
        assert!(filter.is_ignored(&root.join("tools"), true));

        // Should not ignore
        assert!(!filter.is_ignored(&root.join("video.mkv"), false));
        assert!(!filter.is_ignored(&root.join("Anime"), true));
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
        assert!(filter.is_ignored(&root.join("test.tmp"), false));

        // Nested patterns should apply
        assert!(filter.is_ignored(&root.join("Series/old"), true));
        assert!(filter.is_ignored(&root.join("Series/old/episode.mkv"), false));

        // Should not ignore
        assert!(!filter.is_ignored(&root.join("Series/show.mkv"), false));
        assert!(!filter.is_ignored(&root.join("Series/important.mkv"), false));
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
        assert!(filter.is_ignored(&root.join("temp"), true));

        // Should not ignore file with similar name
        assert!(!filter.is_ignored(&root.join("temp_file"), false));
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
        assert!(filter.is_ignored(&root.join("video.mkv"), false));

        // Should not ignore important.mkv due to negation
        assert!(!filter.is_ignored(&root.join("important.mkv"), false));

        // Should not ignore other files
        assert!(!filter.is_ignored(&root.join("test.mp4"), false));
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

    #[test]
    fn a_scoped_filter_reads_the_rules_above_it_and_inside_it() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        fs::write(
            root.join(".plexifyignore"),
            "*.tmp
",
        )
        .unwrap();

        let series = root.join("Series/Elementary");
        fs::create_dir_all(series.join("Season 06")).unwrap();
        fs::write(
            series.join(".plexifyignore"),
            "*.nfo
",
        )
        .unwrap();

        // A sibling branch that cannot affect the scoped subtree.
        let sibling = root.join("Series/Firefly");
        fs::create_dir_all(&sibling).unwrap();
        fs::write(
            sibling.join(".plexifyignore"),
            "*.mkv
",
        )
        .unwrap();

        let filter = IgnoreFilter::for_scope(root.to_path_buf(), &series).unwrap();

        assert!(
            filter.is_ignored(&series.join("Season 06/artwork.tmp"), false),
            "a rule at the library root still governs a scoped run"
        );
        assert!(
            filter.is_ignored(&series.join("Season 06/episode.nfo"), false),
            "a rule inside the scope applies"
        );
        assert!(
            !filter.is_ignored(&series.join("Season 06/episode.mkv"), false),
            "a sibling's rules were never in scope, and are not loaded"
        );
    }

    #[test]
    fn a_whole_library_filter_still_sees_every_rule() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();

        let deep = root.join("Series/Firefly/Season 01");
        fs::create_dir_all(&deep).unwrap();
        fs::write(
            deep.join(".plexifyignore"),
            "*.mkv
",
        )
        .unwrap();

        let filter = IgnoreFilter::new(root.to_path_buf()).unwrap();

        assert!(filter.is_ignored(&deep.join("episode.mkv"), false));
    }

    #[test]
    fn an_invalid_pattern_is_refused_with_its_file_and_line() {
        let temp_dir = TempDir::new().unwrap();
        let ignore_file = temp_dir.path().join(".plexifyignore");
        fs::write(&ignore_file, "# rules\nDownloads/\nzz[\n").unwrap();

        let error = IgnoreFilter::new(temp_dir.path().to_path_buf()).unwrap_err();

        let message = format!("{error:#}");
        assert!(
            message.contains(&format!("{}:3", to_forward_slashes(&ignore_file))),
            "names the file and line: {message}"
        );
        assert!(message.contains("'zz['"), "names the pattern: {message}");
    }

    #[test]
    fn an_ignore_file_that_is_not_utf8_is_refused_with_its_name() {
        let temp_dir = TempDir::new().unwrap();
        let ignore_file = temp_dir.path().join(".plexifyignore");
        // A Latin-1 'é', which is not UTF-8.
        fs::write(&ignore_file, b"Downloads/\n# caf\xe9\n").unwrap();

        let error = IgnoreFilter::new(temp_dir.path().to_path_buf()).unwrap_err();

        let message = format!("{error:#}");
        assert!(
            message.contains(&to_forward_slashes(&ignore_file)),
            "names the file: {message}"
        );
    }

    /// A `.plexifyignore` that exists but is not a readable file is an error,
    /// in the root and in a directory below it. A directory is the trigger
    /// because it works on every platform; a file this user may not read is
    /// the same branch, and is covered on Unix below.
    #[test]
    fn an_ignore_file_that_exists_but_cannot_be_read_is_refused_with_its_name() {
        for at in ["", "Series"] {
            let temp_dir = TempDir::new().unwrap();
            let ignore_file = temp_dir.path().join(at).join(".plexifyignore");
            fs::create_dir_all(&ignore_file).unwrap();

            let result = IgnoreFilter::new(temp_dir.path().to_path_buf());

            let message = format!("{:#}", result.unwrap_err());
            assert!(
                message.contains(&to_forward_slashes(&ignore_file)),
                "names {}: {message}",
                to_forward_slashes(&ignore_file)
            );
        }
    }

    /// Make `dir` unreadable, or report that this process can read it anyway
    /// (as root can), in which case there is nothing to test.
    #[cfg(unix)]
    fn lock(dir: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).unwrap();
        let locked = fs::read_dir(dir).is_err();
        if !locked {
            eprintln!("skipping: this process can read a mode 000 directory");
        }
        locked
    }

    #[cfg(unix)]
    fn unlock(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Deny this user the right to list `dir`.
    #[cfg(windows)]
    fn lock(dir: &Path) -> bool {
        icacls(
            dir,
            &[
                "/deny",
                &format!("{}:(RD)", std::env::var("USERNAME").unwrap()),
            ],
        );
        let locked = fs::read_dir(dir).is_err();
        if !locked {
            eprintln!("skipping: this process can list a directory it was denied");
        }
        locked
    }

    #[cfg(windows)]
    fn unlock(dir: &Path) {
        icacls(dir, &["/remove:d", &std::env::var("USERNAME").unwrap()]);
    }

    #[cfg(windows)]
    fn icacls(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("icacls")
            .arg(dir)
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "icacls {args:?} failed on {dir:?}");
    }

    /// Run `f` and return what it logged, as `main`'s subscriber would print it.
    fn logs_of<T>(f: impl FnOnce() -> T) -> (T, String) {
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let sink = Sink::default();
        let writer = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        let result = tracing::subscriber::with_default(subscriber, f);
        let logs = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
        (result, logs)
    }

    #[test]
    fn a_walk_yields_the_media_files_no_rule_or_queue_directory_excludes() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();
        fs::write(root.join(".plexifyignore"), "Downloads/\n*.tmp.mkv\n").unwrap();
        let season = root.join("Series").join("Show").join("Season 01");
        fs::create_dir_all(&season).unwrap();
        for name in ["a.mkv", "b.MP4", "c.mov", "d.m4v", "e.webm", "f.avi"] {
            fs::write(season.join(name), "").unwrap();
        }
        for name in ["a.en.srt", "e.vtt", "show.nfo", "x.tmp.mkv", "no-extension"] {
            fs::write(season.join(name), "").unwrap();
        }
        for excluded in [
            "Downloads",
            "_queue",
            "_in_progress",
            "Series/_completed",
            "_failed",
        ] {
            fs::create_dir_all(root.join(excluded)).unwrap();
            fs::write(root.join(excluded).join("film.mkv"), "").unwrap();
        }

        let filter = IgnoreFilter::new(root.to_path_buf()).unwrap();
        let mut walk = filter.media_files(root);
        let mut found: Vec<_> = walk.by_ref().collect();
        found.sort();

        let mut expected: Vec<_> = ["a.mkv", "b.MP4", "c.mov", "d.m4v", "e.webm", "f.avi"]
            .iter()
            .map(|name| season.join(name))
            .collect();
        expected.sort();
        assert_eq!(found, expected);
        assert_eq!(walk.directories, 3, "Series, Show and Season 01");
        assert_eq!(
            walk.files, 11,
            "every file but the ignored one, media or not, .plexifyignore too"
        );
        assert_eq!(walk.ignored, 1, "x.tmp.mkv");
    }

    /// Loading the rules refuses a directory it cannot read, so the walk meets
    /// one only when the tree changes after the rules were loaded.
    #[test]
    fn a_directory_the_walk_cannot_read_is_a_warning_naming_it() {
        let temp_dir = TempDir::new().unwrap();
        let root = temp_dir.path();
        let locked = root.join("Series").join("Locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("hidden.mkv"), "").unwrap();
        fs::write(root.join("Series").join("episode.mkv"), "").unwrap();
        let filter = IgnoreFilter::new(root.to_path_buf()).unwrap();
        if !lock(&locked) {
            return;
        }

        let (found, logs) = logs_of(|| filter.media_files(root).collect::<Vec<_>>());
        unlock(&locked);

        assert_eq!(
            found,
            vec![root.join("Series").join("episode.mkv")],
            "the rest of the library is still walked"
        );
        let warning = logs
            .lines()
            .find(|line| line.contains("WARN"))
            .unwrap_or_else(|| panic!("no warning in: {logs}"));
        assert!(
            warning.contains(&to_forward_slashes(&locked)),
            "names the directory: {warning}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_that_cannot_be_searched_for_rules_is_an_error() {
        let temp_dir = TempDir::new().unwrap();
        let locked = temp_dir.path().join("Series/Locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join(".plexifyignore"), "*.mkv\n").unwrap();
        if !lock(&locked) {
            return;
        }

        let result = IgnoreFilter::new(temp_dir.path().to_path_buf());
        unlock(&locked);

        let message = format!("{:#}", result.unwrap_err());
        assert!(
            message.contains(&to_forward_slashes(&locked)),
            "names the directory: {message}"
        );
    }

    /// A directory that can be listed but not searched: the walk finds its
    /// `.plexifyignore`, but this user cannot open it.
    #[cfg(unix)]
    #[test]
    fn an_ignore_file_this_user_cannot_open_is_an_error() {
        use std::os::unix::fs::PermissionsExt;
        let temp_dir = TempDir::new().unwrap();
        let locked = temp_dir.path().join("Series");
        fs::create_dir_all(&locked).unwrap();
        let ignore_file = locked.join(".plexifyignore");
        fs::write(&ignore_file, "*.mkv\n").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
        if fs::metadata(&ignore_file).is_ok() {
            unlock(&locked);
            eprintln!("skipping: this process can search a mode 644 directory");
            return;
        }

        let result = IgnoreFilter::new(temp_dir.path().to_path_buf());
        unlock(&locked);

        let message = format!("{:#}", result.unwrap_err());
        assert!(
            message.contains(&to_forward_slashes(&ignore_file)),
            "names {}: {message}",
            to_forward_slashes(&ignore_file)
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_ignored_directory_is_not_searched_for_rules() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join(".plexifyignore"), "lost+found/\n").unwrap();
        let locked = temp_dir.path().join("lost+found");
        fs::create_dir_all(&locked).unwrap();
        if !lock(&locked) {
            return;
        }

        let result = IgnoreFilter::new(temp_dir.path().to_path_buf());
        unlock(&locked);

        assert!(result.unwrap().should_skip_dir(&locked));
    }
}
