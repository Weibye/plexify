//! Which of a source's subtitle streams an output carries, and the text tracks
//! written beside it as sidecars.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use super::FFmpegProcessor;
use crate::subtitles::{self, Sidecar, SidecarFormat};

/// Subtitle codecs that carry a picture rather than text.
///
/// MP4 carries subtitles as `mov_text`, which is a text format, and FFmpeg
/// cannot encode a picture into one - "subtitle encoding currently only
/// possible from text to text or bitmap to bitmap". Mapping a stream in one of
/// these codecs does not lose that stream, it fails the whole job, taking the
/// video and every other track with it.
///
/// The list names what is provably impossible, not what is known to work. A
/// subtitle codec nobody here has heard of is still mapped and left for FFmpeg
/// to judge: the worst that costs is the failure this list exists to avoid,
/// where dropping an unrecognised codec would silently discard a text track
/// that would have converted perfectly well.
///
/// `dvb_teletext` is on the list for a different reason from the other four,
/// and it is the reason enumerating the obviously-bitmap formats misses it:
/// teletext is not inherently a picture, but the only decoder for it
/// (`libzvbi_teletextdec`) emits one unless `-txt_format` says otherwise, and
/// its default is `bitmap`. Nothing here sets that option, so a teletext stream
/// reaching `-c:s mov_text` fails at encoder open with nothing written, exactly
/// like a PGS one. Setting `-txt_format text` would keep the track instead, but
/// it is a private option of a decoder that a default FFmpeg build does not
/// compile in, and choosing to reconfigure a decoder is a wider decision than
/// this list makes.
const BITMAP_SUBTITLE_CODECS: [&str; 5] = [
    "dvb_subtitle",
    "dvb_teletext",
    "dvd_subtitle",
    "hdmv_pgs_subtitle",
    "xsub",
];

/// Whether a codec is pictures of text rather than text.
///
/// Exposed because extraction has to answer the same question: an image track
/// cannot become a sidecar, and turning one into text would be OCR.
pub fn is_bitmap_subtitle(codec: &str) -> bool {
    BITMAP_SUBTITLE_CODECS.contains(&codec)
}

/// One subtitle stream, as FFprobe describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleStream {
    /// The codec name FFprobe reports, e.g. `subrip` or `hdmv_pgs_subtitle`.
    pub codec: String,
    /// The stream's language tag, where it carries one.
    pub language: Option<String>,
    /// Whether the stream carries the `forced` disposition.
    ///
    /// A forced track holds what a scene cannot be understood without: the
    /// translated signs and the dialogue in another language, rather than a
    /// full transcript a viewer chose to turn on. Losing one is the difference
    /// between a file that plays without subtitles and a file whose
    /// foreign-language scenes are untranslated, so the two are worth telling
    /// apart even where nothing yet acts on the difference.
    pub forced: bool,
}

/// Which of a source's subtitle streams the output can carry.
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleSelection {
    /// The positions, among the source's subtitle streams, of the ones that
    /// can become `mov_text`.
    pub(super) kept: Vec<usize>,
    /// The ones that cannot, so the run can say what it left out.
    pub(super) dropped: Vec<SubtitleStream>,
}

impl SubtitleSelection {
    /// Mappings for the subtitle streams to carry, reading them from the given
    /// input.
    ///
    /// The source is input 0 for a one-pass encode and input 1 for the join
    /// that follows a chunked one, where input 0 is the list of chunks.
    pub fn mappings(&self, input_index: usize) -> Vec<String> {
        self.kept
            .iter()
            .map(|position| format!("{input_index}:s:{position}"))
            .collect()
    }

    /// The subtitle streams that cannot go into the output.
    pub fn dropped(&self) -> &[SubtitleStream] {
        &self.dropped
    }
}

/// Sort a source's subtitle streams into the ones MP4 can carry and the ones it
/// cannot.
pub fn select_subtitle_streams(streams: &[SubtitleStream]) -> SubtitleSelection {
    let mut kept = Vec::new();
    let mut dropped = Vec::new();

    for (position, stream) in streams.iter().enumerate() {
        if BITMAP_SUBTITLE_CODECS.contains(&stream.codec.as_str()) {
            dropped.push(stream.clone());
        } else {
            kept.push(position);
        }
    }

    SubtitleSelection { kept, dropped }
}

/// One line of the subtitle probe's CSV output.
///
/// The shape is `codec,forced` or `codec,forced,language` - FFprobe writes the
/// disposition flag as `0` or `1` and simply omits the tag where a stream
/// carries no language.
///
/// The language is taken as everything after the second comma rather than as a
/// third field, and that is the part worth keeping right: FFprobe CSV-quotes a
/// tag that itself contains a comma (`subrip,0,"en,US"`), so splitting on every
/// comma would read the tail of that tag as a field of its own. Nothing after
/// the language needs parsing, so stopping the split at three is enough to make
/// the codec and the flag immovable no matter what the tag holds.
fn parse_probed_subtitle_line(line: &str) -> SubtitleStream {
    let mut fields = line.splitn(3, ',');
    let codec = fields.next().unwrap_or_default().trim();
    let forced = fields.next().unwrap_or_default().trim() == "1";
    let language = fields
        .next()
        .map(|language| language.trim().to_string())
        .filter(|language| !language.is_empty());

    SubtitleStream {
        codec: codec.to_string(),
        language,
        forced,
    }
}

