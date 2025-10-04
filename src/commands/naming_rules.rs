use anyhow::Result;
use regex::Regex;
use std::path::{Path, PathBuf};

/// A rule for renaming media files to conform to Plex standards
#[derive(Debug, Clone)]
pub struct NamingRule {
    pub name: String,
    pub description: String,
    /// Regex pattern to match the current file structure
    pub pattern: Regex,
    /// Function to apply the transformation
    pub transform: fn(&NamingRuleMatch) -> Result<PathBuf>,
}

/// Captured information from a naming rule match
#[derive(Debug, Clone)]
pub struct NamingRuleMatch {
    pub original_path: PathBuf,
    pub captures: Vec<String>,
    pub extension: String,
}

impl NamingRuleMatch {
    /// Extract season number from captures, ensuring it has at least 2 digits
    pub fn format_season(&self, season_index: usize) -> Result<String> {
        if season_index >= self.captures.len() {
            return Err(anyhow::anyhow!(
                "Season index {} out of bounds",
                season_index
            ));
        }

        let season_str = &self.captures[season_index];
        let season_num: u32 = season_str
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid season number: {}", season_str))?;

        Ok(format!("{:02}", season_num))
    }

    /// Extract episode identifier from captures, ensuring SXXEXX format is uppercase
    pub fn format_episode(&self, episode_index: usize) -> Result<String> {
        if episode_index >= self.captures.len() {
            return Err(anyhow::anyhow!(
                "Episode index {} out of bounds",
                episode_index
            ));
        }

        let episode_str = &self.captures[episode_index].to_uppercase();

        // Ensure SXXEXX pattern
        if let Some(caps) = Regex::new(r"S(\d+)E(\d+)")?.captures(episode_str) {
            let season: u32 = caps[1]
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid season in episode: {}", episode_str))?;
            let episode: u32 = caps[2]
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid episode in episode: {}", episode_str))?;
            Ok(format!("S{:02}E{:02}", season, episode))
        } else {
            Err(anyhow::anyhow!("Invalid episode format: {}", episode_str))
        }
    }

