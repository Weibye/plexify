//! Which subtitle tracks become sidecar files, and what they are called.
//!
//! Measured on the LG: `mov_text` in the container is burned into the picture
//! rather than overlaid, which costs a full video transcode and a downscale.
//! A sidecar `.srt` is converted by the server for whatever the client wants
//! and never touches the video.
//!
//! Two rules here are not negotiable, and both come from the same fact: `work`
//! disables the source once a job succeeds, so a track that is not extracted is
//! gone for good.
//!
//! - **Every text track is extracted.** Preferring English decides what things
//!   are called and what order they are reported in, never what survives.
//! - **Nothing is converted away that cannot be reconstructed.** SRT holds
//!   timing and text. An ASS track that positions and styles its events holds
//!   more than that, so where one does, the original is kept beside the SRT.
//!
//! Image-based tracks cannot become text at all. They are reported, never
//! guessed at.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::ffmpeg::{is_bitmap_subtitle, SubtitleStream};

/// The tags a track carrying English is labelled with in this library.
const ENGLISH_TAGS: [&str; 4] = ["en", "eng", "english", "en-us"];

/// Formats whose events carry more than SRT can express.
const STYLED_CODECS: [&str; 3] = ["ass", "ssa", "mov_text"];

/// What a sidecar file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidecarFormat {
    /// Converted to SubRip, which is what a client renders without the server
    /// touching the picture.
    Srt,
    /// The track copied out as it was, because SRT cannot hold what it carries.
    /// Written only where the source turns out to use that expressiveness.
    Original,
}

/// One file this plan would write next to the output.
#[derive(Debug, Clone, PartialEq)]
pub struct Sidecar {
    /// Position among the source's subtitle streams: FFmpeg's `0:s:{n}`.
    pub stream_index: usize,
    pub path: PathBuf,
    pub format: SidecarFormat,
    pub language: Option<String>,
    pub forced: bool,
}

impl Sidecar {
    /// The FFmpeg arguments that write this one file from `input`.
    pub fn ffmpeg_args(&self, input: &Path) -> Vec<String> {
        let codec = match self.format {
            SidecarFormat::Srt => "srt",
            SidecarFormat::Original => "copy",
        };

        [
            "-v",
            "error",
            "-y",
            "-i",
            &input.to_string_lossy(),
            "-map",
            &format!("0:s:{}", self.stream_index),
            "-c:s",
            codec,
            &self.path.to_string_lossy(),
        ]
        .iter()
        .map(|argument| argument.to_string())
        .collect()
    }
}

/// What a source's subtitle tracks turn into.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubtitlePlan {
    pub sidecars: Vec<Sidecar>,
    /// Tracks no sidecar can hold. Image-based subtitles are pictures of text;
    /// turning them into text is OCR, which is a guess, and this project does
    /// not guess at a library nobody can reconstruct.
    pub unconvertible: Vec<SubtitleStream>,
}

impl SubtitlePlan {
    pub fn is_empty(&self) -> bool {
        self.sidecars.is_empty() && self.unconvertible.is_empty()
    }

    /// The sidecars that are conversions, rather than preserved originals.
    pub fn converted(&self) -> impl Iterator<Item = &Sidecar> {
        self.sidecars
            .iter()
            .filter(|sidecar| sidecar.format == SidecarFormat::Srt)
    }
}

/// Work out what to write beside `output` for a source carrying `streams`.
///
/// `output` is the media file the sidecars sit next to, so they are named after
/// it and move with it: `src/fix.rs` treats any sibling sharing the media
/// file's stem as part of the same group.
pub fn plan(streams: &[SubtitleStream], output: &Path) -> SubtitlePlan {
    let stem = output.file_stem().unwrap_or_default().to_string_lossy();
    let directory = output.parent().unwrap_or(Path::new(""));

    let mut ordered: Vec<(usize, &SubtitleStream)> = streams.iter().enumerate().collect();
    // English first, and a full track before the forced one that accompanies
    // it. This decides which of two same-language tracks gets the plain name;
    // every one of them is still written.
    ordered.sort_by_key(|(index, stream)| {
        (
            !is_english(stream.language.as_deref()),
            stream.forced,
            *index,
        )
    });

    let mut sidecars = Vec::new();
    let mut unconvertible = Vec::new();
    let mut taken: HashMap<String, usize> = HashMap::new();

    for (stream_index, stream) in ordered {
        if is_bitmap_subtitle(&stream.codec) {
            unconvertible.push(stream.clone());
            continue;
        }

        let language = stream.language.as_deref().map(normalise_language);
        let mut push = |format: SidecarFormat, extension: &str| {
            let path = directory.join(unique_name(
                &stem,
                language.as_deref(),
                stream.forced,
                extension,
                &mut taken,
            ));

            sidecars.push(Sidecar {
                stream_index,
                path,
                format,
                language: language.clone(),
                forced: stream.forced,
            });
        };

        push(SidecarFormat::Srt, "srt");

        // Proposed, not promised: whether the track actually uses what SRT
        // cannot hold is a property of its events, which only reading them can
        // settle. The extractor drops this file again where it does not.
        if STYLED_CODECS.contains(&stream.codec.as_str()) {
            push(SidecarFormat::Original, subtitle_extension(&stream.codec));
        }
    }

    SubtitlePlan {
        sidecars,
        unconvertible,
    }
}

