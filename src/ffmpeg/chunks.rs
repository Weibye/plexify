//! The resumable encode a long source is divided into: the plan, the chunks,
//! and the join that puts them back together.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use tracing::{debug, info};

use super::builder::format_seconds;
use super::operation::{with_encoded_audio, with_streams};
use super::processor::SourceFacts;
use super::{FFmpegCommandBuilder, FFmpegProcessor};
use crate::job::{Job, Operation, QualitySettings};
use crate::paths::to_forward_slashes;

/// How much of a source one chunk of a resumable encode covers.
///
/// This is the most work an interrupted worker can lose. Shorter chunks lose
/// less and cost more: every boundary is one more FFmpeg start-up, one more
/// keyframe forced into the video, and one more seam in the audio.
pub const CHUNK_SECONDS: f64 = 300.0;

/// Below this, a source is encoded in one pass.
///
/// Chunking only pays for itself when there is enough work to lose. A file
/// shorter than this re-encodes from scratch faster than the seams are worth.
pub const MIN_CHUNKED_SECONDS: f64 = 900.0;

/// Where the chunk directory for a job lives.
///
/// Named after the job id, which is the v5 UUID of the input path, so the
/// worker that reclaims an abandoned job finds the same directory the last one
/// was filling.
pub fn chunk_dir_for(job_id: &str, work_folder: &Path) -> PathBuf {
    work_folder.join(format!("{job_id}.chunks"))
}

/// Records the settings a chunk directory's contents were encoded with.
const CHUNK_SETTINGS_FILE: &str = "settings.json";

/// One piece of a resumable encode.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    /// Position in the sequence, and the name the chunk file is given.
    pub index: usize,
    /// Where in the source this chunk starts, in seconds.
    pub start: f64,
    /// How long the chunk runs, or `None` for the last one, which runs to the
    /// end of the source.
    pub duration: Option<f64>,
}

impl Chunk {
    /// The finished chunk. It is only given this name once FFmpeg has exited
    /// successfully, so its existence is what says the chunk is complete.
    pub fn path(&self, chunk_dir: &Path) -> PathBuf {
        chunk_dir.join(format!("{:05}.ts", self.index))
    }

    /// Where the chunk is written while FFmpeg is still filling it.
    pub fn partial_path(&self, chunk_dir: &Path) -> PathBuf {
        chunk_dir.join(format!("{:05}.ts.part", self.index))
    }
}

/// Divide a source of `duration` seconds into chunks of at most `chunk_seconds`.
///
/// The last chunk is left open-ended rather than being given a length. The
/// duration came from a probe and may be a little short of the truth, and a
/// final chunk that stops at the probed figure would quietly clip the end of
/// the file.
pub fn plan_chunks(duration: f64, chunk_seconds: f64) -> Vec<Chunk> {
    let count = ((duration / chunk_seconds).ceil() as usize).max(1);

    (0..count)
        .map(|index| Chunk {
            index,
            start: index as f64 * chunk_seconds,
            duration: (index + 1 < count).then_some(chunk_seconds),
        })
        .collect()
}

/// One entry of a concat demuxer list file.
///
/// The demuxer takes a quoted path, and treats a backslash as an escape, so a
/// Windows path has to be handed over with forward slashes or every separator
/// disappears. A single quote inside a filename is escaped the way the demuxer
/// spells it.
///
/// The `duration` line is what keeps a long file in sync. Without it the demuxer
/// starts each chunk where the previous one actually ended, and a chunk never
/// ends exactly where it was asked to: `-t` cuts video on a frame boundary and
/// audio on a 1024-sample AAC frame boundary, so each one runs a few
/// milliseconds long. That error is *per boundary*, so it accumulates - a
/// three-hour film crosses thirty-five of them and ends a third of a second
/// adrift, where a fifteen-minute episode crosses two and shows nothing.
/// Declaring the length the chunk was cut to pins it to the position it came
/// from, and the error cannot compound.
///
/// `declared_duration` is `None` for the last entry in a list. That chunk runs
/// to wherever the source actually ended - which is not always where the plan
/// said, because a container can over-report its duration and the chunk after
/// the end is dropped - so declaring a length for it would hold the timeline
/// open past the content. Nothing follows it whose start could be wrong anyway.
fn concat_list_entry(chunk: &Chunk, chunk_dir: &Path, declared_duration: Option<f64>) -> String {
    let path = to_forward_slashes(&chunk.path(chunk_dir)).replace('\'', "'\\''");
    let mut entry = format!("file '{path}'\n");

    if let Some(duration) = declared_duration {
        entry.push_str(&format!("duration {}\n", format_seconds(duration)));
    }

    entry
}

/// The concat list a join reads its picture from, one entry per encoded chunk.
///
/// Every entry but the last declares its length; see [`concat_list_entry`].
fn concat_list(chunks: &[Chunk], chunk_dir: &Path) -> String {
    let last = chunks.len() - 1;
    chunks
        .iter()
        .enumerate()
        .map(|(position, chunk)| {
            let declared = chunk.duration.filter(|_| position != last);
            concat_list_entry(chunk, chunk_dir, declared)
        })
        .collect()
}

