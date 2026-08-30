use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::naming::{self, LibraryRoot, MediaName};
use crate::paths::to_forward_slashes;
use crate::target::{Conformance, Field, PlaybackTarget};

/// Represents a media file that needs to be transcoded
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: String,
    pub input_path: PathBuf,
    pub output_path: PathBuf,
    pub subtitle_path: Option<PathBuf>,
    pub file_type: MediaFileType,
    /// What this job does to its source. Defaulted so job files written before
    /// it existed deserialize as the re-encode they were queued as.
    #[serde(default)]
    pub operation: Operation,
    pub quality_settings: QualitySettings,
    pub post_processing: PostProcessingSettings,
    /// How many times a worker has tried and failed to transcode this file.
    ///
    /// Carried in the job file so it survives a worker restart, and defaulted so
    /// that job files written before it existed still deserialize.
    #[serde(default)]
    pub attempts: u32,
    /// What went wrong on the most recent failed attempt, so a job parked in
    /// `_failed` says why it is there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// What a job does to its source.
///
/// Most of this library needs no video re-encode: the picture is already in a
/// codec the targets Direct Play, and only the container or the audio is wrong.
/// Copying the bitstream runs at disk speed against days of CPU for a re-encode,
/// so which of the two a job is stays a property of the job, decided once when
/// it is queued.
///
/// The caller resolves it; issue #158 makes that the conformance check, which
/// asks what the source and the target hold rather than what the extension is.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub enum Operation {
    /// Copy the video bitstream into MP4, doing to the other tracks only what
    /// the target needs.
    Remux {
        audio: AudioAction,
        subtitles: SubtitleAction,
    },
    /// Re-encode video and audio to `quality_settings`.
    #[default]
    Reencode,
}

/// What a remux does with the source's audio.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub enum AudioAction {
    #[default]
    Copy,
    /// Re-encode, downmixing to `channels` where the target takes fewer than
    /// the source carries. A layout the client cannot handle is not the same
    /// problem as a codec it cannot decode, and one flag could not say both.
    Transcode { channels: Option<u32> },
}

/// What a remux does with the source's subtitle tracks.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub enum SubtitleAction {
    /// Carry them into the output as `mov_text`.
    #[default]
    Keep,
    /// Leave them out.
    ///
    /// Measured: the LG's Plex app burns `mov_text` into the picture rather
    /// than overlaying it, and re-encodes the video to do it. Carrying a track
    /// the client cannot overlay would cost the transcode a remux exists to
    /// avoid, so on that target the track has to go.
    ///
    /// Reserved for tracks no sidecar can hold. Anything text-based should be
    /// extracted instead: `work` disables the source, so a dropped track is
    /// gone rather than moved.
    Drop,
    /// Take them out of the container and write them beside it as sidecar
    /// files.
    ///
    /// The container then carries no subtitle stream, so nothing is burned in,
    /// and the server converts a sidecar for whatever the client asks for
    /// without touching the picture.
    Extract,
}

impl Operation {
    /// What to do to a file a client was asked about, or `None` where the
    /// answer is to leave it alone.
    ///
    /// `None` is the interesting case and the reason this returns an option
    /// rather than a third variant: most of this library already Direct Plays,
    /// and a file that conforms should produce no job at all. An `Operation`
    /// describes work; "no work" is the absence of one.
    ///
    /// A remux's reasons say which tracks are wrong, and only those are
    /// touched. A wrong container alone changes nothing but the container.
    pub fn for_conformance(conformance: &Conformance, target: &PlaybackTarget) -> Option<Self> {
        let reasons = match conformance {
            Conformance::Conforms { .. } => return None,
            Conformance::Reencode { .. } => return Some(Self::Reencode),
            Conformance::Remux { reasons, .. } => reasons,
        };

        let has = |field: Field| reasons.iter().any(|reason| reason.field == field);

        // A layout the client will not take is fixed by mixing down to the cap
        // it states; a codec it cannot decode is fixed by re-encoding at
        // whatever layout the source has.
        let audio = match (has(Field::AudioChannels), has(Field::AudioCodec)) {
            (true, _) => AudioAction::Transcode {
                channels: Some(target.audio.max_channels.value),
            },
            (false, true) => AudioAction::Transcode { channels: None },
            (false, false) => AudioAction::Copy,
        };

        // The track is why the client would burn the picture, but it is also
        // the subtitles the viewer turned on. It leaves the container and lands
        // beside it rather than being lost.
        let subtitles = if has(Field::Subtitles) {
            SubtitleAction::Extract
        } else {
            SubtitleAction::Keep
        };

        Some(Self::Remux { audio, subtitles })
    }

    /// Whether the output carries the source's subtitle tracks.
    ///
    /// A re-encode always does. The 136 `.mp4` files this project has already
    /// produced carry `mov_text` and fail on the LG for it, so that answer is
    /// wrong too - but fixing it is a re-encode's decision to make, and it
    /// belongs with the conformance check rather than here.
    pub fn keeps_subtitles(&self) -> bool {
        !matches!(
            self,
            Operation::Remux {
                subtitles: SubtitleAction::Drop | SubtitleAction::Extract,
                ..
            }
        )
    }

    /// Whether the source's subtitle tracks are written beside the output
    /// instead of into it.
    pub fn extracts_subtitles(&self) -> bool {
        matches!(
            self,
            Operation::Remux {
                subtitles: SubtitleAction::Extract,
                ..
            }
        )
    }
}

/// Quality settings for video encoding
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QualitySettings {
    pub ffmpeg_preset: String,
    pub ffmpeg_crf: String,
    pub ffmpeg_audio_bitrate: String,
}

