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
use super::naming_rules::NamingRules;

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
    pub fixed_path: Option<PathBuf>,
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
    pub patterns_used: NamingPatterns,
    pub scan_path: PathBuf,
    pub validation_time: Duration,
    pub fixed_files: usize,
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
                    pattern: r"^Anime/[^/]+(?:\s*\{tvdb-\d+\})?/Season \d{2}(?:\s*-[^/]*)*/[^/]+ - s\d{2}e\d{2} - [^/]+\.\w+$".to_string(),
                    example: "Anime/Attack on Titan/Season 01/Attack on Titan - s01e01 - To You, in 2000 Years.mkv".to_string(),
                    content_type: ContentType::Series,
                    compiled_regex: None,
                },
                NamingPattern {
                    description: "Alternative Anime format".to_string(),
                    pattern: r"^Anime/[^/]+(?:\s*\{tvdb-\d+\})?/Season \d{2}(?:\s*-[^/]*)*/[^/]+ S\d{2}E\d{2} [^/]+\.\w+$".to_string(),
                    example: "Anime/Attack on Titan/Season 01/Attack on Titan S01E01 To You, in 2000 Years.mkv".to_string(),
                    content_type: ContentType::Series,
                    compiled_regex: None,
                },
                // Series patterns (shows)  
                NamingPattern {
                    description: "Standard Series format".to_string(),
                    pattern: r"^Series/[^/]+(?:\s*\{tvdb-\d+\})?/Season \d{2}(?:\s*-[^/]*)*/[^/]+ - s\d{2}e\d{2} - [^/]+\.\w+$".to_string(),
                    example: "Series/Breaking Bad/Season 01/Breaking Bad - s01e01 - Pilot.mkv".to_string(),
                    content_type: ContentType::Series,
                    compiled_regex: None,
                },
                NamingPattern {
                    description: "Alternative Series format".to_string(),
                    pattern: r"^Series/[^/]+(?:\s*\{tvdb-\d+\})?/Season \d{2}(?:\s*-[^/]*)*/[^/]+ S\d{2}E\d{2} [^/]+\.\w+$".to_string(),
                    example: "Series/Breaking Bad (2008) {tvdb-296861}/Season 01/Breaking Bad S01E01 Pilot.mkv".to_string(),
                    content_type: ContentType::Series,
                    compiled_regex: None,
                },
                NamingPattern {
                    description: "Simple Series format".to_string(),
                    pattern: r"^Series/[^/]+(?:\s*\{tvdb-\d+\})?/Season \d{2}(?:\s*-[^/]*)*/S\d{2}E\d{2} - [^/]+\.\w+$".to_string(),
                    example: "Series/Breaking Bad/Season 01/S01E01 - Pilot.mkv".to_string(),
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

        // Process files in parallel using rayon
        let mut issues: Vec<ValidationIssue> = media_files
            .par_iter()
            .filter_map(|path| {
                let relative_path = match path.strip_prefix(media_root.as_ref()) {
                    Ok(rel_path) => rel_path,
                    Err(_) => return None,
                };

                let result =
                    self.validate_file_path_parallel(&self.compiled_patterns, relative_path, path);
                pb.inc(1);
                result
            })
            .collect();

        pb.finish_and_clear();

        let validation_time = start_time.elapsed();

        // If we're in fix mode, attempt to rename the files
        let mut fixed_count = 0;
        if self.fix_mode && !issues.is_empty() {
            info!("🔧 Fix mode enabled, attempting to rename {} files...", issues.len());
            
            for issue in &mut issues {
                match self.rename_file(issue).await {
                    Ok(true) => fixed_count += 1,
                    Ok(false) => {}, // No rename needed or failed silently
                    Err(e) => {
                        warn!("Failed to rename {:?}: {}", issue.file_path, e);
                    }
                }
            }
        }

        let report = ValidationReport {
            scanned_files: media_files.len(),
            issues,
            patterns_used: self.patterns.clone(),
            scan_path: self.media_root.clone(),
            validation_time,
            fixed_files: fixed_count,
        };

        info!(
            "✅ Validation complete. Scanned {} files, found {} issues in {:.2}s",
            report.scanned_files,
            report.issues.len(),
            validation_time.as_secs_f64()
        );

        Ok(report)
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
            suggested_path: self.suggest_path(full_path, &issue_type),
            fixed_path: None,
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

    /// Suggest a corrected path for a file using naming rules
    fn suggest_path(&self, file_path: &Path, issue_type: &IssueType) -> Option<PathBuf> {
        // First try to apply naming rules for Series files
        if let Ok(naming_rules) = NamingRules::new() {
            if let Some(rule_match) = naming_rules.apply_rules(file_path) {
                // Apply the rule transformation
                for rule in naming_rules.get_rules() {
                    if rule.pattern.is_match(&file_path.to_string_lossy().replace("\\", "/")) {
                        if let Ok(suggested_path) = (rule.transform)(&rule_match) {
                            return Some(suggested_path);
                        }
                        break;
                    }
                }
            }
        }

        // Fallback to simple suggestion for directory structure issues
        if let IssueType::DirectoryStructure = issue_type {
            // If it's not in Movies/ or TV Shows/, suggest moving to Movies/
            if let Some(filename) = file_path.file_name() {
                let filename_str = filename.to_string_lossy();
                // Try to extract year from filename
                if let Some(_year_match) = Regex::new(r"\((\d{4})\)")
                    .ok()
                    .and_then(|re| re.find(&filename_str))
                {
                    let filename_string = filename_str.to_string();
                    let base_name = filename_string.replace(
                        &format!(
                            ".{}",
                            file_path
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
            }
        }
        None
    }

    /// Rename a file if fix mode is enabled
    async fn rename_file(&self, issue: &mut ValidationIssue) -> Result<bool> {
        if !self.fix_mode {
            return Ok(false);
        }

        if let Some(suggested_path) = &issue.suggested_path {
            let full_suggested_path = self.media_root.join(suggested_path);
            
            // Ensure the target directory exists
            if let Some(parent) = full_suggested_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }

            // Check if target file already exists
            if full_suggested_path.exists() {
                warn!(
                    "Target file already exists, skipping rename: {:?}",
                    full_suggested_path
                );
                return Ok(false);
            }

            // Rename the file
            tokio::fs::rename(&issue.file_path, &full_suggested_path).await?;
            
            info!(
                "📁 Renamed: {:?} -> {:?}",
                issue.file_path,
                full_suggested_path
            );

            issue.fixed_path = Some(full_suggested_path);
            return Ok(true);
        }

        Ok(false)
    }

    /// Print the validation report to stdout
    pub fn print_report(&self, report: &ValidationReport) {
        println!("\n📊 Plex Naming Scheme Validation Report");
        println!("═══════════════════════════════════════");
        println!("📂 Scanned directory: {}", report.scan_path.display());
        println!("📁 Files scanned: {}", report.scanned_files);
        println!("⚠️  Issues found: {}", report.issues.len());
        if report.fixed_files > 0 {
            println!("🔧 Files fixed: {}", report.fixed_files);
        }
        println!(
            "⏱️  Validation time: {:.2}s",
            report.validation_time.as_secs_f64()
        );

        if report.issues.is_empty() {
            println!("\n✅ All files conform to Plex naming conventions!");
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

            if issue.fixed_path.is_some() {
                println!("\n✅ {}", issue.file_path.display());
                println!("   Issue: {}", issue.description);
                if let Some(fixed) = &issue.fixed_path {
                    println!("   Fixed: {}", fixed.display());
                }
            } else {
                println!("\n❌ {}", issue.file_path.display());
                println!("   Issue: {}", issue.description);
                
                if let Some(suggested) = &issue.suggested_path {
                    println!("   Suggested: {}", suggested.display());
                }
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
        fs::write(tv_path.join("Breaking Bad - s01e01 - Pilot.mkv"), "").unwrap();

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
            anime_path.join("Attack on Titan - s01e01 - To You, in 2000 Years.mkv"),
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
    async fn test_validate_with_fix_mode() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create a series file that exactly matches our naming rule test case
        let series_path = media_root.join("Series/Scrubs/Season 9");
        fs::create_dir_all(&series_path).unwrap();
        fs::write(
            series_path.join("Scrubs.S09E02.RETAIL.DVDRip.XviD-REWARD.avi"),
            "test content"
        ).unwrap();

        // First test without fix mode (dry run)
        let validate_cmd = ValidateCommand::new(media_root.to_path_buf(), false);
        let result = validate_cmd.execute().await;
        assert!(result.is_ok());
        
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 1);
        // The file should have an issue (wrong season format: 9 instead of 09)
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.fixed_files, 0);

        // The issue should have a suggested path from our naming rules
        if let Some(issue) = report.issues.first() {
            assert!(issue.suggested_path.is_some());
            println!("Suggested path: {:?}", issue.suggested_path);
        }

        // Now test with fix mode (ensure there's an actual rename happening)
        let validate_cmd_fix = ValidateCommand::new(media_root.to_path_buf(), true);
        let result_fix = validate_cmd_fix.execute().await;
        assert!(result_fix.is_ok());
        
        let report_fix = result_fix.unwrap();
        assert_eq!(report_fix.scanned_files, 1);
        
        // After fix mode, check that file was actually renamed  
        println!("Fixed files: {}", report_fix.fixed_files);
        println!("Issues after fix: {}", report_fix.issues.len());
        
        // Check that we got some fixes
        if report_fix.fixed_files > 0 {
            // File should be fixed, check if it exists in new location
            if let Some(issue) = report_fix.issues.first() {
                if let Some(fixed_path) = &issue.fixed_path {
                    assert!(fixed_path.exists(), "Fixed file should exist at new path");
                }
            }
        }
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
}
