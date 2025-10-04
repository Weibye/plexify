use anyhow::{anyhow, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::ignore::IgnoreFilter;

/// Media file extensions that should be validated
const MEDIA_EXTENSIONS: &[&str] = &["mkv", "mp4", "avi", "webm", "mov", "m4v"];

/// Content type for categorizing naming patterns
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContentType {
    Series,
    Movie,
}

/// Directory mapping configuration
const DIRECTORY_MAPPING: &[(&str, ContentType)] = &[
    ("Anime", ContentType::Series),
    ("Series", ContentType::Series),
    ("Movies", ContentType::Movie),
];

/// Naming scheme patterns for different content types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamingPatterns {
    /// All naming patterns
    pub patterns: Vec<NamingPattern>,
}

/// Unified naming pattern with precompiled regex
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamingPattern {
    pub description: String,
    pub pattern: String,
    pub example: String,
    pub content_type: ContentType,
    #[serde(skip)]
    pub compiled_regex: Option<Regex>,
}

/// Validation issue found in a media file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationIssue {
    pub file_path: PathBuf,
    pub issue_type: IssueType,
    pub description: String,
    pub suggested_path: Option<PathBuf>,
}

/// Types of naming issues
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IssueType {
    ShowNaming,
    MovieNaming,
    DirectoryStructure,
    FileExtension,
    UnknownContentType,
}

/// Validation report containing all issues found
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub scanned_files: usize,
    pub issues: Vec<ValidationIssue>,
    pub fixed_files: Vec<FixedFile>,
    pub patterns_used: NamingPatterns,
    pub scan_path: PathBuf,
    pub validation_time: Duration,
}

/// Record of a file that was fixed
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixedFile {
    pub original_path: PathBuf,
    pub new_path: PathBuf,
    pub issue_type: IssueType,
}

/// Episode information parsed from filename
#[derive(Debug)]
struct EpisodeInfo {
    season: u32,
    episode: u32,
    title: String,
    metadata: Vec<String>,
}

/// Command to validate Plex naming scheme conformity
pub struct ValidateCommand {
    media_root: PathBuf,
    patterns: NamingPatterns,
    compiled_patterns: Vec<CompiledPattern>,
    fix_mode: bool,
}

/// Internal structure for compiled regex patterns
#[derive(Debug, Clone)]
struct CompiledPattern {
    regex: Regex,
}