/// The FFmpeg arguments that encode one chunk's picture to `partial_path`.
///
/// A chunk carries the picture alone; see [`FFmpegProcessor::encode_chunks`].
fn chunk_args(
    quality: &QualitySettings,
    input_path: &Path,
    chunk: &Chunk,
    partial_path: &Path,
) -> Vec<String> {
    let mut builder = FFmpegCommandBuilder::new()
        .with_generated_pts()
        .with_overwrite()
        .with_seek(chunk.start)
        .with_input(input_path)
        .with_stream_mapping(&["0:V"])
        .with_video_encoding(quality)
        .with_transport_stream_output()
        .with_output(partial_path);

    if let Some(chunk_duration) = chunk.duration {
        builder = builder.with_duration(chunk_duration);
    }

    builder.build()
}

/// The FFmpeg arguments that join the chunks listed at `list_path` into
/// `output_path`.
///
/// The picture is copied out of the chunks; the sound and the subtitles are
/// read from the source, which is input 1, and encoded here. The chunks hold
/// no subtitles, because a subtitle event straddling a boundary would have
/// been cut in half by it. The cap rides on the audio for the same reason it
/// does everywhere else - a client that refuses 5.1 refuses it however the
/// file was assembled.
fn join_args(
    job: &Job,
    source: &SourceFacts<'_>,
    list_path: &Path,
    output_path: &Path,
) -> Vec<String> {
    let mux_builder = with_encoded_audio(
        FFmpegCommandBuilder::new()
            .with_overwrite()
            .with_concat_list(list_path)
            .with_input(source.path)
            .with_video_copy(),
        job,
        job.operation.audio_cap(),
        source.channels,
    )
    .with_negative_composition_offsets()
    .with_output(output_path);

    // Only a re-encode is chunked, and a re-encode adds no track.
    with_streams(mux_builder, 1, false, source.subtitles).build()
}

/// How a source is divided up for a resumable encode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chunking {
    /// How much of the source one chunk covers.
    pub chunk_seconds: f64,
    /// The shortest source worth chunking at all.
    pub min_source_seconds: f64,
}

impl Default for Chunking {
    fn default() -> Self {
        Self {
            chunk_seconds: CHUNK_SECONDS,
            min_source_seconds: MIN_CHUNKED_SECONDS,
        }
    }
}

impl FFmpegProcessor {
    /// Encode the source in chunks, reusing any chunk an earlier attempt
    /// already finished.
    ///
    /// A chunk is written under a `.part` name and only given its final name
    /// once FFmpeg has exited successfully, so the presence of a chunk file is
    /// what says the work behind it is sound. Nothing else has to be recorded:
    /// the chunk directory *is* the progress, which is the same trade the job
    /// queue makes.
    ///
    /// Only the picture is cut up. Both of the other streams are left out of
    /// the chunks and taken from the source at the join, for reasons that are
    /// nearly the same one: a subtitle event straddling a boundary would be cut
    /// in half by it, and an AAC encoder run per chunk emits a priming frame
    /// per chunk. MPEG-TS has no edit list to carry that frame in, so it takes
    /// up real time in the chunk, and whether it lands inside or outside the
    /// length the concat list declares varies from chunk to chunk. Measured on
    /// a 30s source in 5s pieces, that stepped the sound one AAC frame - 21ms -
    /// back and forth at every boundary while the picture stayed put. Encoding
    /// the audio once, over the whole file, is one priming frame, in front of
    /// an MP4 that has the edit list to hold it: the same arrangement the
    /// one-pass encode already produces.
    pub(super) async fn process_in_chunks(
        &self,
        job: &Job,
        source: &SourceFacts<'_>,
        output_path: &Path,
        work_folder: &Path,
        duration: f64,
    ) -> Result<()> {
        let input_path = source.path;

        // `encode_chunks` re-encodes the video, so reaching here with a remux
        // would silently throw away the copy the operation exists for. The
        // caller checks; this says so where the code that depends on it lives.
        debug_assert!(
            matches!(job.operation, Operation::Reencode { .. }),
            "only a re-encode is chunked"
        );

        let chunk_dir = chunk_dir_for(&job.id, work_folder);
        self.prepare_chunk_dir(job, &chunk_dir).await?;

        let planned = plan_chunks(duration, self.chunking.chunk_seconds);
        let chunks = self
            .encode_chunks(job, input_path, &chunk_dir, &planned)
            .await?;

        let list_path = chunk_dir.join("chunks.txt");
        tokio::fs::write(&list_path, concat_list(&chunks, &chunk_dir).as_bytes()).await?;

        self.run(
            join_args(job, source, &list_path, output_path),
            "joining chunks",
        )
        .await?;

        // The chunks have served their purpose, and are the size of the output
        // again. Anything left here would be re-used by a later run.
        tokio::fs::remove_dir_all(&chunk_dir).await?;

        info!(
            "✅ Conversion successful: {} -> {}",
            to_forward_slashes(input_path),
            to_forward_slashes(output_path)
        );
        Ok(())
    }