/// What a run says about a subtitle stream it could not carry.
///
/// A forced stream gets a different message rather than a louder one, because
/// what was lost is different in kind: a decorative track is a transcript a
/// viewer would have chosen to turn on, while a forced track is what makes a
/// foreign-language scene followable at all. The file still transcodes either
/// way - naming the loss is the whole of what this does - but a person reading
/// a log needs to be able to pick out the files that now play a scene
/// untranslated from the ones that merely lost a track nobody asked for.
fn dropped_stream_warning(stream: &SubtitleStream, input_path: &Path) -> String {
    let language = stream
        .language
        .as_deref()
        .map(|language| format!(" ({language})"))
        .unwrap_or_default();

    let cannot_be_converted =
        "it holds pictures, and MP4 carries subtitles as text, so it cannot be converted. \
         The source file is not deleted, so the track can still be taken from it.";

    if stream.forced {
        format!(
            "⚠️ FORCED SUBTITLES LOST: leaving a forced {}{} subtitle stream out of {:?}: {} \
             A forced track carries the translated signs and foreign-language dialogue a scene \
             cannot be followed without, so those scenes will now play untranslated.",
            stream.codec, language, input_path, cannot_be_converted
        )
    } else {
        format!(
            "⚠️ Leaving a {}{} subtitle stream out of {:?}: {}",
            stream.codec, language, input_path, cannot_be_converted
        )
    }
}

/// Where a job's subtitles come from.
pub(super) enum SubtitleSource {
    /// A WebM's `.vtt` sidecar, which is declared as its own input.
    External(PathBuf),
    /// The streams the source itself carries, and which of them the output can
    /// hold.
    Embedded(SubtitleSelection),
}

impl FFmpegProcessor {
    /// Write the source's text subtitle tracks beside the output.
    ///
    /// They are written next to the output wherever that is - the work folder
    /// for a job that has one - and travel with it, for the same reason the
    /// video does: a media server that indexes a half-written subtitle shows a
    /// broken track.
    pub(super) async fn extract_sidecars(
        &self,
        input_path: &Path,
        output_path: &Path,
    ) -> Result<()> {
        let streams = self.probe_subtitle_streams(input_path).await?;
        let plan = subtitles::plan(&streams, output_path);

        for stream in &plan.unconvertible {
            warn!(
                "{input_path:?} carries a {} subtitle track{}; it is pictures of text and cannot become a sidecar, so it is left behind rather than guessed at.",
                stream.codec,
                stream
                    .language
                    .as_deref()
                    .map(|language| format!(" in {language}"))
                    .unwrap_or_default()
            );
        }

        for sidecar in &plan.sidecars {
            self.run(sidecar.ffmpeg_args(input_path), "subtitle sidecar")
                .await?;

            if sidecar.format == SidecarFormat::Original {
                self.keep_only_if_styled(sidecar).await;
            }
        }

        Ok(())
    }

    /// Delete a preserved original whose events turn out not to need it.
    ///
    /// The plan proposes one for every ASS track, because only the events can
    /// say whether the conversion to SRT loses anything. Where it loses
    /// nothing, a second file offering the viewer the same words twice is
    /// clutter.
    async fn keep_only_if_styled(&self, sidecar: &Sidecar) {
        let styled = match tokio::fs::read_to_string(&sidecar.path).await {
            Ok(text) => subtitles::carries_styling(&text),
            // Unreadable is not the same as unstyled, and this is the branch
            // that deletes: keep the file and let a person look at it.
            Err(e) => {
                warn!(
                    "Could not read {:?} to see whether it is styled, so it is kept: {e}",
                    sidecar.path
                );
                return;
            }
        };

        if styled {
            info!(
                "Kept {:?} beside the SRT: its events are positioned or styled, which SRT cannot hold.",
                sidecar.path
            );
        } else if let Err(e) = tokio::fs::remove_file(&sidecar.path).await {
            warn!("Could not remove the unstyled {:?}: {e}", sidecar.path);
        }
    }

    /// Work out which of a source's subtitle streams can go into the output,
    /// and say what is being left behind.
    pub(super) async fn select_subtitles(&self, input_path: &Path) -> Result<SubtitleSelection> {
        let selection = select_subtitle_streams(&self.probe_subtitle_streams(input_path).await?);

        for stream in selection.dropped() {
            warn!("{}", dropped_stream_warning(stream, input_path));
        }

        Ok(selection)
    }