impl Default for NamingPatterns {
    fn default() -> Self {
        Self {
            patterns: vec![
                // Anime patterns (shows)
                NamingPattern {
                    description: "Standard Anime format".to_string(),
                    pattern: r"^Anime/[^/]+(?:\s*\{tvdb-\d+\})?/Season \d{2}(?:\s*-[^/]*)*/[^/]+ - S\d{2}E\d{2} - [^/]+(?:\s*\[[^\]]+\])*\.\w+$".to_string(),
                    example: "Anime/Attack on Titan/Season 01/Attack on Titan - S01E01 - To You, in 2000 Years [720p].mkv".to_string(),
                    content_type: ContentType::Series,
                    compiled_regex: None,
                },
                NamingPattern {
                    description: "Alternative Anime format".to_string(),
                    pattern: r"^Anime/[^/]+(?:\s*\{tvdb-\d+\})?/Season \d{2}(?:\s*-[^/]*)*/[^/]+ S\d{2}E\d{2} [^/]+(?:\s*\[[^\]]+\])*\.\w+$".to_string(),
                    example: "Anime/Attack on Titan/Season 01/Attack on Titan S01E01 To You, in 2000 Years [720p].mkv".to_string(),
                    content_type: ContentType::Series,
                    compiled_regex: None,
                },
                // Extended anime patterns for high episode numbers (common in long-running anime)
                NamingPattern {
                    description: "Extended Anime format (high episode numbers)".to_string(),
                    pattern: r"^Anime/[^/]+(?:\s*\{tvdb-\d+\})?/Season \d{2}(?:\s*-[^/]*)*/[^/]+ - S\d{2}E\d{3} - [^/]+(?:\s*\[[^\]]+\])*\.\w+$".to_string(),
                    example: "Anime/One Piece/Season 11/One Piece - S11E397 - Episode 397 [720p][ybis].mkv".to_string(),
                    content_type: ContentType::Series,
                    compiled_regex: None,
                },
                // Anime patterns without episode titles
                NamingPattern {
                    description: "Anime format without episode title".to_string(),
                    pattern: r"^Anime/[^/]+(?:\s*\{tvdb-\d+\})?/Season \d{2}(?:\s*-[^/]*)*/[^/]+ - S\d{2}E\d{2}(?:\s*\[[^\]]+\])*\.\w+$".to_string(),
                    example: "Anime/Attack on Titan/Season 01/Attack on Titan - S01E01 [720p].mkv".to_string(),
                    content_type: ContentType::Series,
                    compiled_regex: None,
                },
                NamingPattern {
                    description: "Extended Anime format without episode title (high episode numbers)".to_string(),
                    pattern: r"^Anime/[^/]+(?:\s*\{tvdb-\d+\})?/Season \d{2}(?:\s*-[^/]*)*/[^/]+ - S\d{2}E\d{3}(?:\s*\[[^\]]+\])*\.\w+$".to_string(),
                    example: "Anime/One Piece/Season 11/One Piece - S11E397 [720p][ybis].mkv".to_string(),
                    content_type: ContentType::Series,
                    compiled_regex: None,
                },

                // Series patterns (shows)  
                NamingPattern {
                    description: "Standard Series format".to_string(),
                    pattern: r"^Series/[^/]+(?:\s*\{tvdb-\d+\})?/Season \d{2}(?:\s*-[^/]*)*/[^/]+ - S\d{2}E\d{2} - [^/]+(?:\s*\[[^\]]+\])*\.\w+$".to_string(),
                    example: "Series/Breaking Bad/Season 01/Breaking Bad - S01E01 - Pilot [720p].mkv".to_string(),
                    content_type: ContentType::Series,
                    compiled_regex: None,
                },
                NamingPattern {
                    description: "Alternative Series format".to_string(),
                    pattern: r"^Series/[^/]+(?:\s*\{tvdb-\d+\})?/Season \d{2}(?:\s*-[^/]*)*/[^/]+ S\d{2}E\d{2} [^/]+(?:\s*\[[^\]]+\])*\.\w+$".to_string(),
                    example: "Series/Breaking Bad (2008) {tvdb-296861}/Season 01/Breaking Bad S01E01 Pilot [720p].mkv".to_string(),
                    content_type: ContentType::Series,
                    compiled_regex: None,
                },
                NamingPattern {
                    description: "Simple Series format".to_string(),
                    pattern: r"^Series/[^/]+(?:\s*\{tvdb-\d+\})?/Season \d{2}(?:\s*-[^/]*)*/S\d{2}E\d{2} - [^/]+(?:\s*\[[^\]]+\])*\.\w+$".to_string(),
                    example: "Series/Breaking Bad/Season 01/S01E01 - Pilot [720p].mkv".to_string(),
                    content_type: ContentType::Series,
                    compiled_regex: None,
                },
                // Series patterns without episode titles
                NamingPattern {
                    description: "Series format without episode title".to_string(),
                    pattern: r"^Series/[^/]+(?:\s*\{tvdb-\d+\})?/Season \d{2}(?:\s*-[^/]*)*/[^/]+ - S\d{2}E\d{2}(?:\s*\[[^\]]+\])*\.\w+$".to_string(),
                    example: "Series/The Expanse/Season 01/The Expanse - S01E01 [720p].mkv".to_string(),
                    content_type: ContentType::Series,
                    compiled_regex: None,
                },
                NamingPattern {
                    description: "Alternative Series format without episode title".to_string(),
                    pattern: r"^Series/[^/]+(?:\s*\{tvdb-\d+\})?/Season \d{2}(?:\s*-[^/]*)*/[^/]+ S\d{2}E\d{2}(?:\s*\[[^\]]+\])*\.\w+$".to_string(),
                    example: "Series/The Expanse/Season 01/The Expanse S01E01 [720p].mkv".to_string(),
                    content_type: ContentType::Series,
                    compiled_regex: None,
                },
                // Specials folder patterns
                NamingPattern {
                    description: "Series Specials format".to_string(),
                    pattern: r"^Series/[^/]+(?:\s*\{tvdb-\d+\})?/Specials/[^/]+ - S\d{2}E\d{2} - [^/]+(?:\s*\[[^\]]+\])*\.\w+$".to_string(),
                    example: "Series/The Great British Bake Off/Specials/The Great British Bake Off - S01E07 - Get Baking - Mary Berry [360p].mkv".to_string(),
                    content_type: ContentType::Series,
                    compiled_regex: None,
                },
                NamingPattern {
                    description: "Series Specials format without episode title".to_string(),
                    pattern: r"^Series/[^/]+(?:\s*\{tvdb-\d+\})?/Specials/[^/]+ - S\d{2}E\d{2}(?:\s*\[[^\]]+\])*\.\w+$".to_string(),
                    example: "Series/The Great British Bake Off/Specials/The Great British Bake Off - S01E07 [360p].mkv".to_string(),
                    content_type: ContentType::Series,
                    compiled_regex: None,
                },
                // Anime Specials patterns
                NamingPattern {
                    description: "Anime Specials format".to_string(),
                    pattern: r"^Anime/[^/]+(?:\s*\{tvdb-\d+\})?/Specials/[^/]+ - S\d{2}E\d{2} - [^/]+(?:\s*\[[^\]]+\])*\.\w+$".to_string(),
                    example: "Anime/Attack on Titan/Specials/Attack on Titan - S01E01 - Behind the Scenes [720p].mkv".to_string(),
                    content_type: ContentType::Series,
                    compiled_regex: None,
                },

                // Movie patterns
                NamingPattern {
                    description: "Standard Movie format".to_string(),
                    pattern: r"^Movies/[^/]+ \(\d{4}\)/[^/]+ \(\d{4}\)\.\w+$".to_string(),
                    example: "Movies/The Dark Knight (2008)/The Dark Knight (2008).mkv".to_string(),
                    content_type: ContentType::Movie,
                    compiled_regex: None,
                },
                NamingPattern {
                    description: "Collection Movie format".to_string(),
                    pattern: r"^Movies/[^/]+ Collection/[^/]+ \(\d{4}\)\.\w+$".to_string(),
                    example: "Movies/Marvel Cinematic Universe Collection/Iron Man (2008).mkv".to_string(),
                    content_type: ContentType::Movie,
                    compiled_regex: None,
                },
            ],
        }
    }
}

impl ValidateCommand {
    /// Create a new validate command
    pub fn new(media_root: PathBuf, fix_mode: bool) -> Self {
        let patterns = NamingPatterns::default();
        let compiled_patterns = Self::compile_patterns(&patterns);

        Self {
            media_root,
            patterns,
            compiled_patterns,
            fix_mode,
        }
    }

    /// Compile all regex patterns once for better performance
    fn compile_patterns(patterns: &NamingPatterns) -> Vec<CompiledPattern> {
        patterns
            .patterns
            .iter()
            .filter_map(|pattern| match Regex::new(&pattern.pattern) {
                Ok(regex) => Some(CompiledPattern { regex }),
                Err(e) => {
                    debug!(
                        "Failed to compile regex pattern '{}': {}",
                        pattern.pattern, e
                    );
                    None
                }
            })
            .collect()
    }

