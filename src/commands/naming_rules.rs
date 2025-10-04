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
            return Err(anyhow::anyhow!("Season index {} out of bounds", season_index));
        }
        
        let season_str = &self.captures[season_index];
        let season_num: u32 = season_str.parse()
            .map_err(|_| anyhow::anyhow!("Invalid season number: {}", season_str))?;
        
        Ok(format!("{:02}", season_num))
    }

    /// Extract episode identifier from captures, ensuring SXXEXX format is uppercase
    pub fn format_episode(&self, episode_index: usize) -> Result<String> {
        if episode_index >= self.captures.len() {
            return Err(anyhow::anyhow!("Episode index {} out of bounds", episode_index));
        }
        
        let episode_str = &self.captures[episode_index].to_uppercase();
        
        // Ensure SXXEXX pattern
        if let Some(caps) = Regex::new(r"S(\d+)E(\d+)")?.captures(episode_str) {
            let season: u32 = caps[1].parse()
                .map_err(|_| anyhow::anyhow!("Invalid season in episode: {}", episode_str))?;
            let episode: u32 = caps[2].parse()
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
            .replace(".", " ")  // Replace dots with spaces
            .replace("_", " ")  // Replace underscores with spaces
            .trim()
            .to_string();
            
        // Remove quality/release metadata (like "RETAIL.DVDRip.XviD-REWARD")
        let clean_regex = Regex::new(r"(?i)\b(retail|dvdrip|webrip|bluray|hdtv|xvid|x264|x265|aac|ac3|dts|1080p|720p|480p|mkv|avi|mp4|webm|[\w\-]+rip|[\w\-]+\d+p?)\b")?;
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
                    Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
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
        
        // Rule 1: Handle standard series pattern without dash between SXXEXX and title
        // Example: Elementary - S06E08 Sand Trap.mkv -> Elementary - S06E08 - Sand Trap.mkv
        rules.push(NamingRule {
            name: "series_missing_dash".to_string(),
            description: "Add missing dash between SXXEXX and episode title".to_string(),
            pattern: Regex::new(r"^Series/([^/]+)/Season (\d+)/([^/]+) - (S\d{2}E\d{2}) ([^/]+)\.(\w+)$")?,
            transform: |m| {
                let series_name = &m.captures[0];
                let season = m.format_season(1)?;
                let show_name = &m.captures[2];
                let episode = m.format_episode(3)?;
                let episode_title = &m.captures[4]; // Don't clean it, just use as-is for this simple case
                let quality = m.extract_quality_metadata(&m.original_path.to_string_lossy());
                
                Ok(PathBuf::from(format!(
                    "Series/{}/Season {}/{} - {} - {}{}{}",
                    series_name, season, show_name, episode, episode_title, quality, m.extension
                )))
            },
        });

        // Rule 2: Handle files with quality metadata that need cleaning
        // Example: Scrubs.S09E02.RETAIL.DVDRip.XviD-REWARD.avi
        rules.push(NamingRule {
            name: "series_metadata_cleanup".to_string(),
            description: "Clean up series files with metadata and fix structure".to_string(),
            pattern: Regex::new(r"^Series/([^/]+)/Season (\d+)/([^.]+)\.(S\d{2}E\d{2})\.(.+)\.(\w+)$")?,
            transform: |m| {
                let series_name = &m.captures[0];
                let season = m.format_season(1)?;
                let show_name = &m.captures[2];
                let episode = m.format_episode(3)?;
                let episode_title = m.clean_episode_title(4)?;
                let quality = m.extract_quality_metadata(&m.captures[4]);
                
                Ok(PathBuf::from(format!(
                    "Series/{}/Season {}/{} - {} - {}{}{}",
                    series_name, season, show_name, episode, episode_title, quality, m.extension
                )))
            },
        });

        // Rule 3: Handle files with quality metadata in parentheses -> brackets
        // Example: Super Best Friends Play - Final Fantasy X - S01E13 (1080p60).webm
        rules.push(NamingRule {
            name: "quality_parentheses_to_brackets".to_string(),
            description: "Convert quality metadata from parentheses to brackets".to_string(),
            pattern: Regex::new(r"^Series/([^/]+)/([^/]+) - (S\d{2}E\d{2}) \(([^)]+)\)\.(\w+)$")?,
            transform: |m| {
                let series_path = &m.captures[0];
                let filename_base = &m.captures[1];
                let episode = m.format_episode(2)?;
                let quality = &m.captures[3];
                
                Ok(PathBuf::from(format!(
                    "Series/{}/{} - {} [{}].{}",
                    series_path, filename_base, episode, quality, m.extension
                )))
            },
        });

        // Rule 4: Handle complex episode titles with extra info
        // Example: Samurai.Jack.S03E10.XXXVI.Jack.The.Monks.and.the.Ancient.Masters.Son.avi
        rules.push(NamingRule {
            name: "complex_episode_title".to_string(),
            description: "Clean complex episode titles and fix structure".to_string(),
            pattern: Regex::new(r"^Series/([^/]+)/Season (\d+)/([^.]+)\.(S\d{2}E\d{2})\.(.+)\.(\w+)$")?,
            transform: |m| {
                let series_name = &m.captures[0];
                let season = m.format_season(1)?;
                let show_name = &m.captures[2];
                let episode = m.format_episode(3)?;
                let raw_title = &m.captures[4];
                
                // Clean the episode title more aggressively for complex cases
                let mut episode_title = raw_title
                    .replace(".", " ")
                    .replace("_", " ")
                    .trim()
                    .to_string();
                
                // Remove Roman numerals and other metadata at the beginning
                let cleanup_regex = Regex::new(r"^(?i)(XC|XL|L?X{0,3})(IX|IV|V?I{0,3})\s*\.?\s*")?;
                episode_title = cleanup_regex.replace(&episode_title, "").to_string();
                
                episode_title = m.clean_episode_title_from_str(&episode_title)?;
                
                let quality = m.extract_quality_metadata(raw_title);
                
                Ok(PathBuf::from(format!(
                    "Series/{}/Season {}/{} - {} - {}{}{}",
                    series_name, season, show_name, episode, episode_title, quality, m.extension
                )))
            },
        });

        Ok(Self { rules })
    }

    /// Try to apply naming rules to a file path
    pub fn apply_rules(&self, file_path: &Path) -> Option<NamingRuleMatch> {
        let path_str = file_path.to_string_lossy().replace("\\", "/");
        
        for rule in &self.rules {
            if let Some(caps) = rule.pattern.captures(&path_str) {
                let captures: Vec<String> = caps.iter()
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
            .replace(".", " ")  // Replace dots with spaces
            .replace("_", " ")  // Replace underscores with spaces
            .trim()
            .to_string();
            
        // Remove quality/release metadata (like "RETAIL.DVDRip.XviD-REWARD")
        let clean_regex = Regex::new(r"(?i)\b(retail|dvdrip|webrip|bluray|hdtv|xvid|x264|x265|aac|ac3|dts|1080p|720p|480p|mkv|avi|mp4|webm|[\w\-]+rip|[\w\-]+\d+p?)\b")?;
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
                    Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
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
}