/// Predefined quality presets for different use cases
#[derive(Debug, Clone, PartialEq)]
pub enum QualityPreset {
    /// Fast encoding with good quality (veryfast/23/128k)
    Fast,
    /// Balanced encoding speed and quality (medium/20/192k)
    Balanced,
    /// High quality, slower encoding (slow/18/256k)
    Quality,
    /// Ultra-fast encoding for quick previews (ultrafast/28/96k)
    UltraFast,
    /// Archive quality for long-term storage (veryslow/15/320k)
    Archive,
}

/// Post-processing settings for what to do after conversion
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PostProcessingSettings {
    pub disable_source_files: bool,
}

/// Supported media file types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MediaFileType {
    /// WebM file with external VTT subtitle
    WebM,
    /// MKV file with embedded subtitles
    Mkv,
    /// AVI file, remuxed into MP4 with its video copied.
    Avi,
}

/// Episode metadata extracted from file paths for prioritization.
///
/// Only episodes have metadata. Movies and anything else that does not parse as
/// an episode yield `None`, which is what separates the two during prioritization.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EpisodeMetadata {
    pub series_name: String,
    pub season_number: u32,
    pub episode_number: u32,
}

impl Job {
    /// Namespace for the v5 UUIDs that identify jobs by the file they transcode.
    ///
    /// A fixed random namespace, generated once for this project. It must never
    /// change: the ids derived from it are the filenames workers see in the queue.
    const ID_NAMESPACE: Uuid = Uuid::from_bytes([
        0x0d, 0x1c, 0x9d, 0x2f, 0x6b, 0x24, 0x4a, 0x5f, 0x9b, 0x3c, 0x7e, 0x51, 0x28, 0xa4, 0x6f,
        0x13,
    ]);

    /// Identify a job by the file it transcodes.
    ///
    /// The id is derived from the resolved input path rather than generated at
    /// random, which is what makes a job file findable again: two scans of the
    /// same library produce the same id for the same file, so the queue can tell
    /// that the job is already there. Separators are normalised first so that
    /// `C:/media` and `C:\\media` do not describe two different jobs.
    pub fn id_for_input(absolute_input_path: &Path) -> String {
        Uuid::new_v5(
            &Self::ID_NAMESPACE,
            to_forward_slashes(absolute_input_path).as_bytes(),
        )
        .to_string()
    }

    /// Create a new job for a media file with configuration, converting relative paths to absolute
    ///
    /// The operation is the caller's to decide, not this constructor's: a job
    /// is a snapshot of what was resolved at scan time, and once the
    /// conformance check answers that question, an extension is not evidence.
    pub fn new(
        input_path: PathBuf,
        file_type: MediaFileType,
        operation: Operation,
        quality_settings: QualitySettings,
        post_processing: PostProcessingSettings,
        media_root: &Path,
    ) -> Self {
        // Convert relative path to absolute path
        let absolute_input_path = if input_path.is_absolute() {
            input_path
        } else {
            media_root.join(&input_path)
        };

        let output_path = absolute_input_path.with_extension("mp4");

        let subtitle_path = match file_type {
            MediaFileType::WebM => Some(absolute_input_path.with_extension("vtt")),
            // MKV and AVI carry their subtitles themselves.
            MediaFileType::Mkv | MediaFileType::Avi => None,
        };

        Self {
            id: Self::id_for_input(&absolute_input_path),
            input_path: absolute_input_path,
            output_path,
            subtitle_path,
            file_type,
            operation,
            quality_settings,
            post_processing,
            attempts: 0,
            last_error: None,
        }
    }

    /// Get the job file name for the queue
    pub fn job_filename(&self) -> String {
        format!("{}.job", self.id)
    }

    /// Check if the output file already exists (works with both absolute and relative paths)
    pub fn output_exists(&self, media_root: Option<&Path>) -> bool {
        let output_path = if self.output_path.is_absolute() {
            self.output_path.clone()
        } else {
            match media_root {
                Some(root) => root.join(&self.output_path),
                None => self.output_path.clone(),
            }
        };
        output_path.exists()
    }

    /// For WebM files, check if the required subtitle file exists (works with both absolute and relative paths)
    pub fn has_required_subtitle(&self, media_root: Option<&Path>) -> Result<bool> {
        match self.file_type {
            MediaFileType::WebM => {
                if let Some(subtitle_path) = &self.subtitle_path {
                    let full_subtitle_path = if subtitle_path.is_absolute() {
                        subtitle_path.clone()
                    } else {
                        match media_root {
                            Some(root) => root.join(subtitle_path),
                            None => subtitle_path.clone(),
                        }
                    };
                    Ok(full_subtitle_path.exists())
                } else {
                    Err(anyhow!("WebM job should have subtitle path"))
                }
            }
            // MKV and AVI carry their own subtitles.
            MediaFileType::Mkv | MediaFileType::Avi => Ok(true),
        }
    }

    /// Get the full input path (for absolute paths, returns as-is; for relative paths, joins with media_root)
    pub fn full_input_path(&self, media_root: Option<&Path>) -> PathBuf {
        if self.input_path.is_absolute() {
            self.input_path.clone()
        } else {
            match media_root {
                Some(root) => root.join(&self.input_path),
                None => self.input_path.clone(),
            }
        }
    }

    /// Get the full output path (for absolute paths, returns as-is; for relative paths, joins with media_root)
    pub fn full_output_path(&self, media_root: Option<&Path>) -> PathBuf {
        if self.output_path.is_absolute() {
            self.output_path.clone()
        } else {
            match media_root {
                Some(root) => root.join(&self.output_path),
                None => self.output_path.clone(),
            }
        }
    }

    /// Get the work folder output path (where the file is written during transcoding)
    pub fn work_folder_output_path(&self, work_folder: &Path) -> PathBuf {
        // Create a unique filename for the work folder based on job ID and original filename
        let output_filename = self
            .output_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("output.mp4");
        work_folder.join(format!("{}_{}", self.id, output_filename))
    }