    /// Execute the validation command
    pub async fn execute(&self) -> Result<ValidationReport> {
        let start_time = Instant::now();

        if !self.media_root.exists() {
            return Err(anyhow!(
                "Media directory does not exist: {:?}",
                self.media_root
            ));
        }

        if !self.media_root.is_dir() {
            return Err(anyhow!("Path is not a directory: {:?}", self.media_root));
        }

        info!("🔍 Validating Plex naming scheme in: {:?}", self.media_root);
        info!("📁 Recursively scanning all subdirectories...");

        // Initialize ignore filter
        let ignore_filter = match IgnoreFilter::new(self.media_root.clone()) {
            Ok(filter) => Some(filter),
            Err(e) => {
                warn!("Failed to load .plexifyignore patterns: {}", e);
                None
            }
        };

        // Create a lookup set for media extensions for faster checks
        let media_extensions: std::collections::HashSet<&str> =
            MEDIA_EXTENSIONS.iter().copied().collect();

        // First, collect all media files with progress indicator
        let mut media_files = Vec::new();
        let mut ignored_count = 0;
        let mut files_processed = 0;

        let scan_pb = ProgressBar::new_spinner();
        scan_pb.set_style(
            ProgressStyle::with_template("{spinner:.green} {msg}")
                .unwrap()
                .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ "),
        );
        scan_pb.set_message("Collecting media files...");
        scan_pb.enable_steady_tick(std::time::Duration::from_millis(120));

        for entry in WalkDir::new(&self.media_root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let path = e.path();

                // Always allow the root directory
                if path == self.media_root {
                    return true;
                }

                // Check if we should skip this directory and all its contents
                if path.is_dir() {
                    if let Some(ref filter) = ignore_filter {
                        if filter.should_skip_dir(path) {
                            debug!("🚫 Skipping entire directory: {:?}", path);
                            return false; // This will cause WalkDir to skip the directory
                        }
                    }
                }

                true // Allow files and non-ignored directories
            })
            .filter_map(|e| e.ok())
        {
            let path = entry.path();

            // Skip directories and non-media files
            if path.is_dir() {
                continue;
            }

            // Check if this individual file should be ignored
            if let Some(ref filter) = ignore_filter {
                if filter.should_ignore(path) {
                    debug!("🚫 Ignoring path: {:?}", path);
                    ignored_count += 1;
                    continue;
                }
            }

            files_processed += 1;

            // Update progress message periodically
            if files_processed % 500 == 0 {
                scan_pb.set_message(format!("Scanned {} files...", files_processed));
            }

            // Check if it's a media file
            if let Some(extension) = path.extension() {
                let ext = extension.to_string_lossy().to_lowercase();
                if media_extensions.contains(ext.as_str()) {
                    media_files.push(path.to_path_buf());
                }
            }
        }

        scan_pb.finish_and_clear();

        info!(
            "🔍 Found {} media files, validating in parallel...",
            media_files.len()
        );

        if ignored_count > 0 {
            info!(
                "📋 Ignored {} paths due to .plexifyignore patterns",
                ignored_count
            );
        }
        // Create validation progress bar
        let validate_pb = ProgressBar::new(media_files.len() as u64);
        validate_pb.set_style(
            ProgressStyle::with_template("Validating {bar:30.cyan/blue} {pos}/{len} {msg}")
                .unwrap()
                .progress_chars("█▉▊▋▌▍▎▏ "),
        );
        validate_pb.set_message("files");

        // Create shared reference to self for parallel processing
        let media_root = Arc::new(&self.media_root);
        let pb = Arc::new(validate_pb);

        // Process files - either just validate or validate and fix
        let (issues, fixed_files) = if self.fix_mode {
            self.validate_and_fix_files(&media_files, &pb).await?
        } else {
            let issues: Vec<ValidationIssue> = media_files
                .par_iter()
                .filter_map(|path| {
                    let relative_path = match path.strip_prefix(media_root.as_ref()) {
                        Ok(rel_path) => rel_path,
                        Err(_) => return None,
                    };

                    let result = self.validate_file_path_parallel(
                        &self.compiled_patterns,
                        relative_path,
                        path,
                    );
                    pb.inc(1);
                    result
                })
                .collect();
            (issues, Vec::new())
        };

        pb.finish_and_clear();

        let validation_time = start_time.elapsed();

        let report = ValidationReport {
            scanned_files: media_files.len(),
            issues,
            fixed_files,
            patterns_used: self.patterns.clone(),
            scan_path: self.media_root.clone(),
            validation_time,
        };

        info!(
            "✅ Validation complete. Scanned {} files, found {} issues in {:.2}s",
            report.scanned_files,
            report.issues.len(),
            validation_time.as_secs_f64()
        );

