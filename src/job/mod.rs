use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::naming::{self, EpisodeSortKey, LibraryRoot};
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
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum Operation {
    /// Copy the video bitstream into MP4, doing to the other tracks only what
    /// the target needs.
    Remux {
        audio: AudioAction,
        subtitles: SubtitleAction,
    },
    /// Re-encode video and audio to `quality_settings`.
    ///
    /// `channels` caps the layout of the track that comes out, for the same
    /// reason a remux does: a client that decodes AAC and takes two channels
    /// is not served by 5.1 AAC. Re-encoding the picture does not make the
    /// audio somebody else's problem.
    Reencode { channels: Option<u32> },
}

impl Default for Operation {
    /// What a job file written before operations existed deserializes as: the
    /// re-encode it was queued as, with no cap, because nothing had asked a
    /// client anything when it was made.
    fn default() -> Self {
        Self::Reencode { channels: None }
    }
}

/// What a remux does with the source's audio.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub enum AudioAction {
    /// Every client plays one of the tracks the file already has.
    #[default]
    Copy,
    /// Keep every track the file has and add one more that the clients the
    /// originals do not serve can play.
    ///
    /// Never the answer for a single client - it is what *disagreement*
    /// resolves to. The Chromecast is measured refusing AC3 in every layout
    /// and the LG refusing Opus, so 235 files play on one and not the other.
    /// Replacing the track would fix that by taking 5.1 away from the device
    /// that was managing perfectly well; adding one lets the server hand each
    /// client the track it can decode.
    Add { channels: Option<u32> },
    /// Replace the audio.
    ///
    /// For when *no* named client plays any track the file has, which makes
    /// keeping one bytes nobody can decode. The original is still in the
    /// source, which `work` renames rather than deletes.
    Transcode { channels: Option<u32> },
}

impl AudioAction {
    /// The action that satisfies both of two targets' answers.
    ///
    /// `Add` is not produced for any single client; it is what this function
    /// makes of a disagreement. `Copy` from one target means that target plays
    /// a track the file already has, so that track has to survive; `Transcode`
    /// from another means no existing track serves it, so a new one is needed.
    /// Both at once is exactly "keep what is there and add one", which is why
    /// adding is the answer to two clients rather than a preference.
    ///
    /// Where every target says `Transcode`, nothing the file carries serves
    /// anybody and replacing is right.
    fn hardest(self, other: Self) -> Self {
        match (self, other) {
            (Self::Copy, Self::Copy) => Self::Copy,

            // One client is served by a track that is already there, another
            // is not. Keeping both is the only answer that serves both.
            (Self::Copy, Self::Transcode { channels })
            | (Self::Transcode { channels }, Self::Copy) => Self::Add { channels },
            (Self::Copy, added @ Self::Add { .. }) | (added @ Self::Add { .. }, Self::Copy) => {
                added
            }

            // Once anything has to be added, adding covers a client that would
            // have settled for a replacement too: it can play the new track.
            (Self::Add { channels: one }, Self::Add { channels: other })
            | (Self::Add { channels: one }, Self::Transcode { channels: other })
            | (Self::Transcode { channels: one }, Self::Add { channels: other }) => Self::Add {
                channels: narrower(one, other),
            },

            (Self::Transcode { channels: one }, Self::Transcode { channels: other }) => {
                Self::Transcode {
                    channels: narrower(one, other),
                }
            }
        }
    }

    /// The channel count the new track is given, or `None` to leave the
    /// source's layout alone.
    pub fn channels(self) -> Option<u32> {
        match self {
            Self::Copy => None,
            Self::Add { channels } | Self::Transcode { channels } => channels,
        }
    }
}

/// The cap that is inside both. A cap and no cap is the cap - re-encoding at
/// six channels does not satisfy the device that will only take two.
fn narrower(one: Option<u32>, other: Option<u32>) -> Option<u32> {
    match (one, other) {
        (Some(one), Some(other)) => Some(one.min(other)),
        (Some(cap), None) | (None, Some(cap)) => Some(cap),
        (None, None) => None,
    }
}