    /// Get the full subtitle path if it exists (for absolute paths, returns as-is; for relative paths, joins with media_root)
    pub fn full_subtitle_path(&self, media_root: Option<&Path>) -> Option<PathBuf> {
        self.subtitle_path.as_ref().map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                match media_root {
                    Some(root) => root.join(path),
                    None => path.clone(),
                }
            }
        })
    }

    /// Extract episode metadata from the job's input path for prioritization.
    ///
    /// What counts as a season directory or an episode marker is asked of
    /// `naming`, which owns that question for the whole project. One description
    /// serves both callers, so an unpadded `Season 6`, a `Specials` directory and
    /// a three-digit episode number order here exactly as they render there.
    ///
    /// The path is absolute and carries native separators, so it is normalised
    /// and cut down to the library-relative form `naming::parse` reads. That cut
    /// needs the media root, which the job does not carry; the queue holds it and
    /// passes it in.
    pub fn extract_episode_metadata(&self, media_root: &Path) -> Option<EpisodeMetadata> {
        let relative = Self::library_relative_path(&self.input_path, media_root)?;

        match naming::parse(&relative) {
            Ok(MediaName::Episode(episode)) => Some(EpisodeMetadata {
                series_name: episode.series,
                season_number: episode.season,
                episode_number: episode.number,
            }),
            _ => None,
        }
    }

    /// Cut an absolute input path down to the library-relative form
    /// `naming::parse` reads.
    ///
    /// **The media root is removed before anything looks for a library root.** A
    /// media root is an ordinary directory and is routinely called `Movies` or
    /// `Anime`, or sits below something that is - `/home/bob/Movies`, `/srv/Anime`,
    /// `D:\Media\Series`. Searching the whole absolute path takes that component
    /// as the library root, and then every real root below it reads as a tree
    /// nested into itself: `DuplicatedRoot` for every file in the library at once,
    /// so `--priority episode` degrades in silence to first-available. The failure
    /// is all-or-nothing per library, which is why it is worth a parameter.
    ///
    /// Two roots the media root can name, and both are ordinary:
    ///
    /// - **The media root holds the library.** `/home/bob/Movies` containing
    ///   `Series/`, `Anime/` and `Movies/`. Stripping it leaves the real root
    ///   first, and that is the one taken.
    /// - **The media root *is* a library root.** `/srv/Anime` containing
    ///   `Naruto/Season 01/...`. Stripping it leaves nothing library-shaped, so
    ///   the whole path is searched instead and `Anime` is found where it is.
    ///
    /// The path itself decides which, rather than a guess about the root's name,
    /// and the fallback can only be reached when nothing below the media root is
    /// library-shaped. It also covers a legacy job whose path was resolved
    /// against some other root and so does not sit below this one at all.
    ///
    /// Whichever branch answers, the **outermost** root component below the cut
    /// is taken, the same choice `naming::scope_for` makes, so a tree genuinely
    /// nested into itself is described here the way validation describes it
    /// rather than a second way.
    fn library_relative_path(input_path: &Path, media_root: &Path) -> Option<String> {
        if let Ok(below_media_root) = input_path.strip_prefix(media_root) {
            let below_media_root = to_forward_slashes(below_media_root);

            if let Some(relative) = Self::below_library_root(&below_media_root) {
                return Some(relative.to_string());
            }
        }

        let whole_path = to_forward_slashes(input_path);
        Self::below_library_root(&whole_path).map(str::to_string)
    }

    /// Cut a forward-slash path down to the part at and below the outermost
    /// library root component, if it holds one.
    fn below_library_root(forward_slash_path: &str) -> Option<&str> {
        let mut offset = 0;

        for component in forward_slash_path.split('/') {
            if LibraryRoot::from_component(component).is_some() {
                return Some(&forward_slash_path[offset..]);
            }
            offset += component.len() + 1;
        }

        None
    }
}

impl QualitySettings {
    /// Create quality settings from environment variables with defaults
    pub fn from_env() -> Self {
        use std::env;
        Self {
            ffmpeg_preset: env::var("FFMPEG_PRESET").unwrap_or_else(|_| "veryfast".to_string()),
            ffmpeg_crf: env::var("FFMPEG_CRF").unwrap_or_else(|_| "23".to_string()),
            ffmpeg_audio_bitrate: env::var("FFMPEG_AUDIO_BITRATE")
                .unwrap_or_else(|_| "128k".to_string()),
        }
    }

    /// Create quality settings from a preset, with optional environment variable overrides
    pub fn from_preset(preset: QualityPreset) -> Self {
        use std::env;
        let base = preset.to_quality_settings();

        Self {
            ffmpeg_preset: env::var("FFMPEG_PRESET").unwrap_or(base.ffmpeg_preset),
            ffmpeg_crf: env::var("FFMPEG_CRF").unwrap_or(base.ffmpeg_crf),
            ffmpeg_audio_bitrate: env::var("FFMPEG_AUDIO_BITRATE")
                .unwrap_or(base.ffmpeg_audio_bitrate),
        }
    }

    /// Create quality settings from a preset name string
    pub fn from_preset_name(preset_name: &str) -> Result<Self> {
        let preset = QualityPreset::from_name(preset_name)?;
        Ok(Self::from_preset(preset))
    }
}

impl QualityPreset {
    /// Convert preset to quality settings
    pub fn to_quality_settings(&self) -> QualitySettings {
        match self {
            QualityPreset::Fast => QualitySettings {
                ffmpeg_preset: "veryfast".to_string(),
                ffmpeg_crf: "23".to_string(),
                ffmpeg_audio_bitrate: "128k".to_string(),
            },
            QualityPreset::Balanced => QualitySettings {
                ffmpeg_preset: "medium".to_string(),
                ffmpeg_crf: "20".to_string(),
                ffmpeg_audio_bitrate: "192k".to_string(),
            },
            QualityPreset::Quality => QualitySettings {
                ffmpeg_preset: "slow".to_string(),
                ffmpeg_crf: "18".to_string(),
                ffmpeg_audio_bitrate: "256k".to_string(),
            },
            QualityPreset::UltraFast => QualitySettings {
                ffmpeg_preset: "ultrafast".to_string(),
                ffmpeg_crf: "28".to_string(),
                ffmpeg_audio_bitrate: "96k".to_string(),
            },
            QualityPreset::Archive => QualitySettings {
                ffmpeg_preset: "veryslow".to_string(),
                ffmpeg_crf: "15".to_string(),
                ffmpeg_audio_bitrate: "320k".to_string(),
            },
        }
    }