/// Whether an ASS or SSA track uses anything SRT cannot represent.
///
/// Override blocks (`{\pos(..)}`, `{\an8}`, `{\c&H..}`) are how a typeset sign
/// is placed and coloured, and a converted SRT keeps none of it. A track whose
/// events carry no override is dialogue, and converting that loses nothing.
pub fn carries_styling(subtitle: &str) -> bool {
    subtitle
        .lines()
        .filter(|line| {
            let lower = line.trim_start().to_ascii_lowercase();
            lower.starts_with("dialogue:") || lower.starts_with("comment:")
        })
        .any(|line| line.contains("{\\"))
}

/// Whether a language tag names English, in any of the spellings this library
/// uses for it.
pub fn is_english(language: Option<&str>) -> bool {
    language
        .map(|tag| ENGLISH_TAGS.contains(&normalise_language(tag).as_str()))
        .unwrap_or(false)
}

/// Lowercased and trimmed. The tag is not translated between two-letter and
/// three-letter forms: Plex reads both, and rewriting `en` as `eng` would be
/// this code inventing a value it was not given.
fn normalise_language(tag: &str) -> String {
    tag.trim().to_ascii_lowercase()
}

fn subtitle_extension(codec: &str) -> &'static str {
    match codec {
        "ssa" => "ssa",
        "mov_text" => "ttxt",
        _ => "ass",
    }
}