    /// Extract and clean episode title, removing common metadata patterns
    pub fn clean_episode_title(&self, title_index: usize) -> Result<String> {
        if title_index >= self.captures.len() {
            return Err(anyhow::anyhow!("Title index {} out of bounds", title_index));
        }

        let mut title = self.captures[title_index].clone();

        // Remove common metadata patterns and clean up
        title = title
            .replace(".", " ") // Replace dots with spaces
            .replace("_", " ") // Replace underscores with spaces
            .trim()
            .to_string();

        // Remove quality/release metadata but preserve resolution info (1080p|720p|480p)
        let clean_regex = Regex::new(
            r"(?i)\b(retail|dvdrip|webrip|bluray|hdtv|xvid|x264|x265|aac|ac3|dts|mkv|avi|mp4|webm|[\w\-]+rip)\b",
        )?;
        title = clean_regex.replace_all(&title, "").to_string();

        // Clean up multiple spaces and trim
        let space_regex = Regex::new(r"\s+")?;
        title = space_regex.replace_all(&title, " ").trim().to_string();

        // Capitalize words properly (basic title case)
        title = title
            .split_whitespace()
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    None => String::new(),
                    Some(first) => {
                        first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(" ");

        Ok(title)
    }

    /// Extract size/quality metadata from filename and format it properly
    pub fn extract_quality_metadata(&self, full_filename: &str) -> String {
        // Extract resolution and quality info
        let quality_regex = Regex::new(r"(?i)(\d+p\d*|1080p|720p|480p|4k|uhd|hd|sd)").unwrap();
        let mut metadata_parts = Vec::new();

        if let Some(caps) = quality_regex.find(full_filename) {
            metadata_parts.push(caps.as_str().to_lowercase());
        }

        // If we found metadata, format it properly
        if !metadata_parts.is_empty() {
            format!(" [{}]", metadata_parts.join(" "))
        } else {
            String::new()
        }
    }
}

/// Collection of naming rules for different file patterns
pub struct NamingRules {
    rules: Vec<NamingRule>,
}

impl NamingRules {
    pub fn new() -> Result<Self> {
        let mut rules = Vec::new();

        // ATOMIC RULE 1: Season directory missing leading zero (Season 6 -> Season 06)
        // Root-independent: works with Series/, Anime/, etc.
        rules.push(NamingRule {
            name: "season_directory_zero_padding".to_string(),
            description: "Add leading zero to season directory".to_string(),
            pattern: Regex::new(r"^([^/]+)/([^/]+)/Season (\d{1})/(.+)\.(\w+)$")?,
            transform: |m| {
                let root = &m.captures[0]; // Series, Anime, etc.
                let series_name = &m.captures[1];
                let season_num = &m.captures[2];
                let filename = &m.captures[3];
                let ext = &m.captures[4];

                let season_formatted = format!("{:02}", season_num.parse::<u32>().unwrap_or(0));

                Ok(PathBuf::from(format!(
                    "{}/{}/Season {}/{}.{}",
                    root, series_name, season_formatted, filename, ext
                )))
            },
        });

        // ATOMIC RULE 2: Quality metadata in parentheses -> brackets
        // Only changes (quality) to [quality]
        rules.push(NamingRule {
            name: "parentheses_to_brackets".to_string(),
            description: "Convert quality metadata from parentheses to brackets".to_string(),
            pattern: Regex::new(r"^(.+) \(([^)]*(?:1080p|720p|480p)[^)]*)\)\.(\w+)$")?,
            transform: |m| {
                let path_base = &m.captures[0];
                let quality = &m.captures[1];
                let ext = &m.captures[2];

                Ok(PathBuf::from(format!(
                    "{} [{}].{}",
                    path_base, quality, ext
                )))
            },
        });

        // ATOMIC RULE 3: Missing dash between SXXEXX and episode title
        // Example: Show - S06E08 Title -> Show - S06E08 - Title
        // But not if it already has quality metadata in brackets
        rules.push(NamingRule {
            name: "missing_dash_after_episode".to_string(),
            description: "Add missing dash between SXXEXX and episode title".to_string(),
            pattern: Regex::new(r"^(.+) - (S\d{2}E\d{2}) ([^/\-\[][^/\[\]]+)\.(\w+)$")?,
            transform: |m| {
                let path_base = &m.captures[0];
                let episode = &m.captures[1];
                let episode_title = &m.captures[2];
                let ext = &m.captures[3];

                Ok(PathBuf::from(format!(
                    "{} - {} - {}.{}",
                    path_base, episode, episode_title, ext
                )))
            },
        });

        // ATOMIC RULE 4: Episode and title order (Some Series - Episode Name - SXXEXX)
        // Reorders to proper format: Some Series - SXXEXX - Episode Name
        rules.push(NamingRule {
            name: "reorder_episode_title".to_string(),
            description: "Reorder episode code and title to proper format".to_string(),
            pattern: Regex::new(r"^(.+) - ([^-]+) - (S\d{2}E\d{2})\.(\w+)$")?,
            transform: |m| {
                let path_base = &m.captures[0];
                let episode_title = &m.captures[1].trim();
                let episode = &m.captures[2];
                let ext = &m.captures[3];

                Ok(PathBuf::from(format!(
                    "{} - {} - {}.{}",
                    path_base, episode, episode_title, ext
                )))
            },
        });

        // ATOMIC RULE 5: Replace dots with dashes in filenames
        // Example: Show.Name.S01E01.Title -> Show.Name - S01E01.Title (preserves series dots but converts structure)
        rules.push(NamingRule {
            name: "dots_to_dashes_structure".to_string(),
            description: "Convert dotted structure to dash-separated structure".to_string(),
            pattern: Regex::new(
                r"^([^/]+)/([^/]+)/Season (\d{1,2})/([^.]+)\.(S\d{2}E\d{2})\.(.+)\.(\w+)$",
            )?,
            transform: |m| {
                let root = &m.captures[0];
                let series_name = &m.captures[1];
                let season = &m.captures[2];
                let show_name = &m.captures[3];
                let episode = &m.captures[4];
                let title_part = &m.captures[5];
                let ext = &m.captures[6];

                Ok(PathBuf::from(format!(
                    "{}/{}/Season {}/{} - {} - {}.{}",
                    root, series_name, season, show_name, episode, title_part, ext
                )))
            },
        });

        // ATOMIC RULE 6: Remove Roman numerals from episode titles
        rules.push(NamingRule {
            name: "remove_roman_numerals".to_string(),
            description: "Remove Roman numerals from episode titles".to_string(),
            pattern: Regex::new(r"^(.+ - S\d{2}E\d{2} - )([IVXLCDM]+) (.+)\.(\w+)$")?,
            transform: |m| {
                let path_base = &m.captures[0];
                let title_part = &m.captures[2].trim();
                let ext = &m.captures[3];

                Ok(PathBuf::from(format!(
                    "{}{}.{}",
                    path_base, title_part, ext
                )))
            },
        });

        // ATOMIC RULE 7: Clean metadata from episode titles
        rules.push(NamingRule {
            name: "clean_episode_metadata".to_string(),
            description: "Remove release metadata from episode titles".to_string(),
            pattern: Regex::new(r"^(.+ - S\d{2}E\d{2} - )(.+?)(?:\.(retail|dvdrip|webrip|bluray|hdtv|xvid|x264|x265|aac|ac3|dts|[\w\-]+rip).*)\.(\w+)$")?,
            transform: |m| {
                let path_base = &m.captures[0];
                let clean_title = &m.captures[1];
                let ext = &m.captures[3];

                // Clean up dots and underscores, apply title case
                let cleaned = clean_title
                    .replace(".", " ")
                    .replace("_", " ")
                    .split_whitespace()
                    .map(|word| {
                        let mut chars = word.chars();
                        match chars.next() {
                            None => String::new(),
                            Some(first) => {
                                first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                            }
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ");

                Ok(PathBuf::from(format!(
                    "{}{}.{}",
                    path_base, cleaned, ext
                )))
            },
        });

        // ATOMIC RULE 8: Remove extra dash before quality metadata when no episode title
        // Example: Show - S01E01 - [720p].mkv -> Show - S01E01 [720p].mkv  
        rules.push(NamingRule {
            name: "remove_dash_before_quality".to_string(),
            description: "Remove extra dash before quality metadata when no episode title".to_string(),
            pattern: Regex::new(r"^(.+ - S\d{2}E\d{2}) - (\[[^\]]+\])\.(\w+)$")?,
            transform: |m| {
                let path_base = &m.captures[0];
                let quality = &m.captures[1];
                let ext = &m.captures[2];

                Ok(PathBuf::from(format!(
                    "{} {}.{}",
                    path_base, quality, ext
                )))
            },
        });

        // ATOMIC RULE 9: Fix duplicated directory structure for Series
        // Example: Series/Veronica Mars/Series/Veronica Mars/Season 01/file -> Series/Veronica Mars/Season 01/file
        rules.push(NamingRule {
            name: "fix_duplicated_series_structure".to_string(),
            description: "Remove duplicated Series/Show structure".to_string(),
            pattern: Regex::new(r"^Series/([^/]+)/Series/[^/]+/(Season .+)$")?,
            transform: |m| {
                let show_name = &m.captures[0];
                let season_and_file = &m.captures[1];

                Ok(PathBuf::from(format!("Series/{}/{}", show_name, season_and_file)))
            },
        });

        // ATOMIC RULE 10: Uppercase SXXEXX patterns
        rules.push(NamingRule {
            name: "uppercase_episode_codes".to_string(),
            description: "Ensure SXXEXX patterns are uppercase".to_string(),
            pattern: Regex::new(r"^(.+ - )(s)(\d{2})(e)(\d{2})( - .+)\.(\w+)$")?,
            transform: |m| {
                let path_start = &m.captures[0];
                let season = &m.captures[2];
                let episode = &m.captures[4];
                let path_end = &m.captures[5];
                let ext = &m.captures[6];

                Ok(PathBuf::from(format!(
                    "{}S{}E{}{}.{}",
                    path_start, season, episode, path_end, ext
                )))
            },
        });

        Ok(Self { rules })
    }

    /// Apply atomic naming rules in sequence to transform a file path
    pub fn apply_rules(&self, file_path: &Path) -> Option<PathBuf> {
        let mut current_path = file_path.to_path_buf();
        let mut was_transformed = false;

        // Apply each rule that matches, allowing multiple transformations
        for rule in &self.rules {
            let path_str = current_path.to_string_lossy().replace("\\", "/");

            if let Some(caps) = rule.pattern.captures(&path_str) {
                let captures: Vec<String> = caps
                    .iter()
                    .skip(1) // Skip the full match
                    .map(|m| m.map_or(String::new(), |m| m.as_str().to_string()))
                    .collect();

                let extension = current_path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| format!(".{}", ext))
                    .unwrap_or_default();

                let rule_match = NamingRuleMatch {
                    original_path: current_path.clone(),
                    captures,
                    extension,
                };

                if let Ok(transformed_path) = (rule.transform)(&rule_match) {
                    current_path = transformed_path;
                    was_transformed = true;
                }
            }
        }

        if was_transformed {
            Some(current_path)
        } else {
            None
        }
    }

    /// Get the first matching rule for a file path (used for testing specific rules)
    pub fn get_first_match(&self, file_path: &Path) -> Option<NamingRuleMatch> {
        let path_str = file_path.to_string_lossy().replace("\\", "/");

        for rule in &self.rules {
            if let Some(caps) = rule.pattern.captures(&path_str) {
                let captures: Vec<String> = caps
                    .iter()
                    .skip(1) // Skip the full match
                    .map(|m| m.map_or(String::new(), |m| m.as_str().to_string()))
                    .collect();

                let extension = file_path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .map(|ext| format!(".{}", ext))
                    .unwrap_or_default();

                return Some(NamingRuleMatch {
                    original_path: file_path.to_path_buf(),
                    captures,
                    extension,
                });
            }
        }

        None
    }

    /// Get all available rules for documentation/debugging
    pub fn get_rules(&self) -> &[NamingRule] {
        &self.rules
    }
}

impl NamingRuleMatch {
    /// Helper method for cleaning episode titles from raw strings
    fn clean_episode_title_from_str(&self, title: &str) -> Result<String> {
        let mut cleaned = title.to_string();

        // Remove common metadata patterns and clean up
        cleaned = cleaned
            .replace(".", " ") // Replace dots with spaces
            .replace("_", " ") // Replace underscores with spaces
            .trim()
            .to_string();

        // Remove quality/release metadata (like "RETAIL.DVDRip.XviD-REWARD")
        let clean_regex = Regex::new(
            r"(?i)\b(retail|dvdrip|webrip|bluray|hdtv|xvid|x264|x265|aac|ac3|dts|1080p|720p|480p|mkv|avi|mp4|webm|[\w\-]+rip|[\w\-]+\d+p?)\b",
        )?;
        cleaned = clean_regex.replace_all(&cleaned, "").to_string();

        // Clean up multiple spaces and trim
        let space_regex = Regex::new(r"\s+")?;
        cleaned = space_regex.replace_all(&cleaned, " ").trim().to_string();

        // Capitalize words properly (basic title case)
        cleaned = cleaned
            .split_whitespace()
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    None => String::new(),
                    Some(first) => {
                        first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(" ");

        Ok(cleaned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_naming_rules_creation() {
        let rules = NamingRules::new().unwrap();
        assert!(!rules.get_rules().is_empty());
    }

    #[test]
    fn test_format_season() {
        let rule_match = NamingRuleMatch {
            original_path: PathBuf::from("test"),
            captures: vec!["6".to_string()],
            extension: ".mkv".to_string(),
        };

        assert_eq!(rule_match.format_season(0).unwrap(), "06");
    }

    #[test]
    fn test_format_episode() {
        let rule_match = NamingRuleMatch {
            original_path: PathBuf::from("test"),
            captures: vec!["s06e08".to_string()],
            extension: ".mkv".to_string(),
        };

        assert_eq!(rule_match.format_episode(0).unwrap(), "S06E08");
    }

    #[test]
    fn test_clean_episode_title() {
        let rule_match = NamingRuleMatch {
            original_path: PathBuf::from("test"),
            captures: vec!["sand.trap.retail.dvdrip".to_string()],
            extension: ".mkv".to_string(),
        };

        let cleaned = rule_match.clean_episode_title(0).unwrap();
        assert_eq!(cleaned, "Sand Trap");
    }

    #[test]
    fn test_extract_quality_metadata() {
        let rule_match = NamingRuleMatch {
            original_path: PathBuf::from("test"),
            captures: vec![],
            extension: ".mkv".to_string(),
        };

        let quality = rule_match.extract_quality_metadata("episode.1080p.h264");
        assert_eq!(quality, " [1080p]");

        let no_quality = rule_match.extract_quality_metadata("episode.normal.title");
        assert_eq!(no_quality, "");
    }

    #[test]
    fn test_atomic_rules_season_padding_and_dash() {
        let rules = NamingRules::new().unwrap();
        let test_path = Path::new("Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv");

        if let Some(result) = rules.apply_rules(test_path) {
            // Should apply both season padding AND missing dash rules in sequence
            assert_eq!(
                result.to_string_lossy(),
                "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv"
            );
        } else {
            panic!("Atomic rules should have transformed the path");
        }
    }

    #[test]
    fn test_atomic_rule_missing_dash_only() {
        let rules = NamingRules::new().unwrap();
        let test_path = Path::new("Series/Elementary/Season 06/Elementary - S06E08 Sand Trap.mkv");

        if let Some(result) = rules.apply_rules(test_path) {
            assert_eq!(
                result.to_string_lossy(),
                "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv"
            );
        } else {
            panic!("Atomic rules should have transformed the path");
        }
    }

    #[test]
    fn test_atomic_rules_multiple_transformations() {
        let rules = NamingRules::new().unwrap();
        let test_path =
            Path::new("Series/Scrubs/Season 9/Scrubs.S09E02.RETAIL.DVDRip.XviD-REWARD.avi");

        // The atomic rules should apply multiple transformations in sequence
        if let Some(result) = rules.apply_rules(test_path) {
            let result_str = result.to_string_lossy();
            // Should have fixed season padding and converted to proper structure
            assert!(result_str.contains("Season 09"));
            assert!(result_str.contains("Scrubs - S09E02"));
        } else {
            panic!("Atomic rules should have transformed the path");
        }
    }

    #[test]
    fn test_atomic_rule_parentheses_to_brackets() {
        let rules = NamingRules::new().unwrap();
        let test_path = Path::new("Series/Super Best Friends Play - FFX/Super Best Friends Play - Final Fantasy X - S01E13 (1080p60).webm");

        if let Some(result) = rules.apply_rules(test_path) {
            assert_eq!(
                result.to_string_lossy(),
                "Series/Super Best Friends Play - FFX/Super Best Friends Play - Final Fantasy X - S01E13 [1080p60].webm"
            );
        } else {
            panic!("Atomic rules should have transformed the path");
        }
    }

    #[test]
    fn test_atomic_rules_complex_transformations() {
        let rules = NamingRules::new().unwrap();
        let test_path = Path::new("Series/Samurai Jack (2001)/Season 3/Samurai.Jack.S03E10.XXXVI.Jack.The.Monks.and.the.Ancient.Master's.Son.avi");

        // This should apply multiple atomic rules in sequence
        if let Some(result) = rules.apply_rules(test_path) {
            let result_str = result.to_string_lossy();
            // Should have fixed season padding at minimum
            assert!(result_str.contains("Season 03"));
            // May have additional transformations applied
        } else {
            panic!("Atomic rules should have transformed the path");
        }
    }

    #[test]
    fn test_atomic_rule_remove_dash_before_quality() {
        let rules = NamingRules::new().unwrap();
        let test_path = Path::new("Series/From/Season 03/From - S03E04 - [720p].mkv");

        if let Some(result) = rules.apply_rules(test_path) {
            assert_eq!(
                result.to_string_lossy(),
                "Series/From/Season 03/From - S03E04 [720p].mkv"
            );
        } else {
            panic!("Atomic rules should have transformed the path");
        }
    }

    #[test]
    fn test_atomic_rule_fix_duplicated_structure() {
        let rules = NamingRules::new().unwrap();
        let test_path = Path::new("Series/Veronica Mars/Series/Veronica Mars/Season 01/Veronica Mars - S01E11.mp4");

        if let Some(result) = rules.apply_rules(test_path) {
            assert_eq!(
                result.to_string_lossy(),
                "Series/Veronica Mars/Season 01/Veronica Mars - S01E11.mp4"
            );
        } else {
            panic!("Atomic rules should have transformed the path");
        }
    }
}