    /// Parse preset from string name
    pub fn from_name(name: &str) -> Result<Self> {
        match name.to_lowercase().as_str() {
            "fast" => Ok(QualityPreset::Fast),
            "balanced" => Ok(QualityPreset::Balanced),
            "quality" => Ok(QualityPreset::Quality),
            "ultrafast" => Ok(QualityPreset::UltraFast),
            "archive" => Ok(QualityPreset::Archive),
            _ => Err(anyhow!(
                "Unknown quality preset '{name}'. Available presets: fast, balanced, quality, ultrafast, archive"
            )),
        }
    }

    /// Get all available preset names
    #[allow(dead_code)]
    pub fn all_names() -> Vec<&'static str> {
        vec!["fast", "balanced", "quality", "ultrafast", "archive"]
    }

    /// Get the preset name as string
    #[allow(dead_code)]
    pub fn name(&self) -> &'static str {
        match self {
            QualityPreset::Fast => "fast",
            QualityPreset::Balanced => "balanced",
            QualityPreset::Quality => "quality",
            QualityPreset::UltraFast => "ultrafast",
            QualityPreset::Archive => "archive",
        }
    }
}

impl Default for QualitySettings {
    fn default() -> Self {
        Self {
            ffmpeg_preset: "veryfast".to_string(),
            ffmpeg_crf: "23".to_string(),
            ffmpeg_audio_bitrate: "128k".to_string(),
        }
    }
}