    /// Encode every chunk that is not already on disk.
    ///
    /// A chunk is written under a partial name and only renamed once FFmpeg has
    /// exited successfully, so a finished chunk file existing is what says the
    /// work behind it is sound and can be skipped. That rename is the whole
    /// resume mechanism - nothing else is written down.
    ///
    /// A chunk carries the picture alone. The audio is encoded once at the
    /// join instead, because an encoder run per chunk is a priming frame per
    /// chunk and MPEG-TS has nowhere to put one but the timeline itself.
    async fn encode_chunks(
        &self,
        job: &Job,
        input_path: &Path,
        chunk_dir: &Path,
        chunks: &[Chunk],
    ) -> Result<Vec<Chunk>> {
        let total = chunks.len();

        let already_encoded = chunks
            .iter()
            .filter(|chunk| chunk.path(chunk_dir).exists())
            .count();
        if already_encoded > 0 {
            info!("↩️ Resuming: {already_encoded} of {total} chunks already encoded.");
        }

        for chunk in chunks {
            let finished_path = chunk.path(chunk_dir);
            if finished_path.exists() {
                debug!("Reusing chunk {} of {total}", chunk.index + 1);
                continue;
            }

            let partial_path = chunk.partial_path(chunk_dir);

            self.run(
                chunk_args(&job.quality_settings, input_path, chunk, &partial_path),
                &format!("chunk {} of {total}", chunk.index + 1),
            )
            .await?;

            // The plan came from the container's duration, which is the longest
            // of its streams - so the final chunk can begin after the last
            // video frame, and `plan_chunks` rounds up, so it can begin after
            // everything has ended. What comes out is a chunk of a few hundred
            // bytes that ffprobe can find no video stream in at all, and
            // joining it leaves the MP4's video track running a whole frame
            // interval past its own last picture.
            //
            // A source whose sound outlives its image loses nothing to this:
            // the audio is not in the chunks at all, and the join reads it from
            // the source in full however far the picture got.
            if self.probe_video_stream_count(&partial_path).await == Some(0) {
                let _ = tokio::fs::remove_file(&partial_path).await;
                debug!(
                    "Chunk {} begins after the last video frame; the picture is shorter than the \
                     container's duration",
                    chunk.index + 1
                );
                break;
            }

            tokio::fs::rename(&partial_path, &finished_path).await?;
            info!("📦 Encoded chunk {} of {total}", chunk.index + 1);
        }

        let encoded: Vec<Chunk> = chunks
            .iter()
            .take_while(|chunk| chunk.path(chunk_dir).exists())
            .cloned()
            .collect();

        if encoded.is_empty() {
            return Err(anyhow!(
                "FFmpeg produced no output for {}",
                to_forward_slashes(input_path)
            ));
        }

        Ok(encoded)
    }

    /// Set up the chunk directory, discarding chunks encoded to a different
    /// specification.
    ///
    /// The directory is keyed on the job id, which is the v5 UUID of the input
    /// path and therefore stable forever. So chunks left by a parked job would
    /// be found again by a job re-scanned for the same file - and if the quality
    /// settings changed in between, half the output would be encoded one way and
    /// half the other with nothing recording that it happened. The settings the
    /// chunks were made with are written down beside them, and anything that
    /// does not match them is thrown away rather than reused.
    async fn prepare_chunk_dir(&self, job: &Job, chunk_dir: &Path) -> Result<()> {
        let stamp_path = chunk_dir.join(CHUNK_SETTINGS_FILE);
        let stamp = serde_json::to_string(&job.quality_settings)?;

        if chunk_dir.exists() {
            let existing = tokio::fs::read_to_string(&stamp_path).await.ok();
            if existing.as_deref() != Some(stamp.as_str()) {
                info!(
                    "🗑️ Discarding chunks encoded with different settings: {}",
                    to_forward_slashes(chunk_dir)
                );
                tokio::fs::remove_dir_all(chunk_dir).await?;
            }
        }

        tokio::fs::create_dir_all(chunk_dir).await?;
        tokio::fs::write(&stamp_path, stamp.as_bytes()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::subtitle_streams::SubtitleSource;
    use crate::ffmpeg::test_support::*;
    use crate::ffmpeg::{select_subtitle_streams, SubtitleStream};
    use crate::job::{MediaFileType, PostProcessingSettings, QualitySettings};
    use std::process::Stdio;
    use tempfile::TempDir;

    #[test]
    fn a_list_cut_short_leaves_its_final_entry_open_ended() {
        // The probe over-reported, the chunk past the end came back empty, and
        // the entry that is now last may hold less than the plan asked for.
        // Declaring a length for it would hold the timeline open past the
        // content and freeze the end of the film.
        let chunks = plan_chunks(1000.0, 300.0);

        let list = concat_list(&chunks[..2], Path::new("/work/x.chunks"));

        assert_eq!(
            list,
            "file '/work/x.chunks/00000.ts'\nduration 300.000\n\
             file '/work/x.chunks/00001.ts'\n"
        );
    }

    #[test]
    fn a_source_is_divided_into_whole_chunks_with_an_open_ended_last_one() {
        let chunks = plan_chunks(1000.0, 300.0);

        assert_eq!(
            chunks,
            vec![
                Chunk {
                    index: 0,
                    start: 0.0,
                    duration: Some(300.0)
                },
                Chunk {
                    index: 1,
                    start: 300.0,
                    duration: Some(300.0)
                },
                Chunk {
                    index: 2,
                    start: 600.0,
                    duration: Some(300.0)
                },
                // The probed duration may be a little short of the truth, so the
                // last chunk is given no length and runs to the end of the file.
                Chunk {
                    index: 3,
                    start: 900.0,
                    duration: None
                },
            ]
        );
    }

    #[test]
    fn a_source_shorter_than_one_chunk_still_yields_one() {
        let chunks = plan_chunks(12.0, 300.0);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].start, 0.0);
        assert_eq!(chunks[0].duration, None);
    }