        Ok(report)
    }

    /// Validate files and optionally fix them
    async fn validate_and_fix_files(
        &self,
        media_files: &[PathBuf],
        pb: &Arc<ProgressBar>,
    ) -> Result<(Vec<ValidationIssue>, Vec<FixedFile>)> {
        let mut issues = Vec::new();
        let mut fixed_files = Vec::new();

        for path in media_files {
            let relative_path = match path.strip_prefix(&self.media_root) {
                Ok(rel_path) => rel_path,
                Err(_) => {
                    pb.inc(1);
                    continue;
                }
            };

            if let Some(issue) =
                self.validate_file_path_parallel(&self.compiled_patterns, relative_path, path)
            {
                // Try to fix the issue if a suggestion is available
                if let Some(suggested_path) = &issue.suggested_path {
                    let full_suggested_path = self.media_root.join(suggested_path);

                    // Try to rename the file
                    match self.fix_file(path, &full_suggested_path).await {
                        Ok(()) => {
                            info!(
                                "✅ Fixed: {} -> {}",
                                path.display(),
                                full_suggested_path.display()
                            );
                            fixed_files.push(FixedFile {
                                original_path: path.clone(),
                                new_path: full_suggested_path,
                                issue_type: issue.issue_type,
                            });
                        }
                        Err(e) => {
                            warn!("❌ Failed to fix {}: {}", path.display(), e);
                            issues.push(issue);
                        }
                    }
                } else {
                    // No suggestion available, keep as issue
                    issues.push(issue);
                }
            }

            pb.inc(1);
        }

        Ok((issues, fixed_files))
    }

    /// Fix a single file by renaming it to the suggested path
    async fn fix_file(&self, original_path: &Path, new_path: &Path) -> Result<()> {
        // Create destination directory if it doesn't exist
        if let Some(parent) = new_path.parent() {
            if !parent.exists() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }

        // Check if destination already exists
        if new_path.exists() {
            return Err(anyhow!(
                "Destination path already exists: {}",
                new_path.display()
            ));
        }

        // Rename the file
        tokio::fs::rename(original_path, new_path).await?;

        Ok(())
    }

    /// Validate a single file path against patterns (sequential version for testing)
    fn validate_file_path(
        &self,
        relative_path: &Path,
        full_path: &Path,
    ) -> Option<ValidationIssue> {
        self.validate_file_path_parallel(&self.compiled_patterns, relative_path, full_path)
    }

    /// Validate a single file path against patterns (parallel version)
    fn validate_file_path_parallel(
        &self,
        compiled_patterns: &[CompiledPattern],
        relative_path: &Path,
        full_path: &Path,
    ) -> Option<ValidationIssue> {
        let path_str = relative_path.to_string_lossy().replace("\\", "/");

        // Try all compiled patterns (much faster than recompiling regex each time)
        for pattern in compiled_patterns.iter() {
            if pattern.regex.is_match(&path_str) {
                // Pattern matched, but do additional validation for broken show directory names
                if self.has_broken_show_directory(&path_str) {
                    // Even though it matched a pattern, the show directory name is broken
                    let issue_type = self.determine_issue_type(&path_str);
                    let description = "Show directory name contains episode pattern (corrupted structure)".to_string();
                    let suggested_path = self.suggest_path(&path_str, &issue_type);
                    return Some(ValidationIssue {
                        file_path: full_path.to_path_buf(),
                        issue_type,
                        description,
                        suggested_path,
                    });
                }
                return None; // Valid
            }
        }

        // If we reach here, the file doesn't match any pattern
        // Determine the expected content type based on directory
        let issue_type = self.determine_issue_type(&path_str);

        let description = match issue_type {
            IssueType::ShowNaming => "Show file doesn't match expected naming pattern".to_string(),
            IssueType::MovieNaming => {
                "Movie file doesn't match expected naming pattern".to_string()
            }
            IssueType::DirectoryStructure => {
                format!(
                    "File is not in a recognized directory structure ({})",
                    DIRECTORY_MAPPING
                        .iter()
                        .map(|(dir, _)| *dir)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            _ => "Unknown naming issue".to_string(),
        };

        Some(ValidationIssue {
            file_path: full_path.to_path_buf(),
            issue_type: issue_type.clone(),
            description,
            suggested_path: self.suggest_path(&path_str, &issue_type),
        })
    }

    /// Determine issue type based on directory structure
    fn determine_issue_type(&self, path_str: &str) -> IssueType {
        for (dir_name, content_type) in DIRECTORY_MAPPING {
            if path_str.starts_with(&format!("{}/", dir_name)) {
                return match content_type {
                    ContentType::Series => IssueType::ShowNaming,
                    ContentType::Movie => IssueType::MovieNaming,
                };
            }
        }
        IssueType::DirectoryStructure
    }

    /// Suggest a corrected path for a file
    fn suggest_path(&self, path_str: &str, issue_type: &IssueType) -> Option<PathBuf> {
        match issue_type {
            IssueType::DirectoryStructure => self.suggest_directory_structure_fix(path_str),
            IssueType::ShowNaming => self.suggest_show_naming_fix(path_str),
            IssueType::MovieNaming => self.suggest_movie_naming_fix(path_str),
            _ => None,
        }
    }

    /// Suggest fix for directory structure issues
    fn suggest_directory_structure_fix(&self, path_str: &str) -> Option<PathBuf> {
        if let Some(filename) = Path::new(path_str).file_name() {
            let filename_str = filename.to_string_lossy();

            // Try to extract year from filename for movie detection
            if let Some(_year_match) = Regex::new(r"\((\d{4})\)")
                .ok()
                .and_then(|re| re.find(&filename_str))
            {
                let filename_string = filename_str.to_string();
                let base_name = filename_string.replace(
                    &format!(
                        ".{}",
                        Path::new(&filename_string)
                            .extension()
                            .and_then(|ext| ext.to_str())
                            .unwrap_or("")
                    ),
                    "",
                );
                return Some(PathBuf::from(format!(
                    "Movies/{}/{}",
                    base_name, filename_str
                )));
            }

            // Check for episode patterns to suggest Series/
            if Regex::new(r"[sS]\d{1,2}[eE]\d{1,2}")
                .ok()
                .map(|re| re.is_match(&filename_str))
                .unwrap_or(false)
            {
                // Extract show name (rough heuristic)
                let show_name = filename_str
                    .split(&['-', '.', '_'])
                    .next()
                    .unwrap_or("Unknown Show")
                    .trim();
                return Some(PathBuf::from(format!(
                    "Series/{}/Season 01/{}",
                    show_name, filename_str
                )));
            }

            // Default to Movies/ for other files
            let filename_string = filename_str.to_string();
            let base_name = Path::new(&filename_string)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("Unknown Movie");
            return Some(PathBuf::from(format!(
                "Movies/{}/{}",
                base_name, filename_str
            )));
        }
        None
    }

    /// Suggest fix for show naming issues
    fn suggest_show_naming_fix(&self, path_str: &str) -> Option<PathBuf> {
        let path = Path::new(path_str);
        let filename = path.file_name()?.to_string_lossy();
        let extension = path.extension()?.to_str()?;

        // Parse the current path components
        let path_components: Vec<&str> = path_str.split('/').collect();
        if path_components.len() < 3 {
            return None;
        }

        let root_type = path_components[0]; // "Anime", "Series", etc.
        let show_dir = path_components[1]; // Show directory name
        let _season_dir = path_components[2]; // Season directory

        // Extract show name from directory (clean up any metadata)
        let show_name = self.extract_clean_show_name(show_dir);

        // Parse filename to extract episode information
        if let Some(episode_info) = self.parse_episode_filename(&filename) {
            let season_num = episode_info.season;
            let episode_num = episode_info.episode;

            // Use existing episode title if available, otherwise leave empty (don't generate generic "Episode X")
            let episode_title = episode_info.title;

            // Format episode number - use appropriate width for the episode count
            let episode_formatted = if episode_num >= 100 {
                format!("{:03}", episode_num) // 3 digits for episodes >= 100
            } else {
                format!("{:02}", episode_num) // 2 digits for episodes < 100
            };

            // Format metadata brackets if any exist
            let metadata_suffix = if episode_info.metadata.is_empty() {
                String::new()
            } else {
                format!(" {}", episode_info.metadata.iter().map(|m| format!("[{}]", m)).collect::<Vec<_>>().join(""))
            };

            // Determine the correct format based on root type and patterns (using uppercase)
            let suggested_filename = if episode_title.is_empty() {
                // No episode title available - use format without title part
                if root_type == "Anime" {
                    format!(
                        "{} - S{:02}E{}{}.{}",
                        show_name, season_num, episode_formatted, metadata_suffix, extension
                    )
                } else {
                    format!(
                        "{} - S{:02}E{}{}.{}",
                        show_name, season_num, episode_formatted, metadata_suffix, extension
                    )
                }
            } else {
                // Episode title available - use full format
                if root_type == "Anime" {
                    format!(
                        "{} - S{:02}E{} - {}{}.{}",
                        show_name, season_num, episode_formatted, episode_title, metadata_suffix, extension
                    )
                } else {
                    format!(
                        "{} - S{:02}E{} - {}{}.{}",
                        show_name, season_num, episode_formatted, episode_title, metadata_suffix, extension
                    )
                }
            };

            // Determine if this should go into Specials folder
            let is_special_content = self.is_special_content(path_str, &episode_title);
            let season_dir_clean = if is_special_content {
                "Specials".to_string()
            } else {
                format!("Season {:02}", season_num)
            };
            
            return Some(PathBuf::from(format!(
                "{}/{}/{}/{}",
                root_type, show_name, season_dir_clean, suggested_filename
            )));
        }

        None
    }

    /// Suggest fix for movie naming issues  
    fn suggest_movie_naming_fix(&self, path_str: &str) -> Option<PathBuf> {
        let path = Path::new(path_str);
        let filename = path.file_name()?.to_string_lossy();
        let extension = path.extension()?.to_str()?;

        // Parse the current path components
        let path_components: Vec<&str> = path_str.split('/').collect();
        if path_components.len() < 3 {
            return None;
        }

        let _movie_dir = path_components[1]; // Movie directory name

        // Extract year if present
        if let Some(year) = self.extract_year(&filename) {
            let clean_title = self.extract_clean_movie_title(&filename, year);
            let suggested_filename = format!("{} ({}).{}", clean_title, year, extension);
            let suggested_dir = format!("{} ({})", clean_title, year);

            return Some(PathBuf::from(format!(
                "Movies/{}/{}",
                suggested_dir, suggested_filename
            )));
        }

        None
    }

    /// Check if a path has a broken show directory name (contains episode patterns)
    fn has_broken_show_directory(&self, path_str: &str) -> bool {
        let path_components: Vec<&str> = path_str.split('/').collect();
        if path_components.len() < 3 {
            return false;
        }

        let root_type = path_components[0]; // "Anime", "Series", etc.
        if root_type != "Series" && root_type != "Anime" {
            return false;
        }

        let show_dir = path_components[1]; // Show directory name

        // Check if show directory name ends with episode pattern (indicates broken structure)
        if let Ok(re) = Regex::new(r"\s+S\d{2}E\d{2,3}$") {
            re.is_match(show_dir)
        } else {
            false
        }
    }

    /// Extract clean show name from directory name (remove metadata, year, etc.)
    fn extract_clean_show_name(&self, show_dir: &str) -> String {
        // Remove common metadata patterns
        let mut clean = show_dir.to_string();

        // Remove TVDB IDs like {tvdb-123456}
        if let Ok(re) = Regex::new(r"\s*\{tvdb-\d+\}") {
            clean = re.replace_all(&clean, "").to_string();
        }

        // Remove year in parentheses
        if let Ok(re) = Regex::new(r"\s*\(\d{4}\)") {
            clean = re.replace_all(&clean, "").to_string();
        }

        // Remove episode patterns like "S01E01" from show directory names (fixes broken nested directories)
        if let Ok(re) = Regex::new(r"\s+S\d{2}E\d{2,3}.*$") {
            clean = re.replace_all(&clean, "").to_string();
        }

        clean = clean.trim().to_string();

        if clean.is_empty() {
            show_dir.to_string()
        } else {
            clean
        }
    }

    /// Parse episode information from filename
    fn parse_episode_filename(&self, filename: &str) -> Option<EpisodeInfo> {
        // First extract metadata from the entire filename before parsing
        let (cleaned_filename, global_metadata) = self.extract_title_and_metadata(filename);
        
        // Try different episode patterns on the cleaned filename
        let patterns = vec![
            // Pattern: "Show - S11E397 - Title"
            r"^(.+?)\s*-\s*[sS](\d{1,2})[eE](\d{1,4})\s*-\s*(.*)$",
            // Pattern: "Show S11E397 Title"
            r"^(.+?)\s+[sS](\d{1,2})[eE](\d{1,4})\s*(.*)$",
            // Pattern: "Show - S11E397 Title"  
            r"^(.+?)\s*-\s*[sS](\d{1,2})[eE](\d{1,4})\s*(.*)$",
        ];

        for pattern in patterns {
            if let Ok(re) = Regex::new(pattern) {
                if let Some(captures) = re.captures(&cleaned_filename) {
                    let season: u32 = captures.get(2)?.as_str().parse().ok()?;
                    let episode: u32 = captures.get(3)?.as_str().parse().ok()?;
                    let title_part = captures.get(4).map(|m| m.as_str()).unwrap_or("");

                    // Clean up the title part and extract any additional metadata
                    let (clean_title, local_metadata) = self.extract_title_and_metadata(title_part);
                    
                    // Combine global and local metadata
                    let mut all_metadata = global_metadata;
                    all_metadata.extend(local_metadata);

                    return Some(EpisodeInfo {
                        season,
                        episode,
                        title: clean_title,
                        metadata: all_metadata,
                    });
                }
            }
        }

        None
    }

    /// Extract title and metadata from the title part of filename
    fn extract_title_and_metadata(&self, title_part: &str) -> (String, Vec<String>) {
        let mut clean = title_part.to_string();
        let mut metadata = Vec::new();

        // Remove file extension first
        if let Some(dot_pos) = clean.rfind('.') {
            clean = clean[..dot_pos].to_string();
        }

        // Extract metadata from brackets [metadata]
        if let Ok(bracket_re) = Regex::new(r"\[([^\]]+)\]") {
            for caps in bracket_re.captures_iter(&clean) {
                if let Some(meta) = caps.get(1) {
                    metadata.push(meta.as_str().to_string());
                }
            }
            // Remove all bracketed metadata from the title
            clean = bracket_re.replace_all(&clean, "").to_string();
        }

        // Extract metadata from parentheses (metadata) that looks like quality tags
        if let Ok(paren_re) = Regex::new(r"\(([^\)]+)\)") {
            for caps in paren_re.captures_iter(&clean) {
                if let Some(meta) = caps.get(1) {
                    let meta_str = meta.as_str();
                    // Only treat as metadata if it looks like quality/source tags
                    if meta_str.len() <= 10 && (meta_str.contains("p") || meta_str.contains("x264") || meta_str.contains("HDTV") || meta_str.contains("BluRay")) {
                        metadata.push(meta_str.to_string());
                    }
                }
            }
            // Remove all quality-like parentheses from the title (more comprehensive)
            if let Ok(quality_re) = Regex::new(r"\((?:\d+p\d*|x264|HDTV|BluRay|WebRip|1080p60)\)") {
                clean = quality_re.replace_all(&clean, "").to_string();
            }
        }

        // Extract other common quality patterns like .x264., .HDTV., .720p.
        let dot_patterns = vec![r"\.x264\.", r"\.HDTV\.", r"\.BluRay\.", r"\.720p\.", r"\.1080p\.", r"\.480p\."];
        for pattern in dot_patterns {
            if let Ok(re) = Regex::new(pattern) {
                for caps in re.captures_iter(&clean) {
                    let full_match = caps.get(0).unwrap().as_str();
                    let meta = full_match.trim_matches('.');
                    metadata.push(meta.to_string());
                }
                clean = re.replace_all(&clean, "").to_string();
            }
        }

        // Extract quality patterns without dots like "720p", "x264" if they appear isolated
        let isolated_quality_pattern = r"\b(720p|1080p|480p|x264|HDTV|BluRay|WebRip|WEB-DL)\b";
        if let Ok(re) = Regex::new(isolated_quality_pattern) {
            for caps in re.captures_iter(&clean) {
                if let Some(meta_match) = caps.get(1) {
                    metadata.push(meta_match.as_str().to_string());
                }
            }
            clean = re.replace_all(&clean, "").to_string();
        }

        // Clean up extra whitespace and dashes
        clean = clean.trim().trim_matches('-').trim().to_string();

        // If we have no clean title after removal, return empty (will generate generic title)
        let final_title = if clean.is_empty() || clean.len() < 3 {
            String::new()
        } else {
            clean
        };

        // Deduplicate metadata while preserving order
        let mut deduplicated_metadata = Vec::new();
        for meta in metadata {
            if !deduplicated_metadata.contains(&meta) {
                deduplicated_metadata.push(meta);
            }
        }

        (final_title, deduplicated_metadata)
    }

    /// Determine if content should go into Specials folder
    fn is_special_content(&self, path_str: &str, episode_title: &str) -> bool {
        let path_lower = path_str.to_lowercase();
        let title_lower = episode_title.to_lowercase();
        
        // Check for "behind the scenes" in path or title
        if path_lower.contains("behind the scenes") || title_lower.contains("behind the scenes") {
            return true;
        }
        
        // Check for other special content indicators
        let special_keywords = [
            "special", "bonus", "making of", "deleted scenes", "bloopers", 
            "interviews", "commentary", "extras", "featurette"
        ];
        
        for keyword in &special_keywords {
            if path_lower.contains(keyword) || title_lower.contains(keyword) {
                return true;
            }
        }
        
        false
    }

    /// Extract year from filename
    fn extract_year(&self, filename: &str) -> Option<u32> {
        if let Ok(re) = Regex::new(r"\((\d{4})\)") {
            re.captures(filename)
                .and_then(|caps| caps.get(1))
                .and_then(|m| m.as_str().parse().ok())
        } else {
            None
        }
    }

    /// Extract clean movie title
    fn extract_clean_movie_title(&self, filename: &str, year: u32) -> String {
        // Remove year and extension, clean up the rest
        let without_ext = filename.replace(
            &format!(
                ".{}",
                Path::new(filename)
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .unwrap_or("")
            ),
            "",
        );
        let without_year = without_ext.replace(&format!("({})", year), "");

        without_year.trim().to_string()
    }

    /// Print the validation report to stdout
    pub fn print_report(&self, report: &ValidationReport) {
        let report_title = if self.fix_mode {
            "📊 Plex Naming Scheme Fix Report"
        } else {
            "📊 Plex Naming Scheme Validation Report"
        };

        println!("\n{}", report_title);
        println!("═══════════════════════════════════════");
        println!("📂 Scanned directory: {}", report.scan_path.display());
        println!("📁 Files scanned: {}", report.scanned_files);

        if self.fix_mode {
            println!("✅ Files fixed: {}", report.fixed_files.len());
            println!("⚠️  Issues remaining: {}", report.issues.len());
        } else {
            println!("⚠️  Issues found: {}", report.issues.len());
        }

        println!(
            "⏱️  Processing time: {:.2}s",
            report.validation_time.as_secs_f64()
        );

        // Report fixed files if in fix mode
        if self.fix_mode && !report.fixed_files.is_empty() {
            println!("\n✅ Fixed Files:");
            println!("───────────────");

            for fixed in &report.fixed_files {
                println!("\n🔧 {}", fixed.original_path.display());
                println!("   → {}", fixed.new_path.display());
            }
        }

        if report.issues.is_empty() {
            if self.fix_mode && report.fixed_files.is_empty() {
                println!("\n✅ All files already conform to Plex naming conventions!");
            } else if !self.fix_mode {
                println!("\n✅ All files conform to Plex naming conventions!");
            }
            return;
        }

        println!("\n🔍 Issues Found:");
        println!("─────────────────");

        let mut issue_counts: HashMap<String, usize> = HashMap::new();

        for issue in &report.issues {
            let issue_type_str = match issue.issue_type {
                IssueType::ShowNaming => "Show Naming",
                IssueType::MovieNaming => "Movie Naming",
                IssueType::DirectoryStructure => "Directory Structure",
                IssueType::FileExtension => "File Extension",
                IssueType::UnknownContentType => "Unknown Content Type",
            };

            *issue_counts.entry(issue_type_str.to_string()).or_insert(0) += 1;

            println!("\n❌ {}", issue.file_path.display());
            println!("   Issue: {}", issue.description);

            if let Some(suggested) = &issue.suggested_path {
                println!("   Suggested: {}", suggested.display());
            }
        }

        println!("\n📈 Issue Summary:");
        println!("─────────────────");
        for (issue_type, count) in issue_counts {
            println!("• {}: {} files", issue_type, count);
        }

        println!("\n💡 Supported Patterns:");
        println!("─────────────────────");

        let show_patterns: Vec<_> = report
            .patterns_used
            .patterns
            .iter()
            .filter(|p| p.content_type == ContentType::Series)
            .collect();
        let movie_patterns: Vec<_> = report
            .patterns_used
            .patterns
            .iter()
            .filter(|p| p.content_type == ContentType::Movie)
            .collect();

        if !show_patterns.is_empty() {
            println!("📺 Shows:");
            for pattern in show_patterns {
                println!("   • {}", pattern.example);
            }
        }

        if !movie_patterns.is_empty() {
            println!("\n🎬 Movies:");
            for pattern in movie_patterns {
                println!("   • {}", pattern.example);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_validate_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let validate_cmd = ValidateCommand::new(temp_dir.path().to_path_buf(), false);

        let result = validate_cmd.execute().await;
        assert!(result.is_ok());

        let report = result.unwrap();
        assert_eq!(report.scanned_files, 0);
        assert_eq!(report.issues.len(), 0);
        assert!(report.validation_time > Duration::from_secs(0));
    }

    #[tokio::test]
    async fn test_validate_correct_tv_show() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create correctly named TV show (using Series instead of TV Shows)
        let tv_path = media_root.join("Series/Breaking Bad/Season 01");
        fs::create_dir_all(&tv_path).unwrap();
        fs::write(tv_path.join("Breaking Bad - S01E01 - Pilot.mkv"), "").unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf(), false);
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 1);
        assert_eq!(report.issues.len(), 0);
    }

    #[tokio::test]
    async fn test_validate_correct_anime() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create correctly named anime
        let anime_path = media_root.join("Anime/Attack on Titan/Season 01");
        fs::create_dir_all(&anime_path).unwrap();
        fs::write(
            anime_path.join("Attack on Titan - S01E01 - To You, in 2000 Years.mkv"),
            "",
        )
        .unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf(), false);
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 1);
        assert_eq!(report.issues.len(), 0);
    }

    #[tokio::test]
    async fn test_validate_correct_movie() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create correctly named movie
        let movie_path = media_root.join("Movies/The Dark Knight (2008)");
        fs::create_dir_all(&movie_path).unwrap();
        fs::write(movie_path.join("The Dark Knight (2008).mkv"), "").unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf(), false);
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 1);
        assert_eq!(report.issues.len(), 0);
    }

    #[tokio::test]
    async fn test_validate_incorrect_naming() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create incorrectly named files
        fs::create_dir_all(media_root.join("Random")).unwrap();
        fs::write(media_root.join("Random/some_movie.mkv"), "").unwrap();

        fs::create_dir_all(media_root.join("Series/Show")).unwrap();
        fs::write(media_root.join("Series/Show/episode.mkv"), "").unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf(), false);
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 2);
        assert_eq!(report.issues.len(), 2);
    }

    #[tokio::test]
    async fn test_validate_nonexistent_directory() {
        let validate_cmd = ValidateCommand::new(PathBuf::from("/nonexistent/path"), false);

        let result = validate_cmd.execute().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_validate_with_plexifyignore() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create .plexifyignore file
        fs::write(
            media_root.join(".plexifyignore"),
            "Downloads/\n*.tmp\ntools",
        )
        .unwrap();

        // Create directory structure with media files
        fs::create_dir_all(media_root.join("Downloads")).unwrap();
        fs::create_dir_all(media_root.join("tools")).unwrap();
        fs::create_dir_all(media_root.join("Movies/Good Movie (2021)")).unwrap();

        // Create media files - some should be ignored
        fs::write(media_root.join("Downloads/bad_movie.mkv"), "").unwrap();
        fs::write(media_root.join("tools/utility.mkv"), "").unwrap();
        fs::write(media_root.join("temp.tmp"), "").unwrap();
        fs::write(
            media_root.join("Movies/Good Movie (2021)/Good Movie (2021).mkv"),
            "",
        )
        .unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf(), false);
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();

        // Should only scan the non-ignored movie file
        assert_eq!(report.scanned_files, 1);
        assert_eq!(report.issues.len(), 0); // The movie is correctly named
    }

    #[tokio::test]
    async fn test_validate_with_nested_plexifyignore() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create root .plexifyignore
        fs::write(media_root.join(".plexifyignore"), "*.tmp").unwrap();

        // Create nested directory with its own .plexifyignore
        fs::create_dir_all(media_root.join("Series/old")).unwrap();
        fs::create_dir_all(media_root.join("Movies/Good Movie (2021)")).unwrap();
        fs::write(media_root.join("Series/.plexifyignore"), "old/").unwrap();

        // Create test files
        fs::write(media_root.join("test.tmp"), "").unwrap();
        fs::write(media_root.join("Series/good_show.mkv"), "").unwrap();
        fs::write(media_root.join("Series/old/old_episode.mkv"), "").unwrap();
        fs::write(
            media_root.join("Movies/Good Movie (2021)/Good Movie (2021).mkv"),
            "",
        )
        .unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf(), false);
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();

        // Should scan 2 files: Series/good_show.mkv and the movie
        // Should ignore: test.tmp (root pattern), Series/old/old_episode.mkv (nested pattern)
        assert_eq!(report.scanned_files, 2);
        assert_eq!(report.issues.len(), 1); // Only Series/good_show.mkv has incorrect naming
    }

    #[tokio::test]
    async fn test_validate_series_with_tvdb_id() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Test 1: Simple case with TVDB id that should match "Alternative Series format"
        let series_path1 = media_root.join("Series/Critical Role (2015) {tvdb-296861}/Season 01");
        fs::create_dir_all(&series_path1).unwrap();
        fs::write(
            series_path1.join("Critical Role S01E01 Arrival at Kraghammer.mp4"),
            "",
        )
        .unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf(), false);
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 1);

        // Print debug info to understand what's happening
        for issue in &report.issues {
            println!(
                "Issue: {} - {}",
                issue.file_path.display(),
                issue.description
            );
        }

        // Now this should pass - TVDB id in series name should be valid
        assert_eq!(
            report.issues.len(),
            0,
            "TVDB id in series name should be valid, but found {} issues",
            report.issues.len()
        );
    }

    #[tokio::test]
    async fn test_validate_complex_series_with_tvdb_id() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Test the exact case from the issue with extended season name and brackets
        let series_path =
            media_root.join("Series/Critical Role (2015) {tvdb-296861}/Season 01 - Vox Machina");
        fs::create_dir_all(&series_path).unwrap();
        fs::write(
            series_path.join("Critical Role - S01E01 - Arrival at Kraghammer - [1080p30].mp4"),
            "",
        )
        .unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf(), false);
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 1);

        // Print debug info
        for issue in &report.issues {
            println!(
                "Complex case issue: {} - {}",
                issue.file_path.display(),
                issue.description
            );
        }

        // This should now pass with updated patterns
        assert_eq!(
            report.issues.len(),
            0,
            "Complex TVDB case should be valid, but found {} issues",
            report.issues.len()
        );
    }

    #[tokio::test]
    async fn test_validate_anime_with_tvdb_id() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Test anime with TVDB id
        let anime_path = media_root.join("Anime/Attack on Titan {tvdb-123456}/Season 01");
        fs::create_dir_all(&anime_path).unwrap();
        fs::write(
            anime_path.join("Attack on Titan S01E01 To You in 2000 Years.mkv"),
            "",
        )
        .unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf(), false);
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 1);
        assert_eq!(report.issues.len(), 0, "Anime with TVDB id should be valid");
    }

    #[tokio::test]
    async fn test_validate_skips_ignored_directories() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create .plexifyignore file that ignores entire directories
        fs::write(
            media_root.join(".plexifyignore"),
            "Downloads/\ntools/\n*.tmp",
        )
        .unwrap();

        // Create directory structure with many files in ignored directories
        fs::create_dir_all(media_root.join("Downloads")).unwrap();
        fs::create_dir_all(media_root.join("tools")).unwrap();
        fs::create_dir_all(media_root.join("Movies/Good Movie (2021)")).unwrap();

        // Create many media files in ignored directories (simulate the performance issue)
        for i in 0..100 {
            fs::write(media_root.join(format!("Downloads/video_{}.mkv", i)), "").unwrap();
            fs::write(media_root.join(format!("tools/tool_{}.mkv", i)), "").unwrap();
        }

        // Create some files that should be processed
        fs::write(media_root.join("temp.tmp"), "").unwrap(); // Should be ignored by pattern
        fs::write(
            media_root.join("Movies/Good Movie (2021)/Good Movie (2021).mkv"),
            "",
        )
        .unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf(), false);
        let result = validate_cmd.execute().await;
        assert!(result.is_ok());
        let report = result.unwrap();

        // Should only scan 1 file (the movie), not the 200+ files in ignored directories
        assert_eq!(report.scanned_files, 1);
        assert_eq!(report.issues.len(), 0); // The movie is correctly named
    }

    #[tokio::test]
    async fn test_validate_fix_mode() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create incorrectly placed files
        fs::create_dir_all(media_root.join("WrongDir")).unwrap();
        fs::write(
            media_root.join("WrongDir/Test Movie (2021).mkv"),
            "test content",
        )
        .unwrap();
        fs::write(
            media_root.join("WrongDir/Test Series s01e01.mkv"),
            "test content",
        )
        .unwrap();

        // Test fix mode
        let validate_cmd = ValidateCommand::new(media_root.to_path_buf(), true);
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 2);
        assert_eq!(report.issues.len(), 0); // All files should be fixed
        assert_eq!(report.fixed_files.len(), 2); // Two files should be fixed

        // Verify files were moved to correct locations
        assert!(media_root
            .join("Movies/Test Movie (2021)/Test Movie (2021).mkv")
            .exists());
        assert!(media_root
            .join("Series/Test Series s01e01/Season 01/Test Series s01e01.mkv")
            .exists());

        // Original files should be gone
        assert!(!media_root.join("WrongDir/Test Movie (2021).mkv").exists());
        assert!(!media_root.join("WrongDir/Test Series s01e01.mkv").exists());
    }

    #[tokio::test]
    async fn test_validate_fix_mode_existing_destination() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create incorrectly placed file
        fs::create_dir_all(media_root.join("WrongDir")).unwrap();
        fs::write(
            media_root.join("WrongDir/Test Movie (2021).mkv"),
            "test content",
        )
        .unwrap();

        // Create the destination that already exists
        fs::create_dir_all(media_root.join("Movies/Test Movie (2021)")).unwrap();
        fs::write(
            media_root.join("Movies/Test Movie (2021)/Test Movie (2021).mkv"),
            "existing",
        )
        .unwrap();

        // Test fix mode with existing destination
        let validate_cmd = ValidateCommand::new(media_root.to_path_buf(), true);
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 2); // Should find both files
        assert_eq!(report.fixed_files.len(), 0); // No files should be fixed due to conflict
        assert_eq!(report.issues.len(), 1); // One file should remain as an issue

        // Original file should still exist since fix failed
        assert!(media_root.join("WrongDir/Test Movie (2021).mkv").exists());
        // Existing file should remain unchanged
        let existing_content = std::fs::read_to_string(
            media_root.join("Movies/Test Movie (2021)/Test Movie (2021).mkv"),
        )
        .unwrap();
        assert_eq!(existing_content, "existing");
    }

    #[tokio::test]
    async fn test_validate_fix_anime_high_episode_numbers() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create One Piece-style files with high episode numbers and quality tags that need fixing
        fs::create_dir_all(
            media_root.join("Anime/One Piece/Season 11 - Seabaody Archipelago (382-407)"),
        )
        .unwrap();
        fs::write(
            media_root.join("Anime/One Piece/Season 11 - Seabaody Archipelago (382-407)/One Piece - s11e397 - [720p][ybis].mkv"),
            "test content",
        )
        .unwrap();
        fs::write(
            media_root.join("Anime/One Piece/Season 11 - Seabaody Archipelago (382-407)/One Piece - s11e398 - [720p][ybis].mkv"),
            "test content",
        )
        .unwrap();

        // Test fix mode
        let validate_cmd = ValidateCommand::new(media_root.to_path_buf(), true);
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 2);
        assert_eq!(report.issues.len(), 0); // All files should be fixed
        assert_eq!(report.fixed_files.len(), 2); // Two files should be fixed

        // Verify files were moved to correct locations with proper names (without generic "Episode X")
        assert!(media_root
            .join("Anime/One Piece/Season 11/One Piece - S11E397 [720p][ybis].mkv")
            .exists());
        assert!(media_root
            .join("Anime/One Piece/Season 11/One Piece - S11E398 [720p][ybis].mkv")
            .exists());

        // Original files should be gone
        assert!(!media_root.join("Anime/One Piece/Season 11 - Seabaody Archipelago (382-407)/One Piece - S11E397 - [720p][ybis].mkv").exists());
        assert!(!media_root.join("Anime/One Piece/Season 11 - Seabaody Archipelago (382-407)/One Piece - S11E398 - [720p][ybis].mkv").exists());

        // Verify the fixed files now validate correctly
        let validate_cmd_check = ValidateCommand::new(media_root.to_path_buf(), false);
        let result_check = validate_cmd_check.execute().await;
        assert!(result_check.is_ok());
        let report_check = result_check.unwrap();
        assert_eq!(report_check.issues.len(), 0); // No issues should remain
    }
}