impl Default for PostProcessingSettings {
    fn default() -> Self {
        Self {
            disable_source_files: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::path::PathBuf;

    /// Clear the encoding environment variables so a test observes documented
    /// defaults rather than whatever the developer's shell happens to export.
    /// Only safe inside `#[serial(env)]` tests.
    fn clear_encoding_env() {
        std::env::remove_var("FFMPEG_PRESET");
        std::env::remove_var("FFMPEG_CRF");
        std::env::remove_var("FFMPEG_AUDIO_BITRATE");
    }

    #[test]
    fn test_webm_job_creation() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = PathBuf::from("/test/media");
        let job = Job::new(
            PathBuf::from("video.webm"),
            MediaFileType::WebM,
            Operation::Reencode,
            quality,
            post_processing,
            &media_root,
        );
        assert_eq!(job.input_path, PathBuf::from("/test/media/video.webm"));
        assert_eq!(job.file_type, MediaFileType::WebM);
        assert_eq!(job.output_path, PathBuf::from("/test/media/video.mp4"));
        assert_eq!(
            job.subtitle_path,
            Some(PathBuf::from("/test/media/video.vtt"))
        );
    }

    #[test]
    fn a_file_that_direct_plays_produces_no_operation_at_all() {
        // The whole point of asking: roughly 1500 of 2421 files need nothing,
        // and a job for one of them is work invented out of an extension.
        let conforms = Conformance::Conforms {
            unverified: Vec::new(),
        };
        let target = PlaybackTarget::builtin("lg-cx-webos").unwrap().unwrap();

        assert_eq!(Operation::for_conformance(&conforms, &target), None);
    }

    /// The three remux shapes the two envelopes actually produce, taken from
    /// `evaluate` rather than assembled by hand - a mapping tested against
    /// findings nothing generates is a mapping of nothing.
    #[test]
    fn each_remux_reason_asks_for_exactly_the_track_it_is_about() {
        use crate::probe::{AudioStream, MediaProbe, SubtitleStream, VideoStream};
        use crate::target::evaluate;

        let chromecast = PlaybackTarget::builtin("chromecast-gen2-3")
            .unwrap()
            .unwrap();
        let lg = PlaybackTarget::builtin("lg-cx-webos").unwrap().unwrap();

        let conforming = || MediaProbe {
            container: Some("mov,mp4,m4a".to_string()),
            video: Some(VideoStream {
                codec: "h264".to_string(),
                profile: Some("high".to_string()),
                level: Some(41),
                pixel_format: Some("yuv420p".to_string()),
                bit_depth: Some(8),
                ref_frames: Some(4),
                width: Some(1920),
                height: Some(1080),
            }),
            audio: vec![AudioStream {
                codec: "aac".to_string(),
                channels: Some(2),
                language: None,
            }],
            ..Default::default()
        };

        // A codec the client cannot decode: re-encode the audio, leave the
        // layout as the source has it.
        let mut wrong_codec = conforming();
        wrong_codec.audio[0].codec = "ac3".to_string();
        assert_eq!(
            Operation::for_conformance(&evaluate(&wrong_codec, &chromecast), &chromecast),
            Some(Operation::Remux {
                audio: AudioAction::Transcode { channels: None },
                subtitles: SubtitleAction::Keep,
            })
        );

        // A layout it will not take: mix down to the cap the target states.
        let mut too_many_channels = conforming();
        too_many_channels.audio[0].channels = Some(6);
        assert_eq!(
            Operation::for_conformance(&evaluate(&too_many_channels, &chromecast), &chromecast),
            Some(Operation::Remux {
                audio: AudioAction::Transcode { channels: Some(2) },
                subtitles: SubtitleAction::Keep,
            })
        );

        // A track the client burns in: take it out of the container and write
        // it beside the file, and do not touch the audio. It is the subtitles
        // the viewer turned on, so losing it is not a fix.
        let mut burned_in = conforming();
        burned_in.subtitles = vec![SubtitleStream {
            codec: "mov_text".to_string(),
            language: None,
        }];
        assert_eq!(
            Operation::for_conformance(&evaluate(&burned_in, &lg), &lg),
            Some(Operation::Remux {
                audio: AudioAction::Copy,
                subtitles: SubtitleAction::Extract,
            })
        );

        // The measured AVI case: only the container is wrong, so only the
        // container changes.
        let mut avi = conforming();
        avi.container = Some("avi".to_string());
        avi.video.as_mut().unwrap().codec = "mpeg4".to_string();
        avi.video.as_mut().unwrap().profile = None;
        avi.video.as_mut().unwrap().level = None;
        assert_eq!(
            Operation::for_conformance(&evaluate(&avi, &lg), &lg),
            Some(Operation::Remux {
                audio: AudioAction::Copy,
                subtitles: SubtitleAction::Keep,
            }),
            "an AVI that needs only its container copies every track"
        );
    }

    #[test]
    fn an_avi_becomes_an_mp4_beside_itself_and_carries_the_given_operation() {
        let remux = Operation::Remux {
            audio: AudioAction::Copy,
            subtitles: SubtitleAction::Drop,
        };
        let job = Job::new(
            PathBuf::from("video.avi"),
            MediaFileType::Avi,
            remux,
            QualitySettings::default(),
            PostProcessingSettings::default(),
            &PathBuf::from("/test/media"),
        );

        assert_eq!(job.output_path, PathBuf::from("/test/media/video.mp4"));
        assert_eq!(job.subtitle_path, None);
        // Taken from the caller, not guessed from the extension.
        assert_eq!(job.operation, remux);
    }

    #[test]
    fn test_mkv_job_creation() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = PathBuf::from("/test/media");
        let job = Job::new(
            PathBuf::from("video.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode,
            quality,
            post_processing,
            &media_root,
        );
        assert_eq!(job.input_path, PathBuf::from("/test/media/video.mkv"));
        assert_eq!(job.file_type, MediaFileType::Mkv);
        assert_eq!(job.output_path, PathBuf::from("/test/media/video.mp4"));
        assert_eq!(job.subtitle_path, None);
    }

    #[test]
    #[serial(env)]
    fn test_quality_settings_from_env() {
        clear_encoding_env();

        std::env::set_var("FFMPEG_PRESET", "fast");
        std::env::set_var("FFMPEG_CRF", "20");
        std::env::set_var("FFMPEG_AUDIO_BITRATE", "192k");

        let quality = QualitySettings::from_env();
        assert_eq!(quality.ffmpeg_preset, "fast");
        assert_eq!(quality.ffmpeg_crf, "20");
        assert_eq!(quality.ffmpeg_audio_bitrate, "192k");

        clear_encoding_env();
    }

    #[test]
    fn test_absolute_paths() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = PathBuf::from("/media/root");
        let job = Job::new(
            PathBuf::from("/absolute/path/video.webm"),
            MediaFileType::WebM,
            Operation::Reencode,
            quality,
            post_processing,
            &media_root,
        );

        // Test that absolute paths stay absolute (ignores media_root)
        assert_eq!(
            job.full_input_path(None),
            PathBuf::from("/absolute/path/video.webm")
        );
        assert_eq!(
            job.full_output_path(None),
            PathBuf::from("/absolute/path/video.mp4")
        );
        assert_eq!(
            job.full_subtitle_path(None),
            Some(PathBuf::from("/absolute/path/video.vtt"))
        );

        // Test that absolute paths ignore media_root parameter passed to full_* methods
        let different_root = PathBuf::from("/different/root");
        assert_eq!(
            job.full_input_path(Some(&different_root)),
            PathBuf::from("/absolute/path/video.webm")
        );
        assert_eq!(
            job.full_output_path(Some(&different_root)),
            PathBuf::from("/absolute/path/video.mp4")
        );
    }

    #[test]
    fn test_relative_paths_with_media_root() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = PathBuf::from("/media/root");
        let job = Job::new(
            PathBuf::from("relative/video.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode,
            quality,
            post_processing,
            &media_root,
        );

        // Test that relative paths are converted to absolute during job creation
        assert_eq!(
            job.input_path,
            PathBuf::from("/media/root/relative/video.mkv")
        );
        assert_eq!(
            job.output_path,
            PathBuf::from("/media/root/relative/video.mp4")
        );
        assert_eq!(job.subtitle_path, None); // MKV has no external subtitles

        // Test that full_* methods return the absolute paths directly
        assert_eq!(
            job.full_input_path(None),
            PathBuf::from("/media/root/relative/video.mkv")
        );
        assert_eq!(
            job.full_output_path(None),
            PathBuf::from("/media/root/relative/video.mp4")
        );
    }

    #[test]
    fn test_quality_presets() {
        // Test Fast preset
        let fast = QualityPreset::Fast.to_quality_settings();
        assert_eq!(fast.ffmpeg_preset, "veryfast");
        assert_eq!(fast.ffmpeg_crf, "23");
        assert_eq!(fast.ffmpeg_audio_bitrate, "128k");

        // Test Quality preset
        let quality = QualityPreset::Quality.to_quality_settings();
        assert_eq!(quality.ffmpeg_preset, "slow");
        assert_eq!(quality.ffmpeg_crf, "18");
        assert_eq!(quality.ffmpeg_audio_bitrate, "256k");

        // Test Balanced preset
        let balanced = QualityPreset::Balanced.to_quality_settings();
        assert_eq!(balanced.ffmpeg_preset, "medium");
        assert_eq!(balanced.ffmpeg_crf, "20");
        assert_eq!(balanced.ffmpeg_audio_bitrate, "192k");
    }