    #[test]
    fn chunks_cover_the_source_without_a_gap_or_an_overlap() {
        let chunks = plan_chunks(2000.0, 300.0);

        for pair in chunks.windows(2) {
            let (earlier, later) = (&pair[0], &pair[1]);
            assert_eq!(
                earlier.start + earlier.duration.unwrap(),
                later.start,
                "chunk {} must end exactly where chunk {} begins",
                earlier.index,
                later.index
            );
        }
    }

    #[test]
    fn a_finished_chunk_is_named_differently_from_one_being_written() {
        let chunk = Chunk {
            index: 7,
            start: 2100.0,
            duration: Some(300.0),
        };
        let chunk_dir = Path::new("/work/abc.chunks");

        // Sorting the names has to sort the chunks, because that is the order
        // the concat list is written in.
        assert!(chunk.path(chunk_dir).ends_with("00007.ts"));
        assert!(chunk.partial_path(chunk_dir).ends_with("00007.ts.part"));
        assert_ne!(chunk.path(chunk_dir), chunk.partial_path(chunk_dir));
    }

    #[test]
    fn a_concat_list_entry_uses_separators_the_demuxer_does_not_eat() {
        // A backslash is an escape character to the concat demuxer, so a Windows
        // path handed over as-is would lose every separator.
        let last = Chunk {
            index: 0,
            start: 0.0,
            duration: None,
        };
        let entry = concat_list_entry(&last, Path::new(r"C:\work\abc.chunks"), None);
        assert_eq!(entry, "file 'C:/work/abc.chunks/00000.ts'\n");

        let quoted = concat_list_entry(&last, Path::new("/work/it's here"), None);
        assert_eq!(quoted, "file '/work/it'\\''s here/00000.ts'\n");
    }

    #[test]
    fn every_chunk_but_the_last_declares_the_length_it_was_cut_to() {
        // Without this the demuxer starts each chunk where the last one actually
        // ended, and a chunk always runs a few milliseconds long. The error is
        // per boundary, so it accumulates over a feature-length file.
        let chunks = plan_chunks(1000.0, 300.0);
        let list = concat_list(&chunks, Path::new("/work/x.chunks"));

        assert_eq!(
            list,
            "file '/work/x.chunks/00000.ts'\nduration 300.000\n\
             file '/work/x.chunks/00001.ts'\nduration 300.000\n\
             file '/work/x.chunks/00002.ts'\nduration 300.000\n\
             file '/work/x.chunks/00003.ts'\n"
        );
    }

    #[test]
    fn a_chunk_is_encoded_from_a_seek_into_a_transport_stream() {
        let quality = QualitySettings {
            ffmpeg_preset: "veryfast".to_string(),
            ffmpeg_crf: "23".to_string(),
            ffmpeg_audio_bitrate: "128k".to_string(),
        };

        let chunk = Chunk {
            index: 2,
            start: 600.0,
            duration: Some(300.0),
        };

        let args = chunk_args(
            &quality,
            Path::new("/media/film.mkv"),
            &chunk,
            Path::new("/work/00002.ts.part"),
        );

        let joined = args.join(" ");

        // The seek is an input option, so it costs a seek rather than decoding
        // ten minutes of film and throwing the result away.
        assert!(
            joined.contains("-ss 600.000 -i /media/film.mkv"),
            "seek must come before the input: {joined}"
        );
        // `-t` is an output option, and must still land before the output file,
        // which is what the builder's buckets are for.
        assert!(joined.contains("-t 300.000"));
        assert!(joined.ends_with("/work/00002.ts.part"));
        assert!(joined.contains("-f mpegts"));
        // Subtitles are held back for the join, so a subtitle spanning a chunk
        // boundary is not cut in half by it.
        assert!(!joined.contains("0:s"));
        // And so is the audio: one AAC encoder run per chunk is one priming
        // frame per chunk, and MPEG-TS has to spend real time on it.
        assert!(!joined.contains("0:a"), "{joined}");
        assert!(!joined.contains("-c:a"), "{joined}");
    }

