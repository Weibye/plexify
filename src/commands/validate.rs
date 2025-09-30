use anyhow::{anyhow, Result};
use rayon::prelude::*;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};
use walkdir::WalkDir;

/// Media file extensions that should be validated
const MEDIA_EXTENSIONS: &[&str] = &["mkv", "mp4", "avi", "webm", "mov", "m4v"];

/// Content type for categorizing naming patterns
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContentType {
    Show,
    Movie,
}

/// Directory mapping configuration
const DIRECTORY_MAPPING: &[(&str, ContentType)] = &[
    ("Anime", ContentType::Show),
    ("Series", ContentType::Show),
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
    pub patterns_used: NamingPatterns,
    pub scan_path: PathBuf,
    pub validation_time: Duration,
}

/// Command to validate Plex naming scheme conformity
pub struct ValidateCommand {
    media_root: PathBuf,
    patterns: NamingPatterns,
    compiled_patterns: Vec<CompiledPattern>,
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
                    pattern: r"^Anime/[^/]+/Season \d{2}/[^/]+ - s\d{2}e\d{2} - [^/]+\.\w+$".to_string(),
                    example: "Anime/Attack on Titan/Season 01/Attack on Titan - s01e01 - To You, in 2000 Years.mkv".to_string(),
                    content_type: ContentType::Show,
                    compiled_regex: None,
                },
                NamingPattern {
                    description: "Alternative Anime format".to_string(),
                    pattern: r"^Anime/[^/]+/Season \d{2}/[^/]+ S\d{2}E\d{2} [^/]+\.\w+$".to_string(),
                    example: "Anime/Attack on Titan/Season 01/Attack on Titan S01E01 To You, in 2000 Years.mkv".to_string(),
                    content_type: ContentType::Show,
                    compiled_regex: None,
                },
                // Series patterns (shows)  
                NamingPattern {
                    description: "Standard Series format".to_string(),
                    pattern: r"^Series/[^/]+/Season \d{2}/[^/]+ - s\d{2}e\d{2} - [^/]+\.\w+$".to_string(),
                    example: "Series/Breaking Bad/Season 01/Breaking Bad - s01e01 - Pilot.mkv".to_string(),
                    content_type: ContentType::Show,
                    compiled_regex: None,
                },
                NamingPattern {
                    description: "Alternative Series format".to_string(),
                    pattern: r"^Series/[^/]+/Season \d{2}/[^/]+ S\d{2}E\d{2} [^/]+\.\w+$".to_string(),
                    example: "Series/Breaking Bad/Season 01/Breaking Bad S01E01 Pilot.mkv".to_string(),
                    content_type: ContentType::Show,
                    compiled_regex: None,
                },
                NamingPattern {
                    description: "Simple Series format".to_string(),
                    pattern: r"^Series/[^/]+/Season \d{2}/S\d{2}E\d{2} - [^/]+\.\w+$".to_string(),
                    example: "Series/Breaking Bad/Season 01/S01E01 - Pilot.mkv".to_string(),
                    content_type: ContentType::Show,
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
    pub fn new(media_root: PathBuf) -> Self {
        let patterns = NamingPatterns::default();
        let compiled_patterns = Self::compile_patterns(&patterns);

        Self {
            media_root,
            patterns,
            compiled_patterns,
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

        // Create a lookup set for media extensions for faster checks
        let media_extensions: std::collections::HashSet<&str> =
            MEDIA_EXTENSIONS.iter().copied().collect();

        // First, collect all media files
        let mut media_files = Vec::new();
        for entry in WalkDir::new(&self.media_root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();

            // Skip directories and non-media files
            if path.is_dir() {
                continue;
            }

            // Check if it's a media file
            if let Some(extension) = path.extension() {
                let ext = extension.to_string_lossy().to_lowercase();
                if media_extensions.contains(ext.as_str()) {
                    media_files.push(path.to_path_buf());
                }
            }
        }

        info!(
            "🔍 Found {} media files, validating in parallel...",
            media_files.len()
        );

        // Create shared reference to self for parallel processing
        let media_root = Arc::new(&self.media_root);

        // Process files in parallel using rayon
        let issues: Vec<ValidationIssue> = media_files
            .par_iter()
            .filter_map(|path| {
                let relative_path = match path.strip_prefix(media_root.as_ref()) {
                    Ok(rel_path) => rel_path,
                    Err(_) => return None,
                };

                self.validate_file_path_parallel(&self.compiled_patterns, relative_path, path)
            })
            .collect();

        let validation_time = start_time.elapsed();

        let report = ValidationReport {
            scanned_files: media_files.len(),
            issues,
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
            suggested_path: self.suggest_path(&path_str, &issue_type),
        })
    }

    /// Determine issue type based on directory structure
    fn determine_issue_type(&self, path_str: &str) -> IssueType {
        for (dir_name, content_type) in DIRECTORY_MAPPING {
            if path_str.starts_with(&format!("{}/", dir_name)) {
                return match content_type {
                    ContentType::Show => IssueType::ShowNaming,
                    ContentType::Movie => IssueType::MovieNaming,
                };
            }
        }
        IssueType::DirectoryStructure
    }

    /// Suggest a corrected path for a file
    fn suggest_path(&self, path_str: &str, issue_type: &IssueType) -> Option<PathBuf> {
        // This is a simplified suggestion system
        // In a full implementation, this would be more sophisticated
        if let IssueType::DirectoryStructure = issue_type {
            // If it's not in Movies/ or TV Shows/, suggest moving to Movies/
            if let Some(filename) = Path::new(path_str).file_name() {
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
            }
        }
        None
    }

    /// Print the validation report to stdout
    pub fn print_report(&self, report: &ValidationReport) {
        println!("\n📊 Plex Naming Scheme Validation Report");
        println!("═══════════════════════════════════════");
        println!("📂 Scanned directory: {}", report.scan_path.display());
        println!("📁 Files scanned: {}", report.scanned_files);
        println!("⚠️  Issues found: {}", report.issues.len());
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
            .filter(|p| p.content_type == ContentType::Show)
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
        let validate_cmd = ValidateCommand::new(temp_dir.path().to_path_buf());

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

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf());
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

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf());
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

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf());
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

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf());
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 2);
        assert_eq!(report.issues.len(), 2);
    }

    #[tokio::test]
    async fn test_validate_nonexistent_directory() {
        let validate_cmd = ValidateCommand::new(PathBuf::from("/nonexistent/path"));

        let result = validate_cmd.execute().await;
        assert!(result.is_err());
    }
}