    #[test]
    fn test_preset_from_name() {
        assert_eq!(
            QualityPreset::from_name("fast").unwrap(),
            QualityPreset::Fast
        );
        assert_eq!(
            QualityPreset::from_name("QUALITY").unwrap(),
            QualityPreset::Quality
        );
        assert_eq!(
            QualityPreset::from_name("Balanced").unwrap(),
            QualityPreset::Balanced
        );

        // Test invalid preset name
        assert!(QualityPreset::from_name("invalid").is_err());
    }

    #[test]
    #[serial(env)]
    fn test_quality_settings_from_preset() {
        clear_encoding_env();

        let settings = QualitySettings::from_preset(QualityPreset::Quality);
        assert_eq!(settings.ffmpeg_preset, "slow");
        assert_eq!(settings.ffmpeg_crf, "18");
        assert_eq!(settings.ffmpeg_audio_bitrate, "256k");
    }

    #[test]
    #[serial(env)]
    fn test_quality_settings_from_preset_name() {
        clear_encoding_env();

        let settings = QualitySettings::from_preset_name("balanced").unwrap();
        assert_eq!(settings.ffmpeg_preset, "medium");
        assert_eq!(settings.ffmpeg_crf, "20");
        assert_eq!(settings.ffmpeg_audio_bitrate, "192k");

        // Test invalid name
        assert!(QualitySettings::from_preset_name("invalid").is_err());
    }

    #[test]
    #[serial(env)]
    fn test_preset_with_env_override() {
        clear_encoding_env();

        // Set environment variables
        std::env::set_var("FFMPEG_PRESET", "custom");
        std::env::set_var("FFMPEG_CRF", "25");

        let settings = QualitySettings::from_preset(QualityPreset::Quality);
        assert_eq!(settings.ffmpeg_preset, "custom"); // Overridden by env
        assert_eq!(settings.ffmpeg_crf, "25"); // Overridden by env
        assert_eq!(settings.ffmpeg_audio_bitrate, "256k"); // From preset

        clear_encoding_env();
    }

    #[test]
    fn test_job_serialization() {
        let quality = QualitySettings {
            ffmpeg_preset: "medium".to_string(),
            ffmpeg_crf: "18".to_string(),
            ffmpeg_audio_bitrate: "256k".to_string(),
        };
        let post_processing = PostProcessingSettings {
            disable_source_files: false,
        };
        let media_root = PathBuf::from("/test/media");
        let job = Job::new(
            PathBuf::from("test.webm"),
            MediaFileType::WebM,
            Operation::Reencode,
            quality.clone(),
            post_processing.clone(),
            &media_root,
        );

        // Test JSON serialization/deserialization
        let json = serde_json::to_string(&job).unwrap();
        let deserialized: Job = serde_json::from_str(&json).unwrap();

        assert_eq!(job.input_path, deserialized.input_path);
        assert_eq!(job.output_path, deserialized.output_path);
        assert_eq!(job.subtitle_path, deserialized.subtitle_path);
        assert_eq!(job.file_type, deserialized.file_type);
        assert_eq!(
            job.quality_settings.ffmpeg_preset,
            deserialized.quality_settings.ffmpeg_preset
        );
        assert_eq!(
            job.post_processing.disable_source_files,
            deserialized.post_processing.disable_source_files
        );
    }

    #[test]
    fn test_work_folder_output_path() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = PathBuf::from("/media/root");
        let job = Job::new(
            PathBuf::from("videos/movie.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode,
            quality,
            post_processing,
            &media_root,
        );

        let work_folder = PathBuf::from("/tmp/work");
        let work_output_path = job.work_folder_output_path(&work_folder);

        // Should include job ID and original filename
        assert!(work_output_path.starts_with(&work_folder));
        assert!(work_output_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .contains(&job.id));
        assert!(work_output_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with("movie.mp4"));
    }

    #[test]
    fn test_post_processing_defaults() {
        let settings = PostProcessingSettings::default();
        assert!(settings.disable_source_files);
    }

    #[test]
    fn test_episode_metadata_extraction_series() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = PathBuf::from("/media");

        // Test standard series format
        let job = Job::new(
            PathBuf::from("Series/Breaking Bad/Season 01/Breaking Bad - s01e03 - Gray Matter.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode,
            quality.clone(),
            post_processing.clone(),
            &media_root,
        );

        let metadata = job.extract_episode_metadata(&media_root).unwrap();
        assert_eq!(metadata.series_name, "Breaking Bad");
        assert_eq!(metadata.season_number, 1);
        assert_eq!(metadata.episode_number, 3);
    }

    #[test]
    fn test_episode_metadata_extraction_series_with_tvdb() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = PathBuf::from("/media");

        // Test series with TVDB ID
        let job = Job::new(
            PathBuf::from(
                "Series/Breaking Bad (2008) {tvdb-296861}/Season 01/Breaking Bad S01E01 Pilot.mkv",
            ),
            MediaFileType::Mkv,
            Operation::Reencode,
            quality.clone(),
            post_processing.clone(),
            &media_root,
        );