    #[test]
    fn the_join_copies_the_picture_and_takes_everything_else_from_the_source() {
        let job = job_for(
            "/media/film.mkv",
            MediaFileType::Mkv,
            Operation::Reencode { channels: Some(2) },
        );

        let subtitles = SubtitleSource::Embedded(select_subtitle_streams(&[SubtitleStream {
            codec: "subrip".to_string(),
            language: None,
            forced: false,
        }]));
        let source = SourceFacts {
            path: Path::new("/media/film.mkv"),
            subtitles: Some(&subtitles),
            channels: &[6],
        };

        let joined = join_args(
            &job,
            &source,
            Path::new("/work/chunks.txt"),
            Path::new("/work/film.mp4"),
        )
        .join(" ");

        // `-f concat` applies to the input that follows it, so the chunk list
        // has to be input 0 and the source input 1.
        assert!(
            joined.contains("-f concat -safe 0 -i /work/chunks.txt -i /media/film.mkv"),
            "{joined}"
        );
        // Only the picture is copied. The sound is encoded here, once, rather
        // than a chunk at a time.
        assert!(joined.contains("-c:v copy"), "{joined}");
        assert!(!joined.contains("-c:a copy"), "{joined}");
        assert!(joined.contains("-c:a aac"), "{joined}");
        // And the cap comes with it, because this is the only place the
        // output's audio is written.
        assert!(joined.contains("-ac:a:0 2"), "{joined}");
        assert!(joined.contains("-c:s mov_text"), "{joined}");
        assert!(joined.ends_with("/work/film.mp4"), "{joined}");
    }

    /// Build a short MKV whose sound says where in the file it is: a 100ms
    /// 1kHz tone at the start of every second, silence in between.
    ///
    /// A continuous tone cannot show a timing fault, because every part of it
    /// looks like every other part. Marks a whole AAC frame apart can, and are
    /// what `beep_starts` reads back.
    ///
    /// 25fps rather than the 10fps the other fixtures use, and that is
    /// load-bearing. A chunk's own timeline has to hold both the picture's
    /// reorder delay and the audio encoder's priming frame, and at 10fps a
    /// frame interval is five times an AAC frame, so the priming disappears
    /// inside it and every chunk comes out with the same offset. What is being
    /// measured only shows where the two are comparable.
    fn build_beeping_source(path: &Path, seconds: u32) {
        let built = std::process::Command::new("ffmpeg")
            .args([
                "-f",
                "lavfi",
                "-i",
                &format!("testsrc=duration={seconds}:size=160x120:rate=25"),
                "-f",
                "lavfi",
                "-i",
                &format!("sine=frequency=1000:sample_rate=48000:duration={seconds}"),
                "-af",
                "volume=0:enable='gte(mod(t,1),0.1)'",
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-y",
            ])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(built.success(), "could not build the beeping test source");
    }

    /// Where each tone begins, as `silencedetect` hears it.
    ///
    /// The same detector is run over the source and over the output, so the
    /// two lists are directly comparable: whatever the encoder's window does
    /// to the attack, it does to both.
    ///
    /// The last run of silence is dropped. It is ended by the end of the file
    /// rather than by a tone, so where it falls says only how much padding the
    /// encoder left, which is not what any of this is measuring.
    fn beep_starts(path: &Path) -> Vec<f64> {
        let output = std::process::Command::new("ffmpeg")
            .args(["-v", "info", "-i"])
            .arg(path)
            .args(["-af", "silencedetect=n=-40dB:d=0.2", "-f", "null", "-"])
            .output()
            .unwrap();

        let mut marks: Vec<f64> = String::from_utf8_lossy(&output.stderr)
            .lines()
            .filter_map(|line| line.split("silence_end: ").nth(1))
            .filter_map(|rest| rest.split_whitespace().next())
            .filter_map(|value| value.parse().ok())
            .collect();
        marks.pop();
        marks
    }

    /// What FFprobe says a file runs for.
    fn probed_duration(path: &Path) -> f64 {
        let output = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap()
    }

    /// How long one stream of a file runs, as the container declares it.
    ///
    /// This is the figure a player lays out its scrub bar from, and the one
    /// that has to agree with the source: a track declared longer than its own
    /// last frame is a tail of nothing.
    fn probed_stream_duration(path: &Path, stream: &str) -> f64 {
        let output = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                stream,
                "-show_entries",
                "stream=duration",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .and_then(|line| line.trim().parse().ok())
            .unwrap_or_else(|| panic!("no {stream} stream duration in {path:?}"))
    }

    #[tokio::test]
    async fn a_long_source_is_encoded_in_chunks_and_joined_back_into_one_file() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("film.mkv");
        build_chunking_source(&input, 6);

        let work_folder = temp.path().join("work");
        std::fs::create_dir_all(&work_folder).unwrap();

        let job = chunking_job(&input);
        let processor = FFmpegProcessor::new(false).with_chunking(TEST_CHUNKING);

        processor
            .process_job(&job, None, &work_folder)
            .await
            .unwrap();