    /// The subtitle streams a source carries, in the order it lists them.
    ///
    /// That order is what makes the result usable as a mapping: the nth line
    /// here is the stream FFmpeg calls `0:s:n`. A source with no subtitles
    /// probes successfully and comes back empty, which is a different answer
    /// from an error - that means FFprobe could not read the file at all, and
    /// a job that goes on without knowing what its tracks are can deliver an
    /// output without them while `work` disables the source that had them.
    async fn probe_subtitle_streams(&self, input_path: &Path) -> Result<Vec<SubtitleStream>> {
        let output = self
            .probe(
                &[
                    "-v",
                    "error",
                    "-select_streams",
                    "s",
                    "-show_entries",
                    "stream=codec_name:stream_disposition=forced:stream_tags=language",
                    "-of",
                    "csv=p=0",
                ],
                input_path,
            )
            .await
            .map_err(|e| {
                anyhow!(
                    "Failed to start FFprobe to read the subtitle streams of {input_path:?}: {e}"
                )
            })?;

        if !output.status.success() {
            return Err(anyhow!(
                "FFprobe could not read the subtitle streams of {input_path:?}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(parse_probed_subtitle_line)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::test_support::*;
    use crate::ffmpeg::Chunking;
    use crate::job::{
        AudioAction, Job, MediaFileType, Operation, PostProcessingSettings, QualitySettings,
        SubtitleAction,
    };
    use std::process::Stdio;
    use tempfile::TempDir;

    #[tokio::test]
    async fn a_long_source_converts_its_subtitles_the_same_way_a_short_one_does() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("film.mkv");

        // An event that outlives the one after it, which is the case the two
        // paths are most likely to disagree on: a `mov_text` track cannot hold
        // an overlap, so something has to give way, and both have to give way
        // in the same place or a file over the chunking threshold converts
        // differently from one under it.
        let subtitles = input.with_extension("srt");
        std::fs::write(
            &subtitles,
            "1\n00:00:00,000 --> 00:00:09,000\noverlapping\n\n\
             2\n00:00:01,000 --> 00:00:02,000\nnext\n\n\
             3\n00:00:03,000 --> 00:00:04,000\nlater\n",
        )
        .unwrap();

        let built = std::process::Command::new("ffmpeg")
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=6:size=160x120:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=6",
            ])
            .arg("-i")
            .arg(&subtitles)
            .args([
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "2:s",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-c:s",
                "srt",
                "-y",
            ])
            .arg(&input)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(built.success());

        let job = chunking_job(&input);

        let one_pass_dir = temp.path().join("one-pass");
        std::fs::create_dir_all(&one_pass_dir).unwrap();
        FFmpegProcessor::new(false)
            .with_chunking(Chunking {
                chunk_seconds: 2.0,
                // Above the source length, so this one is encoded in one pass.
                min_source_seconds: 60.0,
            })
            .process_job(&job, None, &one_pass_dir)
            .await
            .unwrap();

        let chunked_dir = temp.path().join("chunked");
        std::fs::create_dir_all(&chunked_dir).unwrap();
        FFmpegProcessor::new(false)
            .with_chunking(TEST_CHUNKING)
            .process_job(&job, None, &chunked_dir)
            .await
            .unwrap();

        let one_pass = subtitle_timings(&job.work_folder_output_path(&one_pass_dir));
        let chunked = subtitle_timings(&job.work_folder_output_path(&chunked_dir));

        assert!(!one_pass.is_empty(), "the fixture should carry subtitles");
        assert_eq!(
            subtitle_events(&one_pass),
            subtitle_events(&chunked),
            "chunking a source must not change how its subtitles are converted"
        );

        // And what both produce is non-overlapping, so the agreement is on a
        // track a player can actually show rather than on the same mistake.
        for pair in chunked.windows(2) {
            let ((start, duration), (next_start, _)) = (pair[0], pair[1]);
            assert!(
                start + duration <= next_start + f64::EPSILON,
                "subtitle at {start} runs {duration}s into the one at {next_start}"
            );
        }
    }

    /// A source whose subtitle track holds an overlap and, after it, more
    /// events - including one that nothing follows.
    fn build_overlapping_subtitle_source(input: &Path, seconds: u32) {
        let subtitles = input.with_extension("srt");
        std::fs::write(
            &subtitles,
            "1\n00:00:00,000 --> 00:00:09,000\noverlapping\n\n\
             2\n00:00:01,000 --> 00:00:02,000\nnext\n\n\
             3\n00:00:03,000 --> 00:00:04,000\nlater\n",
        )
        .unwrap();

        let built = std::process::Command::new("ffmpeg")
            .args([
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=duration={seconds}:size=160x120:rate=10"),
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:duration={seconds}"),
            ])
            .arg("-i")
            .arg(&subtitles)
            .args([
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "2:s",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-c:s",
                "srt",
                "-y",
            ])
            .arg(input)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(built.success(), "could not build the test source");
    }

    /// Every event the source declares has to reach the output.
    ///
    /// An overlap has to be resolved somehow - `mov_text` cannot hold two
    /// events at once - but resolving it is trimming one of them, never
    /// throwing the rest of the track away. The source is renamed `.disabled`
    /// after a job succeeds, so a subtitle lost here is only visible by
    /// watching the file.
    #[tokio::test]
    async fn no_subtitle_event_is_lost_by_either_encode_path() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("film.mkv");
        build_overlapping_subtitle_source(&input, 6);

        let job = chunking_job(&input);
        let mut by_path = Vec::new();

        for (name, chunking) in [
            (
                "one-pass",
                Chunking {
                    chunk_seconds: 2.0,
                    // Above the source length, so this one is encoded whole.
                    min_source_seconds: 60.0,
                },
            ),
            ("chunked", TEST_CHUNKING),
        ] {
            let work_folder = temp.path().join(name);
            std::fs::create_dir_all(&work_folder).unwrap();

            FFmpegProcessor::new(false)
                .with_chunking(chunking)
                .process_job(&job, None, &work_folder)
                .await
                .unwrap();

            let texts = subtitle_texts(&job.work_folder_output_path(&work_folder));
            for event in ["overlapping", "next", "later"] {
                assert!(
                    texts.iter().any(|text| text == event),
                    "the {name} encode lost the subtitle event {event:?}: {texts:?}"
                );
            }

            by_path.push(texts);
        }

        // Naming the three events one by one says each of them arrived, but
        // `any` is blind to order and to duplication, so on its own it would
        // let one path reorder or repeat an event the other did not. The
        // companion test compares the two paths' event *timings*; comparing
        // the text as a whole sequence is the other half of the same property,
        // and together they say the two paths produce one subtitle track.
        assert_eq!(
            by_path[0], by_path[1],
            "the two encode paths produced different subtitle text"
        );
    }