        let metadata = job.extract_episode_metadata(&media_root).unwrap();
        assert_eq!(metadata.series_name, "Breaking Bad");
        assert_eq!(metadata.season_number, 1);
        assert_eq!(metadata.episode_number, 1);
    }

    #[test]
    fn test_episode_metadata_extraction_anime() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = PathBuf::from("/media");

        // Test anime format
        let job = Job::new(
            PathBuf::from(
                "Anime/Attack on Titan/Season 01/Attack on Titan S01E05 First Battle.mkv",
            ),
            MediaFileType::Mkv,
            Operation::Reencode,
            quality.clone(),
            post_processing.clone(),
            &media_root,
        );

        let metadata = job.extract_episode_metadata(&media_root).unwrap();
        assert_eq!(metadata.series_name, "Attack on Titan");
        assert_eq!(metadata.season_number, 1);
        assert_eq!(metadata.episode_number, 5);
    }

    #[test]
    fn test_episode_metadata_extraction_season_with_extra_info() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = PathBuf::from("/media");

        // Test season with extra information
        let job = Job::new(
            PathBuf::from("Series/Critical Role (2015) {tvdb-296861}/Season 01 - Vox Machina/Critical Role S01E12 Arrival at Kraghammer.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode,
            quality.clone(),
            post_processing.clone(),
            &media_root,
        );

        let metadata = job.extract_episode_metadata(&media_root).unwrap();
        assert_eq!(metadata.series_name, "Critical Role");
        assert_eq!(metadata.season_number, 1);
        assert_eq!(metadata.episode_number, 12);
    }

    #[test]
    fn test_episode_metadata_extraction_movie_returns_none() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = PathBuf::from("/media");

        // Test movie format - should return None
        let job = Job::new(
            PathBuf::from("Movies/The Dark Knight (2008)/The Dark Knight (2008).mkv"),
            MediaFileType::Mkv,
            Operation::Reencode,
            quality.clone(),
            post_processing.clone(),
            &media_root,
        );

        let metadata = job.extract_episode_metadata(&media_root);
        assert!(metadata.is_none());
    }

    /// Metadata for a library-relative path, as a scan would queue it.
    fn metadata_for(relative_path: PathBuf) -> Option<EpisodeMetadata> {
        metadata_under(Path::new("/media"), relative_path)
    }

    /// Metadata for a path queued by a scan of `media_root`.
    ///
    /// The two arguments are the two a scan supplies, and keeping them apart is
    /// the point: `Job::new` joins them into one absolute path, and what the
    /// prioritiser has to do is take the join back apart.
    fn metadata_under(media_root: &Path, relative_path: PathBuf) -> Option<EpisodeMetadata> {
        Job::new(
            relative_path,
            MediaFileType::Mkv,
            Operation::Reencode,
            QualitySettings::default(),
            PostProcessingSettings::default(),
            media_root,
        )
        .extract_episode_metadata(media_root)
    }

    #[test]
    fn episode_metadata_reads_a_path_with_native_separators() {
        let expected = EpisodeMetadata {
            series_name: "Elementary".to_string(),
            season_number: 6,
            episode_number: 8,
        };

        // Built component-wise, which is what a scan hands over: `WalkDir` and
        // `strip_prefix` yield the platform's own separator throughout, and a
        // path written as a `/` literal is a shape the scanner never produces.
        let joined = PathBuf::from("Series")
            .join("Elementary")
            .join("Season 06")
            .join("Elementary - S06E08 - Sand Trap.mkv");
        assert_eq!(metadata_for(joined), Some(expected.clone()));

        // Spelled out as well, so the normalisation is asserted on every
        // platform rather than only on the one that produces backslashes.
        let backslashes =
            PathBuf::from(r"Series\Elementary\Season 06\Elementary - S06E08 - Sand Trap.mkv");
        assert_eq!(metadata_for(backslashes), Some(expected));
    }

    #[test]
    fn episode_metadata_covers_the_season_directories_a_library_holds() {
        let unpadded = metadata_for(
            PathBuf::from("Series")
                .join("Elementary")
                .join("Season 6")
                .join("Elementary - S06E08 - Sand Trap.mkv"),
        );
        assert_eq!(
            unpadded,
            Some(EpisodeMetadata {
                series_name: "Elementary".to_string(),
                season_number: 6,
                episode_number: 8,
            })
        );

        let specials = metadata_for(
            PathBuf::from("Series")
                .join("Firefly")
                .join("Specials")
                .join("Firefly - S00E01 - Here's How It Was.mkv"),
        );
        assert_eq!(
            specials,
            Some(EpisodeMetadata {
                series_name: "Firefly".to_string(),
                season_number: 0,
                episode_number: 1,
            })
        );
    }

    #[test]
    fn a_media_root_named_after_a_library_root_does_not_hide_the_library() {
        // `/home/bob/Movies` holding `Series/`, `Anime/` and `Movies/` is an
        // ordinary layout, and so is an anime-only mount at `/srv/Anime`. The
        // component that names a root is part of the *media root* here, not of
        // the library, and a search of the whole absolute path takes it anyway -
        // which makes every real root below it read as a tree nested into itself
        // and costs the metadata for the entire library at once.
        let expected = EpisodeMetadata {
            series_name: "Elementary".to_string(),
            season_number: 1,
            episode_number: 1,
        };

        let episode = PathBuf::from("Series")
            .join("Elementary")
            .join("Season 01")
            .join("Elementary - S01E01 - Pilot.mkv");

        for media_root in ["/home/bob/Movies", "/srv/Anime", r"D:\Media\Series"] {
            assert_eq!(
                metadata_under(Path::new(media_root), episode.clone()),
                Some(expected.clone()),
                "a library under a media root at '{media_root}'"
            );
        }
    }

    #[test]
    fn a_media_root_that_is_itself_a_library_root_still_reads() {
        // The other thing `/srv/Anime` can mean: the root itself, with series
        // directories directly inside it. Nothing below the media root is
        // library-shaped, so the root is found where it actually is.
        let episode = metadata_under(
            Path::new("/srv/Anime"),
            PathBuf::from("Naruto")
                .join("Season 01")
                .join("Naruto - S01E01 - Enter Naruto.mkv"),
        );

        assert_eq!(
            episode,
            Some(EpisodeMetadata {
                series_name: "Naruto".to_string(),
                season_number: 1,
                episode_number: 1,
            })
        );
    }

    #[test]
    fn a_three_digit_episode_number_is_read_whole() {
        let long_running = metadata_for(
            PathBuf::from("Anime")
                .join("One Piece")
                .join("Season 01")
                .join("One Piece - S01E108 - Dashing Onto The Scene.mkv"),
        );
        assert_eq!(long_running.unwrap().episode_number, 108);
    }

    #[test]
    fn a_three_digit_episode_sorts_after_the_two_digit_ones() {
        let episode = |number: &str| {
            metadata_for(
                PathBuf::from("Anime")
                    .join("One Piece")
                    .join("Season 01")
                    .join(format!("One Piece - S01E{number} - Episode.mkv")),
            )
            .unwrap()
        };

        assert!(episode("09") < episode("11"));
        assert!(episode("107") < episode("108"));
        assert!(episode("11") < episode("107"));
    }

    #[test]
    fn every_episode_naming_parses_yields_prioritization_metadata() {
        // Both answer the same question about the same file, so a path one of
        // them calls an episode cannot be a path the other passes over.
        let paths = [
            "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv",
            "Series/Elementary/Season 6/Elementary - S06E08 - Sand Trap.mkv",
            "Series/Firefly/Specials/Firefly - S00E01 - Here's How It Was.mkv",
            "Series/Breaking Bad (2008) {tvdb-296861}/Season 01/Breaking Bad S01E01 Pilot.mkv",
            "Series/Critical Role/Season 01 - Vox Machina/Critical Role S01E12 Kraghammer.mkv",
            "Series/The Wire/Season 02/the.wire.s02e05.1080p.mkv",
            "Series/Charmed/Charmed - S06E12 - Prince Charmed.mkv",
            "Series/Elementary/Season 01/Elementary - S01E02 - Extras/S01E02 clip.mkv",
            "Anime/One Piece/Season 01/One Piece - S01E108 - Dashing Onto The Scene.mkv",
            "Anime/Attack on Titan/Season 01/Attack on Titan S01E05 First Battle.mkv",
            "Movies/The Dark Knight (2008)/The Dark Knight (2008).mkv",
            "Random/Path/file.mkv",
            // Carries an episode marker and is still not a canonical episode.
            // `naming` refuses both of these today, so both take the `_` arm and
            // are asserted to have no metadata - see the seam test below for what
            // that costs.
            "Series/Elementary/Season 01/Elementary - S01E13.5 - Recap.mkv",
            "Series/Veronica Mars/Series/Season 01/Veronica Mars - S01E01 - Pilot.mkv",
        ];

        for path in paths {
            let parsed = crate::naming::parse(path);
            let metadata = metadata_for(PathBuf::from(path));

            match parsed {
                Ok(MediaName::Episode(episode)) => {
                    let metadata = metadata.unwrap_or_else(|| {
                        panic!("'{path}' parses as an episode but has no metadata")
                    });
                    assert_eq!(
                        metadata.season_number, episode.season,
                        "season for '{path}'"
                    );
                    assert_eq!(
                        metadata.episode_number, episode.number,
                        "episode for '{path}'"
                    );
                    assert_eq!(metadata.series_name, episode.series, "series for '{path}'");
                }
                _ => assert!(
                    metadata.is_none(),
                    "'{path}' is not an episode but yielded {metadata:?}"
                ),
            }
        }
    }

    /// A file `naming` will not name is demoted, not ordered.
    ///
    /// This is the seam #133 is about, pinned as it currently stands so that
    /// changing it has to be deliberate. A path carrying a perfectly legible
    /// `S01E13` still yields no metadata when anything else about it is
    /// unresolvable, and `claim_prioritized_job` sorts `None` behind `Some`
    /// rather than filtering it - so the file is still transcoded, just at the
    /// back of the queue in `read_dir` order, with nothing logged.
    ///
    /// Both shapes below are refused on `main` today, so this records existing
    /// behaviour rather than introducing it. Giving the prioritiser a sort key
    /// that can fall back where a *destination* may not is #133's decision to
    /// make, not this test's.
    #[test]
    fn a_marker_naming_refuses_is_demoted_rather_than_ordered() {
        let fractional = "Series/Elementary/Season 01/Elementary - S01E13.5 - Recap.mkv";
        let nested = "Series/Veronica Mars/Series/Season 01/Veronica Mars - S01E01 - Pilot.mkv";

        for path in [fractional, nested] {
            assert!(
                crate::naming::parse(path).is_err(),
                "'{path}' is meant to be a path naming refuses"
            );
            assert_eq!(
                metadata_for(PathBuf::from(path)),
                None,
                "'{path}' carries a marker but is demoted, not ordered"
            );
        }
    }

    #[test]
    fn test_episode_metadata_extraction_invalid_format_returns_none() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = PathBuf::from("/media");

        // Test invalid format - should return None
        let job = Job::new(
            PathBuf::from("Random/Path/file.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode,
            quality.clone(),
            post_processing.clone(),
            &media_root,
        );

        let metadata = job.extract_episode_metadata(&media_root);
        assert!(metadata.is_none());
    }

    #[test]
    fn job_id_is_derived_from_the_input_path() {
        let media_root = Path::new("/media");
        let job = |name: &str| {
            Job::new(
                PathBuf::from(name),
                MediaFileType::Mkv,
                Operation::Reencode,
                QualitySettings::default(),
                PostProcessingSettings::default(),
                media_root,
            )
        };

        assert_eq!(
            job("show.mkv").id,
            job("show.mkv").id,
            "the same file must always produce the same job id"
        );
        assert_ne!(job("show.mkv").id, job("other.mkv").id);
    }

    #[test]
    fn job_id_ignores_separator_style() {
        let forward = Job::id_for_input(Path::new("C:/media/Series/show.mkv"));
        let native = Job::id_for_input(&Path::new("C:/media").join("Series").join("show.mkv"));

        assert_eq!(forward, native);
    }

    #[test]
    fn job_filename_is_the_id() {
        let job = Job::new(
            PathBuf::from("show.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode,
            QualitySettings::default(),
            PostProcessingSettings::default(),
            Path::new("/media"),
        );

        assert_eq!(job.job_filename(), format!("{}.job", job.id));
    }
}