        let output = job.work_folder_output_path(&work_folder);
        assert!(
            output.exists(),
            "the joined file should be in the work folder"
        );

        // The join must reproduce the whole source, not one chunk of it.
        let source_duration = probed_duration(&input);
        let output_duration = probed_duration(&output);
        assert!(
            (output_duration - source_duration).abs() < 0.5,
            "joined output runs {output_duration}s against a {source_duration}s source"
        );

        // Video, audio, and the subtitles that were held back for the join.
        assert_eq!(stream_codecs(&output), vec!["h264", "aac", "mov_text"]);

        // Nothing is left behind to be mistaken for progress on a later run.
        assert!(!chunk_dir_for(&job.id, &work_folder).exists());
    }

    /// The sound of a joined file has to sit where the source's does at every
    /// boundary, not only at the first one.
    ///
    /// This is what an AAC encoder run per chunk cost. MPEG-TS has no edit
    /// list, so each chunk's priming frame took up real time on that chunk's
    /// own timeline, and whether it fell inside the length the concat list
    /// declares varied from chunk to chunk. The sound stepped one AAC frame -
    /// 21ms at 48kHz - back and forth at every boundary while the picture
    /// stayed where it belonged. An offset is one thing; sync that changes
    /// every few minutes is another, and it also puts a floor under any later
    /// measurement of A/V alignment.
    ///
    /// The tolerance is a quarter of that step, which is wide enough for what
    /// re-encoding does to the attack of a tone and narrow enough that the
    /// step itself cannot hide inside it. Measured with FFmpeg 9.0 on Windows:
    /// every mark 0.2ms late once the audio is encoded once, and a run of
    /// eight marks 21.2ms late between two that are not when it is encoded a
    /// chunk at a time.
    #[tokio::test]
    async fn the_joined_audio_lands_where_the_source_has_it() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("film.mkv");
        // Long enough to cross five boundaries. The fault alternates, so a
        // source crossing one could show nothing at all.
        build_beeping_source(&input, 12);

        let work_folder = temp.path().join("work");
        std::fs::create_dir_all(&work_folder).unwrap();

        let job = chunking_job(&input);
        FFmpegProcessor::new(false)
            .with_chunking(TEST_CHUNKING)
            .process_job(&job, None, &work_folder)
            .await
            .unwrap();

        let output = job.work_folder_output_path(&work_folder);

        let source = beep_starts(&input);
        let joined = beep_starts(&output);

        assert!(
            source.len() > 5,
            "the fixture must carry a mark on either side of several boundaries: {source:?}"
        );
        assert_eq!(
            source.len(),
            joined.len(),
            "the output has {} marks against the source's {}: {joined:?} vs {source:?}",
            joined.len(),
            source.len()
        );

        for (expected, actual) in source.iter().zip(&joined) {
            assert!(
                (actual - expected).abs() < 0.005,
                "a mark the source has at {expected}s is at {actual}s in the joined output: \
                 {joined:?} against {source:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_joined_output_ends_where_the_source_ends() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("film.mkv");
        // Long enough for ten chunk boundaries at the test chunk length. The
        // faults this guards against are both invisible on a shorter source:
        // the tail is one video frame, and a boundary that did not declare the
        // length it was cut to costs a few milliseconds each time round.
        build_chunking_source(&input, 20);

        let work_folder = temp.path().join("work");
        std::fs::create_dir_all(&work_folder).unwrap();

        let job = chunking_job(&input);
        let processor = FFmpegProcessor::new(false).with_chunking(TEST_CHUNKING);

        let source_duration = probed_duration(&input);
        let planned = plan_chunks(source_duration, TEST_CHUNKING.chunk_seconds);
        assert!(
            planned.len() > 10,
            "the source must cross enough boundaries for a per-boundary error to show: {} chunks",
            planned.len()
        );

        processor
            .process_job(&job, None, &work_folder)
            .await
            .unwrap();

        let output = job.work_folder_output_path(&work_folder);

        // The container's duration is the longest of its streams, so the plan's
        // final chunk can begin after the last picture and hold nothing at all.
        // Joined in, that chunk leaves the video track running a whole frame
        // interval past its own last frame.
        //
        // The same figure catches a boundary that stopped declaring its length,
        // because that error is per boundary and this source crosses ten of
        // them.
        //
        // The picture is measured against the source's *picture*, not against
        // its container: an MKV declares no per-stream duration, and its
        // container outlives its last frame by the audio encoder's padding, so
        // comparing the two would leave a whole AAC frame of room for a fault
        // to sit in.
        let source_frames = packet_times(&input, "v");
        let source_video = source_frames.last().unwrap()
            + (source_frames[source_frames.len() - 1] - source_frames[source_frames.len() - 2]);
        let video = probed_stream_duration(&output, "v");
        assert!(
            (video - source_video).abs() < 0.02,
            "the joined video runs {video}s against a {source_video}s source picture"
        );

        let audio = probed_stream_duration(&output, "a");
        assert!(
            (audio - source_duration).abs() < 0.05,
            "the joined audio runs {audio}s against a {source_duration}s source"
        );

        // And the picture itself is where it belongs at the end of the file,
        // not merely declared to be.
        let last_frame = *packet_times(&output, "v").last().unwrap();
        let source_last_frame = *packet_times(&input, "v").last().unwrap();
        assert!(
            (last_frame - source_last_frame).abs() < 0.05,
            "the last picture is at {last_frame}s, against {source_last_frame}s in the source"
        );
    }

    #[tokio::test]
    async fn a_chunk_an_earlier_attempt_finished_is_not_encoded_again() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("film.mkv");
        build_chunking_source(&input, 6);

        let chunk_dir = temp.path().join("chunks");
        std::fs::create_dir_all(&chunk_dir).unwrap();

        let job = chunking_job(&input);
        let processor = FFmpegProcessor::new(false).with_chunking(TEST_CHUNKING);
        let chunks = plan_chunks(probed_duration(&input), TEST_CHUNKING.chunk_seconds);
        assert!(chunks.len() > 1, "the test source must span several chunks");

        // A worker gets part-way through and is killed.
        processor
            .encode_chunks(&job, &input, &chunk_dir, &chunks[..1])
            .await
            .unwrap();

        let first_chunk = chunks[0].path(&chunk_dir);
        let untouched = std::fs::metadata(&first_chunk).unwrap().modified().unwrap();

        // The next worker picks the job back up and finishes it.
        let encoded = processor
            .encode_chunks(&job, &input, &chunk_dir, &chunks)
            .await
            .unwrap();

        assert_eq!(
            std::fs::metadata(&first_chunk).unwrap().modified().unwrap(),
            untouched,
            "the chunk the first worker finished was encoded a second time"
        );
        // What was encoded, rather than what was planned: the plan divides up
        // the container's duration, and its final chunk can begin after the
        // last picture, in which case it is dropped rather than joined in.
        assert!(
            encoded.len() > 1,
            "the test source must span several chunks: {encoded:?}"
        );
        for chunk in &encoded {
            assert!(
                chunk.path(&chunk_dir).exists(),
                "chunk {} is missing",
                chunk.index
            );
            assert!(
                !chunk.partial_path(&chunk_dir).exists(),
                "chunk {} was left half-written",
                chunk.index
            );
        }
    }

    #[tokio::test]
    async fn a_source_too_short_to_chunk_is_encoded_in_one_pass() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("clip.mkv");
        build_chunking_source(&input, 2);

        let work_folder = temp.path().join("work");
        std::fs::create_dir_all(&work_folder).unwrap();

        let job = chunking_job(&input);
        let processor = FFmpegProcessor::new(false).with_chunking(TEST_CHUNKING);

        processor
            .process_job(&job, None, &work_folder)
            .await
            .unwrap();

        assert!(job.work_folder_output_path(&work_folder).exists());
        assert!(
            !chunk_dir_for(&job.id, &work_folder).exists(),
            "a source below the threshold should never be divided up"
        );
    }

    #[tokio::test]
    async fn chunks_encoded_with_other_settings_are_discarded_rather_than_reused() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("film.mkv");
        build_chunking_source(&input, 6);

        let work_folder = temp.path().join("work");
        std::fs::create_dir_all(&work_folder).unwrap();

        let job = chunking_job(&input);
        let processor = FFmpegProcessor::new(false).with_chunking(TEST_CHUNKING);
        let chunk_dir = chunk_dir_for(&job.id, &work_folder);

        let chunks = plan_chunks(probed_duration(&input), TEST_CHUNKING.chunk_seconds);
        processor.prepare_chunk_dir(&job, &chunk_dir).await.unwrap();
        processor
            .encode_chunks(&job, &input, &chunk_dir, &chunks[..1])
            .await
            .unwrap();
        assert!(chunks[0].path(&chunk_dir).exists());

        // The job file was moved out of _failed and the library re-scanned at a
        // different quality. The id is derived from the input path, so the new
        // job finds the same directory - and half an output at one CRF and half
        // at another, with nothing recording it, is worse than redoing the work.
        let mut requeued = job.clone();
        requeued.quality_settings.ffmpeg_crf = "18".to_string();
        processor
            .prepare_chunk_dir(&requeued, &chunk_dir)
            .await
            .unwrap();

        assert!(
            !chunks[0].path(&chunk_dir).exists(),
            "a chunk encoded to a different specification must not be reused"
        );
    }

    #[tokio::test]
    async fn chunks_encoded_with_the_same_settings_survive_a_restart() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("film.mkv");
        build_chunking_source(&input, 6);

        let work_folder = temp.path().join("work");
        std::fs::create_dir_all(&work_folder).unwrap();

        let job = chunking_job(&input);
        let processor = FFmpegProcessor::new(false).with_chunking(TEST_CHUNKING);
        let chunk_dir = chunk_dir_for(&job.id, &work_folder);

        let chunks = plan_chunks(probed_duration(&input), TEST_CHUNKING.chunk_seconds);
        processor.prepare_chunk_dir(&job, &chunk_dir).await.unwrap();
        processor
            .encode_chunks(&job, &input, &chunk_dir, &chunks[..1])
            .await
            .unwrap();

        processor.prepare_chunk_dir(&job, &chunk_dir).await.unwrap();

        assert!(
            chunks[0].path(&chunk_dir).exists(),
            "guarding against a settings change must not throw away good work"
        );
    }

    #[tokio::test]
    async fn a_parked_job_does_not_leave_its_chunks_behind() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("film.mkv");
        build_chunking_source(&input, 6);

        let work_folder = temp.path().join("work");
        std::fs::create_dir_all(&work_folder).unwrap();

        let job = chunking_job(&input);
        let processor = FFmpegProcessor::new(false).with_chunking(TEST_CHUNKING);
        let chunk_dir = chunk_dir_for(&job.id, &work_folder);

        let chunks = plan_chunks(probed_duration(&input), TEST_CHUNKING.chunk_seconds);
        processor.prepare_chunk_dir(&job, &chunk_dir).await.unwrap();
        processor
            .encode_chunks(&job, &input, &chunk_dir, &chunks[..1])
            .await
            .unwrap();
        assert!(chunk_dir.exists());

        // And whatever the interrupted join had started writing.
        let partial_output = job.work_folder_output_path(&work_folder);
        std::fs::write(&partial_output, b"half a film").unwrap();

        // By id, which is all the startup sweep has when it parks a job that
        // kept taking its worker down.
        processor.discard_work_for_id(&job.id, &work_folder).await;

        assert!(
            !chunk_dir.exists(),
            "nothing will ever come back for a parked job's chunks"
        );
        assert!(!partial_output.exists());
    }

    /// A job for a WebM and its sidecar, chunked at test sizes.
    fn webm_job(input: &Path) -> Job {
        Job::new(
            input.to_path_buf(),
            MediaFileType::WebM,
            Operation::Reencode { channels: None },
            QualitySettings {
                ffmpeg_preset: "ultrafast".to_string(),
                ffmpeg_crf: "30".to_string(),
                ffmpeg_audio_bitrate: "64k".to_string(),
            },
            PostProcessingSettings::default(),
            input.parent().unwrap(),
        )
    }

    /// The join treats a WebM differently from an MKV: its subtitle is a
    /// separate file and is the reason the job exists, so the join maps it
    /// without a `?`. If it produced no stream there the job would fail
    /// outright rather than degrade - three attempts and then `_failed`, for
    /// every WebM in the library over the chunking threshold.
    ///
    /// The source is VP8 and Opus rather than H.264 and AAC for the same reason.
    /// The chunks are cut by seeking into it, and what a seek lands on is a
    /// property of the codec, not of the container the MKV fixtures happen to
    /// use.
    #[tokio::test]
    async fn a_long_webm_is_chunked_and_still_carries_its_sidecar() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("video.webm");
        build_webm_source(&input, 6, "libvpx");

        let work_folder = temp.path().join("work");
        std::fs::create_dir_all(&work_folder).unwrap();

        let job = webm_job(&input);

        // The condition `process_job` actually branches on, asserted rather than
        // assumed: a fixture that came in under the threshold would take the
        // one-pass path and pass this test without touching the arm it is here
        // for.
        let source_duration = probed_duration(&input);
        assert!(
            source_duration >= TEST_CHUNKING.min_source_seconds,
            "the fixture runs {source_duration}s, which is under the chunking threshold"
        );
        assert!(
            plan_chunks(source_duration, TEST_CHUNKING.chunk_seconds).len() > 1,
            "and it has to cross a boundary to be worth chunking"
        );

        FFmpegProcessor::new(false)
            .with_chunking(TEST_CHUNKING)
            .process_job(&job, None, &work_folder)
            .await
            .unwrap();

        let output = job.work_folder_output_path(&work_folder);
        assert!(
            output.exists(),
            "the joined file should be in the work folder"
        );

        // The subtitle is the whole reason the job exists, so a file without one
        // is a failure even though FFmpeg exited cleanly.
        assert_eq!(stream_codecs(&output), vec!["h264", "aac", "mov_text"]);

        let output_duration = probed_duration(&output);
        assert!(
            (output_duration - source_duration).abs() < 0.5,
            "joined output runs {output_duration}s against a {source_duration}s source"
        );

        // Both events, at the times the sidecar gives them, rather than one
        // chunk's worth. The list is not compared whole because `mov_text`
        // writes an empty sample across each gap - at the head and between the
        // two events here - and how a muxer fills a gap is not what this is
        // measuring.
        let events = subtitle_events(&subtitle_timings(&output));
        for event in [(500, 1000), (1600, 1400)] {
            assert!(
                events.contains(&event),
                "the sidecar's event at {event:?} is not in the joined output: {events:?}"
            );
        }
        assert_eq!(
            subtitle_texts(&output),
            vec!["first line", "spanning a chunk boundary"],
            "and it is the sidecar's text, not a gap the muxer filled"
        );

        // Nothing is left behind to be mistaken for progress on a later run.
        assert!(!chunk_dir_for(&job.id, &work_folder).exists());
    }
}