    /// Write a short PGS subtitle stream - the bitmap subtitle format a
    /// Blu-ray rip carries, and one MP4 has no way to hold.
    ///
    /// PGS is a run of segments, each `PG`, a 90kHz presentation timestamp, a
    /// decode timestamp, a type byte and a length. One display set puts an
    /// object on screen and a later one clears it.
    fn write_pgs_sup(path: &Path) {
        fn segment(out: &mut Vec<u8>, seconds: f64, kind: u8, payload: &[u8]) {
            out.extend_from_slice(b"PG");
            out.extend_from_slice(&((seconds * 90_000.0) as u32).to_be_bytes());
            out.extend_from_slice(&0u32.to_be_bytes());
            out.push(kind);
            out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            out.extend_from_slice(payload);
        }

        let mut sup = Vec::new();

        // Presentation composition: one object, at the start of an epoch.
        let mut presentation = vec![0, 160, 0, 120, 0x10, 0, 0, 0x80, 0, 0, 1];
        presentation.extend_from_slice(&[0, 0, 0, 0, 0, 10, 0, 10]);
        segment(&mut sup, 1.0, 0x16, &presentation);
        // One window, the size of the object.
        segment(&mut sup, 1.0, 0x17, &[1, 0, 0, 10, 0, 10, 0, 8, 0, 2]);
        // A palette: entry 0 transparent, entry 1 opaque white.
        segment(
            &mut sup,
            1.0,
            0x14,
            &[0, 0, 0, 16, 128, 128, 0, 1, 235, 128, 128, 255],
        );
        // An 8x2 block of colour 1. PGS runs are `00`, a length with its two
        // top bits saying how it is encoded, then the colour; `00 00` ends a
        // line. The declared length covers the two size fields as well.
        let rle: Vec<u8> = [0x00, 0x88, 0x01, 0x00, 0x00].repeat(2);
        let mut object = vec![0, 0, 0, 0xC0];
        object.extend_from_slice(&((rle.len() + 4) as u32).to_be_bytes()[1..]);
        object.extend_from_slice(&[0, 8, 0, 2]);
        object.extend_from_slice(&rle);
        segment(&mut sup, 1.0, 0x15, &object);
        segment(&mut sup, 1.0, 0x80, &[]);

        // And a display set that takes it away again.
        segment(
            &mut sup,
            2.0,
            0x16,
            &[0, 160, 0, 120, 0x10, 0, 1, 0x00, 0, 0, 0],
        );
        segment(&mut sup, 2.0, 0x80, &[]);

        std::fs::write(path, sup).unwrap();
    }

    /// A source carrying a text subtitle track and a bitmap one.
    ///
    /// `forced_bitmap` puts the `forced` disposition on the bitmap track, which
    /// is the realistic shape of the case worth telling apart: a Blu-ray rip
    /// whose PGS track holds only the translated signs and the foreign-language
    /// dialogue, alongside a full text track a viewer can choose.
    fn build_mixed_subtitle_source(input: &Path, seconds: u32, forced_bitmap: bool) {
        let text = input.with_extension("srt");
        std::fs::write(
            &text,
            "1\n00:00:00,500 --> 00:00:01,500\nspoken line\n\n\
             2\n00:00:02,000 --> 00:00:03,000\nanother line\n",
        )
        .unwrap();

        let bitmap = input.with_extension("sup");
        write_pgs_sup(&bitmap);

        let built = std::process::Command::new("ffmpeg")
            .args([
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=duration={seconds}:size=160x120:rate=10"),
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=440:duration={seconds}"),
            ])
            .arg("-i")
            .arg(&text)
            .arg("-i")
            .arg(&bitmap)
            .args([
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "2:s",
                "-map",
                "3:s",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-c:s:0",
                "srt",
                // The bitmap track goes in untouched; there is nothing to
                // encode it from.
                "-c:s:1",
                "copy",
                "-metadata:s:s:1",
                "language=eng",
                "-disposition:s:1",
                if forced_bitmap { "forced" } else { "0" },
                "-y",
            ])
            .arg(input)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(built.success(), "could not build the test source");

        assert_eq!(
            stream_codecs(input),
            vec!["h264", "aac", "subrip", "hdmv_pgs_subtitle"],
            "the fixture must carry a bitmap subtitle track for this to test anything"
        );
    }