/// `Show - S01E01.en.srt`, and `.en.2.srt` for a second English track.
///
/// A file with no language tag gets none in its name rather than a guessed one.
fn unique_name(
    stem: &str,
    language: Option<&str>,
    forced: bool,
    extension: &str,
    taken: &mut HashMap<String, usize>,
) -> String {
    let mut parts = String::new();
    if let Some(language) = language {
        parts.push('.');
        parts.push_str(language);
    }
    if forced {
        parts.push_str(".forced");
    }

    let key = format!("{parts}.{extension}");
    let seen = taken.entry(key.clone()).or_insert(0);
    *seen += 1;

    match *seen {
        1 => format!("{stem}{key}"),
        nth => format!("{stem}{parts}.{nth}.{extension}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(codec: &str, language: Option<&str>, forced: bool) -> SubtitleStream {
        SubtitleStream {
            codec: codec.to_string(),
            language: language.map(str::to_string),
            forced,
        }
    }

    fn output() -> PathBuf {
        PathBuf::from("/media/Series/Show/Season 01/Show - S01E01 - Pilot [1080p].mp4")
    }

    fn names(plan: &SubtitlePlan) -> Vec<String> {
        plan.sidecars
            .iter()
            .map(|sidecar| {
                sidecar
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_sidecar_is_named_after_the_media_file_it_sits_beside() {
        let plan = plan(&[stream("subrip", Some("eng"), false)], &output());

        assert_eq!(names(&plan), ["Show - S01E01 - Pilot [1080p].eng.srt"]);
        assert_eq!(plan.sidecars[0].path.parent(), output().parent());
    }

    /// `work` disables the source, so a track left behind is lost. Preferring
    /// English orders the output; it does not decide what survives.
    #[test]
    fn every_text_track_is_extracted_not_only_the_english_one() {
        let plan = plan(
            &[
                stream("subrip", Some("spa"), false),
                stream("subrip", Some("eng"), false),
                stream("ass", Some("jpn"), false),
            ],
            &output(),
        );

        assert_eq!(plan.converted().count(), 3);
        // English first, so it takes the plain name and leads the report.
        assert!(names(&plan)[0].ends_with(".eng.srt"));
    }

    #[test]
    fn a_forced_track_is_kept_apart_from_the_full_one() {
        let plan = plan(
            &[
                stream("subrip", Some("eng"), true),
                stream("subrip", Some("eng"), false),
            ],
            &output(),
        );

        let names = names(&plan);
        assert!(names.contains(&"Show - S01E01 - Pilot [1080p].eng.srt".to_string()));
        assert!(names.contains(&"Show - S01E01 - Pilot [1080p].eng.forced.srt".to_string()));
    }

    #[test]
    fn two_tracks_in_one_language_do_not_collide() {
        let plan = plan(
            &[
                stream("subrip", Some("eng"), false),
                stream("subrip", Some("eng"), false),
            ],
            &output(),
        );

        assert_eq!(
            names(&plan),
            [
                "Show - S01E01 - Pilot [1080p].eng.srt",
                "Show - S01E01 - Pilot [1080p].eng.2.srt"
            ]
        );
    }

    #[test]
    fn an_untagged_track_is_not_given_a_language_it_never_declared() {
        let plan = plan(&[stream("subrip", None, false)], &output());

        assert_eq!(names(&plan), ["Show - S01E01 - Pilot [1080p].srt"]);
    }

    /// OCR is a guess, and this runs against a library nobody can rebuild.
    #[test]
    fn image_based_tracks_are_reported_rather_than_converted() {
        let plan = plan(
            &[
                stream("hdmv_pgs_subtitle", Some("eng"), false),
                stream("dvd_subtitle", Some("eng"), false),
                stream("subrip", Some("eng"), false),
            ],
            &output(),
        );

        assert_eq!(plan.converted().count(), 1);
        assert_eq!(plan.unconvertible.len(), 2);
    }

    /// An ASS track is proposed twice: the SRT that Direct Plays, and the
    /// original, which the extractor keeps only if the events need it.
    #[test]
    fn an_ass_track_proposes_its_original_alongside_the_conversion() {
        let plan = plan(&[stream("ass", Some("eng"), false)], &output());

        assert_eq!(names(&plan).len(), 2);
        assert_eq!(plan.sidecars[0].format, SidecarFormat::Srt);
        assert_eq!(plan.sidecars[1].format, SidecarFormat::Original);
        assert!(names(&plan)[1].ends_with(".eng.ass"));
    }

    #[test]
    fn a_subrip_track_proposes_nothing_but_its_own_conversion() {
        let plan = plan(&[stream("subrip", Some("eng"), false)], &output());

        assert_eq!(plan.sidecars.len(), 1);
    }

    #[test]
    fn styling_is_read_from_the_events_rather_than_assumed_from_the_codec() {
        let plain = "[Events]\nDialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,Hello there.\n";
        let typeset =
            "[Events]\nDialogue: 0,0:00:01.00,0:00:03.00,Sign,,0,0,0,,{\\pos(320,100)}SHOP\n";

        assert!(!carries_styling(plain));
        assert!(carries_styling(typeset));
    }

    /// A brace in the dialogue itself is not an override block.
    #[test]
    fn a_stray_brace_in_the_text_is_not_styling() {
        let text = "[Events]\nDialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,use {x} here\n";

        assert!(!carries_styling(text));
    }

    #[test]
    fn a_header_that_mentions_styles_is_not_an_event_that_uses_them() {
        let header = "[V4+ Styles]\nStyle: Sign,Arial,48,&H00FFFFFF,{\\an8}\n[Events]\nDialogue: 0,0:00:01.00,0:00:03.00,Default,,0,0,0,,Plain line\n";

        assert!(!carries_styling(header));
    }

    #[test]
    fn the_arguments_name_the_stream_and_the_conversion() {
        let plan = plan(&[stream("ass", Some("eng"), false)], &output());
        let arguments = plan.sidecars[0]
            .ffmpeg_args(Path::new("/media/in.mkv"))
            .join(" ");

        assert!(arguments.contains("-map 0:s:0"), "{arguments}");
        assert!(arguments.contains("-c:s srt"), "{arguments}");

        let original = plan.sidecars[1]
            .ffmpeg_args(Path::new("/media/in.mkv"))
            .join(" ");
        assert!(original.contains("-c:s copy"), "{original}");
    }

    /// The index is the position among the source's subtitle streams, not the
    /// order this plan reports them in.
    #[test]
    fn the_stream_index_follows_the_source_not_the_report() {
        let plan = plan(
            &[
                stream("subrip", Some("spa"), false),
                stream("subrip", Some("eng"), false),
            ],
            &output(),
        );

        assert_eq!(plan.sidecars[0].language.as_deref(), Some("eng"));
        assert_eq!(plan.sidecars[0].stream_index, 1);
    }
}