/// What a remux does with the source's subtitle tracks.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub enum SubtitleAction {
    /// Carry them into the output as `mov_text`.
    #[default]
    Keep,
    /// Leave them out, and write nothing in their place.
    ///
    /// Measured: the LG's Plex app burns `mov_text` into the picture rather
    /// than overlaying it, and re-encodes the video to do it. Carrying a track
    /// the client cannot overlay would cost the transcode a remux exists to
    /// avoid, so on that target the track has to go.
    ///
    /// **Nothing produces this.** `Extract` empties the container just as
    /// thoroughly and keeps the tracks, so there is no case left where losing
    /// them outright is the better answer. It is kept for one reason that is
    /// not a guess: a job file is a self-describing snapshot that outlives the
    /// code which wrote it, and the version of `for_conformance` that shipped
    /// with the remux path produced this. A queue holding one of those jobs
    /// still has to deserialize. Removing the variant is safe only once no
    /// such job file can exist.
    Drop,
    /// Take the subtitles out of the container and write the ones that can be
    /// written beside it as sidecar files.
    ///
    /// The container then carries no subtitle stream, so nothing is burned in,
    /// and the server converts a sidecar for whatever the client asks for
    /// without touching the picture.
    ///
    /// Note what the name does not promise: **every** stream leaves the
    /// container, but only text becomes a sidecar. An image-based track - PGS,
    /// VobSub - is pictures of text, and turning it into text would be OCR, so
    /// it is reported and left where it is. On that path this does what `Drop`
    /// does, and the only copy of the track is then the source file, which
    /// `work` keeps beside the output as `.disabled` rather than deleting.
    Extract,
}