    /// A bitmap subtitle stream costs its own track and nothing else.
    ///
    /// `mov_text` is a text format and FFmpeg will not encode a picture into
    /// one, so mapping the stream does not lose the stream, it fails the job -
    /// the video, the audio and the text subtitles with it. Leaving it out is
    /// the only way the rest of the file gets through, and it is recoverable:
    /// the source is renamed, never deleted.
    #[tokio::test]
    async fn a_bitmap_subtitle_stream_is_left_out_rather_than_failing_the_job() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("film.mkv");
        build_mixed_subtitle_source(&input, 6, false);

        let job = chunking_job(&input);

        for (name, chunking) in [
            (
                "one-pass",
                Chunking {
                    chunk_seconds: 2.0,
                    min_source_seconds: 60.0,
                },
            ),
            ("chunked", TEST_CHUNKING),
        ] {
            let work_folder = temp.path().join(name);
            std::fs::create_dir_all(&work_folder).unwrap();

            FFmpegProcessor::new(false)
                .with_chunking(chunking)
                .process_job(&job, None, &work_folder)
                .await
                .unwrap_or_else(|error| panic!("the {name} encode failed: {error:#}"));

            let output = job.work_folder_output_path(&work_folder);
            assert_eq!(
                stream_codecs(&output),
                vec!["h264", "aac", "mov_text"],
                "the {name} encode should carry everything but the bitmap track"
            );
            assert_eq!(
                subtitle_texts(&output),
                vec!["spoken line", "another line"],
                "the {name} encode should keep the text subtitles it can convert"
            );
        }
    }

    /// A source whose only subtitles are bitmaps still transcodes, by either
    /// encode path.
    ///
    /// The chunked path is the one worth stating: with every subtitle stream
    /// dropped, the join still declares the source as input 1 and then maps
    /// nothing at all from it. An input nobody reads is a shape FFmpeg has to
    /// tolerate rather than one it is asked for anywhere else here, so the
    /// join is the half of this that could break on its own.
    #[tokio::test]
    async fn a_source_whose_only_subtitles_are_bitmaps_still_transcodes() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("film.mkv");
        let bitmap = input.with_extension("sup");
        write_pgs_sup(&bitmap);

        let built = std::process::Command::new("ffmpeg")
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=4:size=160x120:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=4",
            ])
            .arg("-i")
            .arg(&bitmap)
            .args([
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "2:s",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-c:s",
                "copy",
                "-y",
            ])
            .arg(&input)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(built.success(), "could not build the test source");

        let job = chunking_job(&input);

        for (name, chunking) in [
            (
                "one-pass",
                Chunking {
                    chunk_seconds: 2.0,
                    min_source_seconds: 60.0,
                },
            ),
            ("chunked", TEST_CHUNKING),
        ] {
            let work_folder = temp.path().join(name);
            std::fs::create_dir_all(&work_folder).unwrap();

            FFmpegProcessor::new(false)
                .with_chunking(chunking)
                .process_job(&job, None, &work_folder)
                .await
                .unwrap_or_else(|error| panic!("the {name} encode failed: {error:#}"));

            assert_eq!(
                stream_codecs(&job.work_folder_output_path(&work_folder)),
                vec!["h264", "aac"],
                "with no subtitle stream left to carry, the {name} encode should still put the picture and sound through"
            );
        }
    }

    #[tokio::test]
    async fn probing_reads_the_subtitle_streams_in_the_order_they_are_mapped_by() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("film.mkv");
        build_mixed_subtitle_source(&input, 4, false);

        let streams = FFmpegProcessor::new(false)
            .probe_subtitle_streams(&input)
            .await
            .expect("ffprobe should read the source");

        assert_eq!(
            streams,
            vec![
                SubtitleStream {
                    codec: "subrip".to_string(),
                    language: None,
                    forced: false,
                },
                SubtitleStream {
                    codec: "hdmv_pgs_subtitle".to_string(),
                    // Named in the warning, so it says which track was left.
                    language: Some("eng".to_string()),
                    forced: false,
                },
            ]
        );

        // A stream's position in that list is what `-map 0:s:n` refers to.
        assert_eq!(
            select_subtitle_streams(&streams).mappings(0),
            vec!["0:s:0"],
            "only the text track can be carried, and it is the source's first subtitle stream"
        );
    }

    /// A dropped bitmap track that a scene actually needs is reported as such.
    ///
    /// A forced track holds the translated signs and the foreign-language
    /// dialogue, so losing one costs something a decorative track does not.
    /// Nothing about the transcode changes - the track is dropped either way
    /// and the job still succeeds - but the run has to be able to say which of
    /// the two happened, because a person reading a log otherwise cannot tell a
    /// file that now plays a scene untranslated from one that lost a transcript
    /// nobody asked for.
    #[tokio::test]
    async fn a_dropped_bitmap_track_that_is_forced_is_reported_as_forced() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("film.mkv");
        build_mixed_subtitle_source(&input, 4, true);

        let processor = FFmpegProcessor::new(false);
        let streams = processor
            .probe_subtitle_streams(&input)
            .await
            .expect("ffprobe should read the source");

        // The disposition comes out of the probe the encode already makes, so
        // the two kinds of track are distinguishable without a second process.
        assert_eq!(
            streams,
            vec![
                SubtitleStream {
                    codec: "subrip".to_string(),
                    language: None,
                    forced: false,
                },
                SubtitleStream {
                    codec: "hdmv_pgs_subtitle".to_string(),
                    language: Some("eng".to_string()),
                    forced: true,
                },
            ]
        );

        let selection = select_subtitle_streams(&streams);

        // What happens to the track is exactly what happened before: it is
        // dropped, and the text track is still carried. Only the report differs.
        assert_eq!(selection.mappings(0), vec!["0:s:0"]);
        let dropped = selection.dropped();
        assert_eq!(dropped.len(), 1);
        assert!(dropped[0].forced);

        let warning = dropped_stream_warning(&dropped[0], &input);
        assert!(
            warning.contains("FORCED SUBTITLES LOST"),
            "a forced track has to be distinguishable at a glance: {warning}"
        );
        assert!(
            warning.contains("play untranslated"),
            "the warning has to say what it costs, not just that it happened: {warning}"
        );

        // And the job still succeeds. The detection is a report, not a policy.
        let work_folder = temp.path().join("work");
        std::fs::create_dir_all(&work_folder).unwrap();
        let job = chunking_job(&input);
        FFmpegProcessor::new(false)
            .with_chunking(Chunking {
                chunk_seconds: 2.0,
                min_source_seconds: 60.0,
            })
            .process_job(&job, None, &work_folder)
            .await
            .expect("a forced bitmap track is still only a dropped track");

        assert_eq!(
            stream_codecs(&job.work_folder_output_path(&work_folder)),
            vec!["h264", "aac", "mov_text"]
        );
    }

    #[test]
    fn the_probe_line_keeps_the_codec_and_the_forced_flag_whatever_the_tag_holds() {
        let parse = parse_probed_subtitle_line;

        // No language tag: the line stops after the disposition flag.
        assert_eq!(
            parse("subrip,0"),
            SubtitleStream {
                codec: "subrip".to_string(),
                language: None,
                forced: false,
            }
        );

        assert_eq!(
            parse("hdmv_pgs_subtitle,1,eng"),
            SubtitleStream {
                codec: "hdmv_pgs_subtitle".to_string(),
                language: Some("eng".to_string()),
                forced: true,
            }
        );

        // A tag holding a comma is CSV-quoted by FFprobe. The quotes are
        // cosmetic in a warning, but the split must not treat the tail of the
        // tag as a field - the codec and the flag stay where they are.
        let quoted = parse(r#"subrip,1,"en,US""#);
        assert_eq!(quoted.codec, "subrip");
        assert!(quoted.forced);
        assert_eq!(quoted.language.as_deref(), Some(r#""en,US""#));
    }

    #[test]
    fn a_decorative_dropped_track_and_a_forced_one_do_not_read_alike() {
        let path = Path::new("/media/film.mkv");

        let decorative = dropped_stream_warning(
            &SubtitleStream {
                codec: "hdmv_pgs_subtitle".to_string(),
                language: Some("eng".to_string()),
                forced: false,
            },
            path,
        );
        let forced = dropped_stream_warning(
            &SubtitleStream {
                codec: "hdmv_pgs_subtitle".to_string(),
                language: Some("eng".to_string()),
                forced: true,
            },
            path,
        );

        // Both name the codec, the language and the file, because both are a
        // track that went missing from a specific file.
        for warning in [&decorative, &forced] {
            assert!(warning.contains("hdmv_pgs_subtitle"), "{warning}");
            assert!(warning.contains("(eng)"), "{warning}");
            assert!(warning.contains("film.mkv"), "{warning}");
            assert!(
                warning.contains("The source file is not deleted"),
                "the way back to the track is the same in both cases: {warning}"
            );
        }

        // Only the forced one is escalated, and it says what was lost rather
        // than only that something was.
        assert!(!decorative.contains("FORCED"), "{decorative}");
        assert!(forced.contains("FORCED SUBTITLES LOST"), "{forced}");
        assert!(forced.contains("play untranslated"), "{forced}");
    }

    #[test]
    fn only_the_subtitle_codecs_that_cannot_be_converted_are_left_out() {
        let streams = |codecs: &[&str]| {
            codecs
                .iter()
                .map(|codec| SubtitleStream {
                    codec: codec.to_string(),
                    language: None,
                    forced: false,
                })
                .collect::<Vec<_>>()
        };

        // Every bitmap format goes, and the text around it stays - identified
        // by position, because that is what FFmpeg's `0:s:n` counts.
        //
        // `dvb_teletext` is in here because it is the one entry whose output
        // format is a decoder option rather than a codec property: the only
        // decoder for it emits a bitmap unless `-txt_format` says otherwise,
        // and nothing here sets that option.
        let mixed = streams(&[
            "hdmv_pgs_subtitle",
            "subrip",
            "dvd_subtitle",
            "ass",
            "dvb_subtitle",
            "xsub",
            "mov_text",
            "dvb_teletext",
            "webvtt",
        ]);
        let selection = select_subtitle_streams(&mixed);
        assert_eq!(
            selection.mappings(0),
            vec!["0:s:1", "0:s:3", "0:s:6", "0:s:8"]
        );
        assert_eq!(
            selection
                .dropped()
                .iter()
                .map(|stream| stream.codec.as_str())
                .collect::<Vec<_>>(),
            vec![
                "hdmv_pgs_subtitle",
                "dvd_subtitle",
                "dvb_subtitle",
                "xsub",
                "dvb_teletext"
            ]
        );

        // The formats that decode to text stay on, and they are the reason the
        // list cannot simply be "anything a broadcast carries": `arib_caption`
        // and `eia_608` come off the same kind of capture as teletext and both
        // convert perfectly well.
        let broadcast = streams(&["arib_caption", "eia_608"]);
        assert_eq!(
            select_subtitle_streams(&broadcast).mappings(0),
            vec!["0:s:0", "0:s:1"]
        );
        assert!(select_subtitle_streams(&broadcast).dropped().is_empty());

        // A codec nobody here has heard of is carried, not discarded. Being
        // wrong that way costs a job that fails and can be run again; being
        // wrong the other way loses a track from a library.
        let unknown = streams(&["something_new"]);
        assert_eq!(select_subtitle_streams(&unknown).mappings(0), vec!["0:s:0"]);
        assert!(select_subtitle_streams(&unknown).dropped().is_empty());

        // A source with no subtitles maps none, rather than an optional group
        // that would pick up whatever it found.
        assert!(select_subtitle_streams(&[]).mappings(0).is_empty());
    }

    /// A source carrying two text subtitle tracks, one of them styled ASS.
    fn build_subtitled_source(path: &Path, directory: &Path) -> PathBuf {
        let english = directory.join("english.srt");
        std::fs::write(
            &english,
            "1\n00:00:00,100 --> 00:00:01,500\nHello there.\n\n",
        )
        .unwrap();

        let signs = directory.join("signs.ass");
        std::fs::write(
            &signs,
            "[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize\nStyle: Default,Arial,20\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:00.10,0:00:01.50,Default,,0,0,0,,{\\pos(320,100)}SHOP\n",
        )
        .unwrap();

        let built = std::process::Command::new("ffmpeg")
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=160x120:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=2",
            ])
            .arg("-i")
            .arg(&english)
            .arg("-i")
            .arg(&signs)
            .args([
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "2:s",
                "-map",
                "3:s",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
                "-c:s",
                "copy",
                "-metadata:s:s:0",
                "language=eng",
                "-metadata:s:s:1",
                "language=jpn",
                "-y",
            ])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(built.success(), "could not build the subtitled source");

        path.to_path_buf()
    }

    fn sidecar_names(directory: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(directory)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| !name.ends_with(".mkv") && !name.ends_with(".mp4"))
            .collect();
        names.sort();
        names
    }

    /// Convert `job` in a work folder under `temp`, then move it and its
    /// sidecars beside the source the way `work` does.
    fn deliver(job: &Job, temp: &Path) {
        let work_folder = temp.join("work");
        std::fs::create_dir_all(&work_folder).unwrap();
        let processor = FFmpegProcessor::new(false);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime
            .block_on(processor.process_job(job, None, &work_folder))
            .expect("the remux should succeed");
        runtime
            .block_on(processor.move_to_destination(job, None, &work_folder))
            .expect("the output should move beside its source");
    }

    /// The tracks leave the container so nothing is burned in, and they land
    /// beside it rather than being lost.
    #[test]
    fn extracting_subtitles_empties_the_container_and_writes_them_beside_it() {
        if !ffmpeg_present() {
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let source = temp_dir.path().join("sources");
        std::fs::create_dir_all(&source).unwrap();
        let input = build_subtitled_source(&source.join("show.mkv"), &source);

        let job = Job::new(
            input.clone(),
            MediaFileType::Mkv,
            Operation::Remux {
                audio: AudioAction::Copy,
                subtitles: SubtitleAction::Extract,
            },
            QualitySettings::default(),
            PostProcessingSettings::default(),
            temp_dir.path(),
        );

        deliver(&job, temp_dir.path());

        // Nothing left in the container to burn in.
        assert!(
            !streams_of(&job.output_path)
                .iter()
                .any(|stream| stream.starts_with("subtitle")),
            "the output still carries a subtitle stream: {:?}",
            streams_of(&job.output_path)
        );

        let names = sidecar_names(&source);
        assert!(
            names.contains(&"show.eng.srt".to_string()),
            "the English track should be beside the file: {names:?}"
        );
        // Extracted even though it is not English: `work` disables the source,
        // so a track left behind is lost.
        assert!(
            names.contains(&"show.jpn.srt".to_string()),
            "the Japanese track should survive too: {names:?}"
        );
        // The styled ASS is kept as well, because SRT cannot hold its position.
        assert!(
            names.contains(&"show.jpn.ass".to_string()),
            "a styled track keeps its original: {names:?}"
        );

        let srt = std::fs::read_to_string(source.join("show.eng.srt")).unwrap();
        assert!(srt.contains("Hello there."), "{srt}");
    }

    /// An MP4 this project produced itself carries `mov_text`. There is no
    /// muxer to write that codec's original to, so only the SRT is proposed;
    /// asking for the original would fail the job after the media file was
    /// written, with an error naming no subtitle.
    #[test]
    fn a_mov_text_source_extracts_instead_of_failing_the_job() {
        if !ffmpeg_present() {
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let source = temp_dir.path().join("sources");
        std::fs::create_dir_all(&source).unwrap();

        let english = source.join("english.srt");
        std::fs::write(
            &english,
            "1\n00:00:00,100 --> 00:00:01,500\nAlready ours.\n\n",
        )
        .unwrap();

        // mov_text is an MP4 codec; Matroska will not carry it. The output
        // therefore lands on the input's own path, so this runs through a work
        // folder - which is the realistic route anyway.
        let input = source.join("past-output.mp4");
        let built = std::process::Command::new("ffmpeg")
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=160x120:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=2",
            ])
            .arg("-i")
            .arg(&english)
            .args([
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "2:s",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
                "-c:s",
                "mov_text",
                "-metadata:s:s:0",
                "language=eng",
                "-y",
            ])
            .arg(&input)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(built.success(), "could not build the mov_text source");
        assert!(
            stream_codecs(&input)
                .iter()
                .any(|codec| codec == "mov_text"),
            "the fixture must actually carry mov_text: {:?}",
            stream_codecs(&input)
        );

        let job = Job::new(
            input,
            MediaFileType::Mkv,
            Operation::Remux {
                audio: AudioAction::Copy,
                subtitles: SubtitleAction::Extract,
            },
            QualitySettings::default(),
            PostProcessingSettings::default(),
            temp_dir.path(),
        );

        let work_folder = temp_dir.path().join("work");
        std::fs::create_dir_all(&work_folder).unwrap();

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(FFmpegProcessor::new(false).process_job(&job, None, &work_folder))
            .expect("a mov_text source must not fail the job");

        let names = sidecar_names(&work_folder);
        assert!(
            names.iter().any(|name| name.ends_with(".eng.srt")),
            "the track should have been extracted: {names:?}"
        );
        assert!(
            !names.iter().any(|name| name.ends_with(".ttxt")),
            "nothing should propose a file FFmpeg cannot write: {names:?}"
        );
    }

    /// The unstyled original is deleted again: two files with the same words is
    /// clutter, and only the events could say which case this was.
    #[test]
    fn an_unstyled_ass_track_leaves_only_its_srt() {
        if !ffmpeg_present() {
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let source = temp_dir.path().join("sources");
        std::fs::create_dir_all(&source).unwrap();

        let plain = source.join("plain.ass");
        std::fs::write(
            &plain,
            "[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\nFormat: Name, Fontname, Fontsize\nStyle: Default,Arial,20\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:00.10,0:00:01.50,Default,,0,0,0,,Just dialogue.\n",
        )
        .unwrap();

        let input = source.join("show.mkv");
        let built = std::process::Command::new("ffmpeg")
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=160x120:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=2",
            ])
            .arg("-i")
            .arg(&plain)
            .args([
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "2:s",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
                "-c:s",
                "copy",
                "-metadata:s:s:0",
                "language=eng",
                "-y",
            ])
            .arg(&input)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(built.success(), "could not build the source");

        let job = Job::new(
            input,
            MediaFileType::Mkv,
            Operation::Remux {
                audio: AudioAction::Copy,
                subtitles: SubtitleAction::Extract,
            },
            QualitySettings::default(),
            PostProcessingSettings::default(),
            temp_dir.path(),
        );

        deliver(&job, temp_dir.path());

        let names = sidecar_names(&source);
        assert!(names.contains(&"show.eng.srt".to_string()), "{names:?}");
        assert!(
            !names.iter().any(|name| name == "show.eng.ass"),
            "an unstyled original is not worth keeping: {names:?}"
        );
    }

    /// A job that cannot read its source's subtitle streams fails, whether it
    /// carries them or extracts them. On success `work` disables the source,
    /// so a job that went on without knowing what the tracks were could leave
    /// the library holding a file without them.
    #[test]
    fn a_job_that_cannot_probe_its_subtitles_fails() {
        if !ffmpeg_present() {
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let source = temp_dir.path().join("sources");
        std::fs::create_dir_all(&source).unwrap();
        let input = build_subtitled_source(&source.join("show.mkv"), &source);

        for subtitles in [SubtitleAction::Keep, SubtitleAction::Extract] {
            let job = Job::new(
                input.clone(),
                MediaFileType::Mkv,
                Operation::Remux {
                    audio: AudioAction::Copy,
                    subtitles,
                },
                QualitySettings::default(),
                PostProcessingSettings::default(),
                temp_dir.path(),
            );
            let work_folder = temp_dir.path().join(format!("work-{subtitles:?}"));
            std::fs::create_dir_all(&work_folder).unwrap();

            let result = tokio::runtime::Runtime::new().unwrap().block_on(
                FFmpegProcessor::new(false)
                    .with_ffprobe("plexify-test-no-such-ffprobe")
                    .process_job(&job, None, &work_folder),
            );

            let error = result.expect_err(&format!(
                "{subtitles:?}: the job succeeded without reading the subtitle streams; \
                 the work folder holds {:?}",
                std::fs::read_dir(&work_folder)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect::<Vec<_>>()
            ));
            assert!(
                format!("{error:#}").contains("subtitle streams"),
                "{subtitles:?}: the failure should say what could not be read: {error:#}"
            );
        }
    }
}