impl SubtitleAction {
    /// The action that satisfies both of two targets' answers.
    ///
    /// Read each as what one client *requires*, which is what makes the order
    /// fall out rather than being chosen. `Keep` requires nothing: that client
    /// is content with the track where it is. `Extract` requires the track to
    /// leave the container. `Drop` requires the same and adds that no sidecar
    /// can hold it - which is a fact about the track rather than about the
    /// client, so two targets cannot disagree about it.
    ///
    /// So `Drop` beats `Extract` beats `Keep`, and the middle case is the one
    /// that changed: extracting to satisfy a client that burns the track in
    /// costs the other client nothing, because the server hands a sidecar to
    /// any of them and converts it without touching the picture. Combining
    /// used to mean a device that rendered the track perfectly well lost it
    /// because another one burned it in. It no longer does.
    fn hardest(self, other: Self) -> Self {
        match (self, other) {
            (Self::Drop, _) | (_, Self::Drop) => Self::Drop,
            (Self::Extract, _) | (_, Self::Extract) => Self::Extract,
            (Self::Keep, Self::Keep) => Self::Keep,
        }
    }
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
            Conformance::Reencode { reasons, .. } | Conformance::Remux { reasons, .. } => reasons,
        };

        let has = |field: Field| reasons.iter().any(|reason| reason.field == field);

        // A re-encode writes a new audio track whatever was wrong with the
        // picture, so it takes the same ceiling a remux does: re-encoding a
        // picture does not make the audio somebody else's problem.
        if matches!(conformance, Conformance::Reencode { .. }) {
            return Some(Self::Reencode {
                channels: has(Field::AudioChannels).then_some(target.audio.max_channels.value),
            });
        }

        // The channel count is a ceiling to mix down to, not a layout to
        // produce, so it is asked for only where the file is above it. A
        // verdict now reports the codec fault and the layout fault separately
        // even when both hold, which is what lets these three cases stay
        // apart: reaching for the cap on every fault upmixes stereo into 5.1,
        // and reaching for it on none leaves a 5.1 track in a codec the client
        // decodes at a layout it refuses. Both have shipped here.
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

    /// The work that leaves a file playable on every target that asked for
    /// some, where each of `self` and `other` is one target's answer.
    ///
    /// The expensive answer wins, because it is the only one that satisfies
    /// both. This is the rule that decides a quarter of this library: the two
    /// shipped envelopes disagree about 586 MPEG-4 AVIs, which the LG decodes
    /// and the Chromecast does not, so a remux against one is a re-encode
    /// against the pair.
    ///
    /// Going the other way is not a cheaper version of the same thing. A file
    /// left conforming on only one device is one the Pi is asked to transcode
    /// the moment it is played on the other, and the Pi cannot transcode video
    /// at all - the playback does not degrade, it collapses. Days of encoding
    /// bought once is the lesser cost.
    pub fn hardest(self, other: Self) -> Self {
        match (self, other) {
            (Self::Reencode { channels: one }, Self::Reencode { channels: other }) => {
                Self::Reencode {
                    channels: narrower(one, other),
                }
            }

            // A re-encode absorbs a remux, but not the remux's cap: the client
            // that only wanted its audio changed still only takes two channels,
            // and the file it gets is now the re-encoded one.
            (Self::Reencode { channels }, Self::Remux { audio, .. })
            | (Self::Remux { audio, .. }, Self::Reencode { channels }) => Self::Reencode {
                channels: narrower(channels, audio.channels()),
            },
            (
                Self::Remux { audio, subtitles },
                Self::Remux {
                    audio: other_audio,
                    subtitles: other_subtitles,
                },
            ) => Self::Remux {
                audio: audio.hardest(other_audio),
                subtitles: subtitles.hardest(other_subtitles),
            },
        }
    }

    /// Whether the operation encodes any audio, and so needs to know what the
    /// source's layout is before it can decide what to ask for.
    pub fn touches_audio(&self) -> bool {
        matches!(
            self,
            Operation::Reencode { .. }
                | Operation::Remux {
                    audio: AudioAction::Add { .. } | AudioAction::Transcode { .. },
                    ..
                }
        )
    }

    /// The most channels any track this operation writes may carry.
    pub fn audio_cap(&self) -> Option<u32> {
        match self {
            Operation::Reencode { channels } => *channels,
            Operation::Remux { audio, .. } => audio.channels(),
        }
    }

    /// Whether the output gains an audio track the source did not have.
    ///
    /// The command that does it needs the source's audio stream count, so this
    /// is what decides whether that probe is worth running.
    pub fn adds_an_audio_track(&self) -> bool {
        matches!(
            self,
            Operation::Remux {
                audio: AudioAction::Add { .. },
                ..
            }
        )
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

    /// Where this job's input sits in the order episodes should be worked in.
    ///
    /// What counts as a season directory or an episode marker is asked of
    /// `naming`, which owns that question for the whole project. One description
    /// serves both callers, so an unpadded `Season 6`, a `Specials` directory and
    /// a three-digit episode number order here exactly as they render there.
    ///
    /// It asks `naming::sort_key` rather than `naming::parse`, and that is the
    /// point rather than a shortcut. `parse` refuses a path it cannot *name*, and
    /// a refusal collapsing into `None` here demotes the file to the back of the
    /// queue in `read_dir` order with nothing said about it - silently, and for
    /// exactly the files `validate --fix` also refuses, so they are the ones that
    /// will still be in that shape next month.
    ///
    /// The path is absolute and carries native separators, so it is normalised
    /// and cut down to the library-relative form `naming` reads. That cut needs
    /// the media root, which the job does not carry; the queue holds it and
    /// passes it in.
    pub fn episode_sort_key(&self, media_root: &Path) -> Option<EpisodeSortKey> {
        let relative = Self::library_relative_path(&self.input_path, media_root)?;

        naming::sort_key(Path::new(&relative))
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
            Operation::Reencode { channels: None },
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
    fn combining_two_targets_keeps_the_subtitles_rather_than_losing_them() {
        use SubtitleAction::{Drop, Extract, Keep};

        // The case the two shipped envelopes produce: the LG burns `mov_text`
        // in, the Chromecast overlays it. Extracting satisfies the first and
        // costs the second nothing, because the server hands a sidecar to
        // either of them without touching the picture.
        assert_eq!(Keep.hardest(Extract), Extract);
        assert_eq!(Extract.hardest(Keep), Extract);

        assert_eq!(Keep.hardest(Keep), Keep);
        assert_eq!(Extract.hardest(Extract), Extract);

        // A track no sidecar can hold is a fact about the track, not about the
        // client, so it cannot be argued away by a target that would have kept
        // it.
        assert_eq!(Keep.hardest(Drop), Drop);
        assert_eq!(Extract.hardest(Drop), Drop);
    }

    /// The channel count in a `Transcode` is a ceiling to mix down to, not a
    /// layout to produce. Both directions, because getting one right by
    /// assuming the other has been wrong twice here.
    #[test]
    fn a_replacement_track_is_capped_without_being_widened() {
        use crate::probe::{AudioStream, MediaProbe, VideoStream};
        use crate::target::evaluate;

        let chromecast = PlaybackTarget::builtin("chromecast-gen2-3")
            .unwrap()
            .unwrap();
        let lg = PlaybackTarget::builtin("lg-cx-webos").unwrap().unwrap();

        let with_audio = |codec: &str, channels: u32| MediaProbe {
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
                codec: codec.to_string(),
                channels: Some(channels),
                language: None,
            }],
            ..Default::default()
        };

        // Above the cap: mix down to it. A 5.1 AAC track is a codec the
        // Chromecast decodes at a layout it refuses.
        let surround = with_audio("ac3", 6);
        assert_eq!(
            Operation::for_conformance(&evaluate(&surround, &chromecast), &chromecast),
            Some(Operation::Remux {
                audio: AudioAction::Transcode { channels: Some(2) },
                subtitles: SubtitleAction::Keep,
            })
        );

        // Below the cap: leave the layout alone. The LG takes six channels and
        // every one of this library's 342 Opus tracks is stereo or mono, so
        // reaching for the cap would invent surround out of stereo.
        let stereo = with_audio("opus", 2);
        assert_eq!(
            Operation::for_conformance(&evaluate(&stereo, &lg), &lg),
            Some(Operation::Remux {
                audio: AudioAction::Transcode { channels: None },
                subtitles: SubtitleAction::Keep,
            })
        );
    }

    #[test]
    fn combining_audio_takes_the_cap_that_is_inside_both() {
        use AudioAction::{Add, Copy, Transcode};

        assert_eq!(Copy.hardest(Copy), Copy);

        // The case that matters, and the whole of #161: one target plays a
        // track the file already has, the other cannot. Replacing would take
        // 5.1 away from the device that was managing; adding serves both.
        assert_eq!(
            Copy.hardest(Transcode { channels: Some(2) }),
            Add { channels: Some(2) }
        );
        assert_eq!(
            Transcode { channels: Some(2) }.hardest(Copy),
            Add { channels: Some(2) }
        );

        // Nothing the file carries serves anybody, so keeping a track would be
        // bytes no client can decode.
        assert_eq!(
            Transcode { channels: None }.hardest(Transcode { channels: Some(2) }),
            Transcode { channels: Some(2) }
        );
        assert_eq!(
            Transcode { channels: Some(6) }.hardest(Transcode { channels: Some(2) }),
            Transcode { channels: Some(2) }
        );

        // Folding a third target onto an addition keeps it an addition: the
        // track that was added is one the newcomer can play too.
        assert_eq!(
            Add { channels: Some(2) }.hardest(Transcode { channels: Some(6) }),
            Add { channels: Some(2) }
        );
        assert_eq!(
            Add { channels: Some(2) }.hardest(Copy),
            Add { channels: Some(2) }
        );
    }

    /// Confirmed rather than assumed: 932 files already carry AAC, and a file
    /// whose AAC stereo track already serves a client must never reach the
    /// track-adding path, because there is nothing to add.
    ///
    /// `evaluate` picks whichever track plays, so the AC3 5.1 beside it is not
    /// a fault - it is the surround the LG is welcome to.
    #[test]
    fn a_file_that_already_carries_a_playable_track_conforms_and_is_never_queued() {
        use crate::probe::{AudioStream, MediaProbe, VideoStream};
        use crate::target::evaluate;

        let chromecast = PlaybackTarget::builtin("chromecast-gen2-3")
            .unwrap()
            .unwrap();
        let lg = PlaybackTarget::builtin("lg-cx-webos").unwrap().unwrap();

        let probe = MediaProbe {
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
            audio: vec![
                AudioStream {
                    codec: "ac3".to_string(),
                    channels: Some(6),
                    language: Some("eng".to_string()),
                },
                AudioStream {
                    codec: "aac".to_string(),
                    channels: Some(2),
                    language: Some("eng".to_string()),
                },
            ],
            ..Default::default()
        };

        for target in [&chromecast, &lg] {
            assert_eq!(
                Operation::for_conformance(&evaluate(&probe, target), target),
                None,
                "{} should need nothing done to this file",
                target.name
            );
        }

        // And the two together, which is the fold `scan --target` performs.
        let combined = [&chromecast, &lg]
            .into_iter()
            .filter_map(|target| Operation::for_conformance(&evaluate(&probe, target), target))
            .reduce(Operation::hardest);
        assert_eq!(combined, None, "no job at all, so #161 never sees it");
    }

    /// A track produced for a client has to be one that client can play, and
    /// that is both halves at once: the codec it decodes *and* a layout inside
    /// its cap. 5.1 AC3 re-encoded to 5.1 AAC for a device that takes two
    /// channels is a file that still does not play.
    #[test]
    fn a_track_made_for_a_client_is_inside_that_client_s_channel_cap() {
        use crate::probe::{AudioStream, MediaProbe, VideoStream};
        use crate::target::evaluate;

        let chromecast = PlaybackTarget::builtin("chromecast-gen2-3")
            .unwrap()
            .unwrap();

        let probe = MediaProbe {
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
            // The measured population: AC3, which the Chromecast refuses in
            // every layout, carrying more channels than it will take.
            audio: vec![AudioStream {
                codec: "ac3".to_string(),
                channels: Some(6),
                language: None,
            }],
            ..Default::default()
        };

        assert_eq!(
            Operation::for_conformance(&evaluate(&probe, &chromecast), &chromecast),
            Some(Operation::Remux {
                audio: AudioAction::Transcode { channels: Some(2) },
                subtitles: SubtitleAction::Keep,
            })
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

        // A codec the client cannot decode, in a layout it takes: produce one
        // it can and leave the layout alone. Asking for the cap here would
        // widen a stereo track towards a ceiling it was never near.
        let mut wrong_codec = conforming();
        wrong_codec.audio[0].codec = "ac3".to_string();
        assert_eq!(
            Operation::for_conformance(&evaluate(&wrong_codec, &chromecast), &chromecast),
            Some(Operation::Remux {
                audio: AudioAction::Transcode { channels: None },
                subtitles: SubtitleAction::Keep,
            })
        );

        // Both at once, which is the case the two above are each half of: a
        // track that fixes only the codec still does not play.
        let mut wrong_codec_and_layout = conforming();
        wrong_codec_and_layout.audio[0].codec = "ac3".to_string();
        wrong_codec_and_layout.audio[0].channels = Some(6);
        assert_eq!(
            Operation::for_conformance(
                &evaluate(&wrong_codec_and_layout, &chromecast),
                &chromecast
            ),
            Some(Operation::Remux {
                audio: AudioAction::Transcode { channels: Some(2) },
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
            Operation::Reencode { channels: None },
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
            Operation::Reencode { channels: None },
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
            Operation::Reencode { channels: None },
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
            Operation::Reencode { channels: None },
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
            Operation::Reencode { channels: None },
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
    fn a_sort_key_comes_off_a_standard_series_path() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = PathBuf::from("/media");

        // Test standard series format
        let job = Job::new(
            PathBuf::from("Series/Breaking Bad/Season 01/Breaking Bad - s01e03 - Gray Matter.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
            quality.clone(),
            post_processing.clone(),
            &media_root,
        );

        let key = job.episode_sort_key(&media_root).unwrap();
        assert_eq!(key.series_directory, "Series/Breaking Bad");
        assert_eq!(key.season, 1);
        assert_eq!(key.episode, 3);
    }

    #[test]
    fn an_annotated_series_directory_is_the_group_as_written() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = PathBuf::from("/media");

        // Test series with TVDB ID
        let job = Job::new(
            PathBuf::from(
                "Series/Breaking Bad (2008) {tvdb-296861}/Season 01/Breaking Bad S01E01 Pilot.mkv",
            ),
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
            quality.clone(),
            post_processing.clone(),
            &media_root,
        );

        let key = job.episode_sort_key(&media_root).unwrap();
        assert_eq!(
            key.series_directory,
            "Series/Breaking Bad (2008) {tvdb-296861}"
        );
        assert_eq!(key.season, 1);
        assert_eq!(key.episode, 1);
    }

    #[test]
    fn an_anime_path_sorts_the_same_way() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = PathBuf::from("/media");

        // Test anime format
        let job = Job::new(
            PathBuf::from(
                "Anime/Attack on Titan/Season 01/Attack on Titan S01E05 First Battle.mkv",
            ),
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
            quality.clone(),
            post_processing.clone(),
            &media_root,
        );

        let key = job.episode_sort_key(&media_root).unwrap();
        assert_eq!(key.series_directory, "Anime/Attack on Titan");
        assert_eq!(key.season, 1);
        assert_eq!(key.episode, 5);
    }

    #[test]
    fn an_arc_named_season_directory_is_not_part_of_the_group() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = PathBuf::from("/media");

        // Test season with extra information
        let job = Job::new(
            PathBuf::from("Series/Critical Role (2015) {tvdb-296861}/Season 01 - Vox Machina/Critical Role S01E12 Arrival at Kraghammer.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
            quality.clone(),
            post_processing.clone(),
            &media_root,
        );

        let key = job.episode_sort_key(&media_root).unwrap();
        assert_eq!(
            key.series_directory,
            "Series/Critical Role (2015) {tvdb-296861}"
        );
        assert_eq!(key.season, 1);
        assert_eq!(key.episode, 12);
    }

    #[test]
    fn a_film_has_no_sort_key_and_so_sorts_last() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = PathBuf::from("/media");

        // Test movie format - should return None
        let job = Job::new(
            PathBuf::from("Movies/The Dark Knight (2008)/The Dark Knight (2008).mkv"),
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
            quality.clone(),
            post_processing.clone(),
            &media_root,
        );

        assert!(job.episode_sort_key(&media_root).is_none());
    }

    /// The sort key for a library-relative path, as a scan would queue it.
    fn sort_key_for(relative_path: PathBuf) -> Option<EpisodeSortKey> {
        sort_key_under(Path::new("/media"), relative_path)
    }

    /// The sort key for a path queued by a scan of `media_root`.
    ///
    /// The two arguments are the two a scan supplies, and keeping them apart is
    /// the point: `Job::new` joins them into one absolute path, and what the
    /// prioritiser has to do is take the join back apart.
    fn sort_key_under(media_root: &Path, relative_path: PathBuf) -> Option<EpisodeSortKey> {
        Job::new(
            relative_path,
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
            QualitySettings::default(),
            PostProcessingSettings::default(),
            media_root,
        )
        .episode_sort_key(media_root)
    }

    #[test]
    fn a_sort_key_reads_a_path_with_native_separators() {
        let expected = EpisodeSortKey {
            series_directory: "Series/Elementary".to_string(),
            season: 6,
            episode: 8,
        };

        // Built component-wise, which is what a scan hands over: `WalkDir` and
        // `strip_prefix` yield the platform's own separator throughout, and a
        // path written as a `/` literal is a shape the scanner never produces.
        let joined = PathBuf::from("Series")
            .join("Elementary")
            .join("Season 06")
            .join("Elementary - S06E08 - Sand Trap.mkv");
        assert_eq!(sort_key_for(joined), Some(expected.clone()));

        // Spelled out as well, so the normalisation is asserted on every
        // platform rather than only on the one that produces backslashes.
        let backslashes =
            PathBuf::from(r"Series\Elementary\Season 06\Elementary - S06E08 - Sand Trap.mkv");
        assert_eq!(sort_key_for(backslashes), Some(expected));
    }

    #[test]
    fn a_sort_key_covers_the_season_directories_a_library_holds() {
        let unpadded = sort_key_for(
            PathBuf::from("Series")
                .join("Elementary")
                .join("Season 6")
                .join("Elementary - S06E08 - Sand Trap.mkv"),
        );
        assert_eq!(
            unpadded,
            Some(EpisodeSortKey {
                series_directory: "Series/Elementary".to_string(),
                season: 6,
                episode: 8,
            })
        );

        let specials = sort_key_for(
            PathBuf::from("Series")
                .join("Firefly")
                .join("Specials")
                .join("Firefly - S00E01 - Here's How It Was.mkv"),
        );
        assert_eq!(
            specials,
            Some(EpisodeSortKey {
                series_directory: "Series/Firefly".to_string(),
                season: 0,
                episode: 1,
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
        let expected = EpisodeSortKey {
            series_directory: "Series/Elementary".to_string(),
            season: 1,
            episode: 1,
        };

        let episode = PathBuf::from("Series")
            .join("Elementary")
            .join("Season 01")
            .join("Elementary - S01E01 - Pilot.mkv");

        for media_root in ["/home/bob/Movies", "/srv/Anime", r"D:\Media\Series"] {
            assert_eq!(
                sort_key_under(Path::new(media_root), episode.clone()),
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
        let episode = sort_key_under(
            Path::new("/srv/Anime"),
            PathBuf::from("Naruto")
                .join("Season 01")
                .join("Naruto - S01E01 - Enter Naruto.mkv"),
        );

        assert_eq!(
            episode,
            Some(EpisodeSortKey {
                series_directory: "Anime/Naruto".to_string(),
                season: 1,
                episode: 1,
            })
        );
    }

    #[test]
    fn a_three_digit_episode_is_read_whole() {
        let long_running = sort_key_for(
            PathBuf::from("Anime")
                .join("One Piece")
                .join("Season 01")
                .join("One Piece - S01E108 - Dashing Onto The Scene.mkv"),
        );
        assert_eq!(long_running.unwrap().episode, 108);
    }

    #[test]
    fn a_three_digit_episode_sorts_after_the_two_digit_ones() {
        let episode = |number: &str| {
            sort_key_for(
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
    fn every_episode_naming_parses_also_sorts() {
        // The sort key answers for more paths than `parse` does, but never for
        // fewer, and where both speak they must agree about which file this is.
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
        ];

        for path in paths {
            let Ok(crate::naming::MediaName::Episode(episode)) = crate::naming::parse(path) else {
                panic!("'{path}' is meant to parse as an episode");
            };
            let key = sort_key_for(PathBuf::from(path))
                .unwrap_or_else(|| panic!("'{path}' parses as an episode but has no sort key"));

            assert_eq!(key.season, episode.season, "season for '{path}'");
            assert_eq!(key.episode, episode.number, "episode for '{path}'");

            // The group is where the parse says the file is - the root and the
            // directories above the season directory - rather than the series
            // name the parse recovered from the file.
            let expected: Vec<&str> = std::iter::once(episode.root.as_str())
                .chain(episode.directories.iter().map(String::as_str))
                .collect();
            assert_eq!(
                key.series_directory,
                expected.join("/"),
                "group for '{path}'"
            );
        }
    }

    #[test]
    fn nothing_episodic_in_the_path_means_no_sort_key() {
        for path in [
            // A film: under a root that holds no episodes.
            "Movies/The Dark Knight (2008)/The Dark Knight (2008).mkv",
            // Under no library root at all.
            "Random/Path/file.mkv",
            // Under an episodic root, but the filename states no episode.
            "Series/Firefly/Season 01/Serenity.mkv",
        ] {
            assert_eq!(sort_key_for(PathBuf::from(path)), None, "for '{path}'");
        }
    }

    /// Issue #133: a file `naming` will not *name* is still a file the queue can
    /// order.
    ///
    /// Every path here carries a legible marker and is refused by `parse` for a
    /// reason that has nothing to do with the marker - a directory named after an
    /// episode, half an episode, a tree nested into itself. Collapsing those
    /// refusals into `None` sorted them behind every parseable episode in the
    /// library, in `read_dir` order, with nothing logged - and they are precisely
    /// the files `validate --fix` also refuses, so they stay in that shape.
    #[test]
    fn a_path_naming_refuses_is_still_ordered() {
        for (path, expected) in [
            (
                "Series/S01E01/Season 01/S01E01 - x.mkv",
                EpisodeSortKey {
                    series_directory: "Series/S01E01".to_string(),
                    season: 1,
                    episode: 1,
                },
            ),
            (
                "Series/Elementary/Season 01/Elementary - S01E13.5 - Recap.mkv",
                EpisodeSortKey {
                    series_directory: "Series/Elementary".to_string(),
                    season: 1,
                    episode: 13,
                },
            ),
            (
                "Series/Veronica Mars/Series/Season 01/Veronica Mars - S01E01 - Pilot.mkv",
                EpisodeSortKey {
                    series_directory: "Series/Veronica Mars/Series".to_string(),
                    season: 1,
                    episode: 1,
                },
            ),
        ] {
            assert!(
                crate::naming::parse(path).is_err(),
                "'{path}' is meant to be a path naming refuses to name"
            );
            assert_eq!(
                sort_key_for(PathBuf::from(path)),
                Some(expected),
                "'{path}' carries a marker and must be ordered by it"
            );
        }
    }

    /// A half episode ties with the whole one it sits beside, and that is the
    /// answer rather than a gap in it: they are adjacent in the season, so
    /// either order is right, and there is no third episode a tie can let in.
    #[test]
    fn a_half_episode_sorts_beside_the_episode_it_follows() {
        let whole = sort_key_for(PathBuf::from(
            "Series/Elementary/Season 01/Elementary - S01E13 - Real.mkv",
        ));
        let half = sort_key_for(PathBuf::from(
            "Series/Elementary/Season 01/Elementary - S01E13.5 - Recap.mkv",
        ));

        assert_eq!(whole, half);
        assert!(whole.is_some());
    }

    #[test]
    fn a_path_under_no_library_root_has_no_sort_key() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = PathBuf::from("/media");

        // Test invalid format - should return None
        let job = Job::new(
            PathBuf::from("Random/Path/file.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
            quality.clone(),
            post_processing.clone(),
            &media_root,
        );

        assert!(job.episode_sort_key(&media_root).is_none());
    }

    #[test]
    fn job_id_is_derived_from_the_input_path() {
        let media_root = Path::new("/media");
        let job = |name: &str| {
            Job::new(
                PathBuf::from(name),
                MediaFileType::Mkv,
                Operation::Reencode { channels: None },
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
            Operation::Reencode { channels: None },
            QualitySettings::default(),
            PostProcessingSettings::default(),
            Path::new("/media"),
        );

        assert_eq!(job.job_filename(), format!("{}.job", job.id));
    }
}
