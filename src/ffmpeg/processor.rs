//! `FFmpegProcessor`, which reads a source with FFprobe, chooses how to encode
//! it, and runs FFmpeg.

use anyhow::{anyhow, Result};
use std::path::Path;
use std::process::{Output, Stdio};
use tokio::process::Command;
use tracing::{debug, error, info};
use uuid::Uuid;

use super::operation::{mappings, with_operation};
use super::subtitle_streams::SubtitleSource;
use super::{Chunking, FFmpegCommandBuilder};
use crate::job::{Job, MediaFileType, Operation};

/// What a run has read off the source file, before either encode path is
/// chosen.
///
/// Read once and handed to whichever path runs, because a source over the
/// chunking threshold has to be treated exactly like one under it. Two of
/// these came from probes that are not worth running twice.
pub(super) struct SourceFacts<'a> {
    pub(super) path: &'a Path,
    /// Which of the source's subtitle streams the output carries, or `None`
    /// where it carries none.
    pub(super) subtitles: Option<&'a SubtitleSource>,
    /// How many channels each audio stream carries, in order. Empty when the
    /// operation writes no audio and never needed to ask.
    pub(super) channels: &'a [u32],
}

/// FFmpeg wrapper for media transcoding
pub struct FFmpegProcessor {
    background_mode: bool,
    pub(super) chunking: Chunking,
    /// Tells this worker's staging files apart from every other worker's. See
    /// [`staging_path_for`](super::staging_path_for).
    pub(super) worker_id: Uuid,
    /// The FFprobe program to run. Only a test names another, to make the
    /// probe fail while FFmpeg still works.
    ffprobe: &'static str,
}

impl FFmpegProcessor {
    pub fn new(background_mode: bool) -> Self {
        Self {
            background_mode,
            chunking: Chunking::default(),
            worker_id: Uuid::new_v4(),
            ffprobe: "ffprobe",
        }
    }

    #[cfg(test)]
    pub(super) fn with_ffprobe(mut self, ffprobe: &'static str) -> Self {
        self.ffprobe = ffprobe;
        self
    }

    /// Divide sources differently from the default.
    ///
    /// Exists so a test can exercise a resumed encode on a source a few seconds
    /// long instead of the quarter of an hour the real threshold asks for.
    pub fn with_chunking(mut self, chunking: Chunking) -> Self {
        self.chunking = chunking;
        self
    }

    /// Start an FFmpeg command, de-prioritised if this is a background worker.
    ///
    /// How a process is de-prioritised has no portable spelling. `nice` is a
    /// POSIX utility with nothing by that name on a Windows PATH, so wrapping
    /// the command in it there does not run FFmpeg at low priority - it fails to
    /// spawn at all, and the job comes back around to fail again. Windows
    /// expresses the same idea as a creation flag on the child, which needs no
    /// extra dependency.
    fn ffmpeg_command(&self) -> Command {
        let mut command = self.program("ffmpeg");
        // FFmpeg's build banner runs to a screenful and is reproduced in every
        // error a failed job records. Nothing reads it.
        command.arg("-hide_banner");
        // The level mark on each line is what lets `run` tell the line that
        // names a failure from the layers that repeat it on the way out. Below
        // warnings are the stream dump and encoder statistics, which bury the
        // cause in the part of the error a job file keeps.
        command.args(["-loglevel", "level+warning"]);
        command
    }

    /// Run FFprobe on one file, de-prioritised on the same terms as FFmpeg.
    ///
    /// It goes through [`Self::program`] rather than spawning directly so that
    /// there is one place where a background worker's priority is decided, and
    /// no FFmpeg-family process that quietly bypasses it.
    pub(super) async fn probe(&self, args: &[&str], path: &Path) -> std::io::Result<Output> {
        self.program(self.ffprobe)
            .args(args)
            .arg(path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
    }

    /// The command that runs an FFmpeg-family program, before any arguments.
    fn program(&self, program: &str) -> Command {
        #[cfg(windows)]
        {
            /// `IDLE_PRIORITY_CLASS` from the Windows process creation flags:
            /// the child only runs when nothing else wants the CPU, which is
            /// what `nice -n 19` buys on Unix.
            const IDLE_PRIORITY_CLASS: u32 = 0x0000_0040;

            let mut command = Command::new(program);
            if self.background_mode {
                command.creation_flags(IDLE_PRIORITY_CLASS);
            }
            command
        }

        #[cfg(not(windows))]
        {
            if self.background_mode {
                let mut command = Command::new("nice");
                command.args(["-n", "19", program]);
                command
            } else {
                Command::new(program)
            }
        }
    }

    pub async fn process_job(
        &self,
        job: &Job,
        media_root: Option<&Path>,
        work_folder: Option<&Path>,
    ) -> Result<()> {
        let input_path = job.full_input_path(media_root);
        let output_path = if let Some(work_folder) = work_folder {
            job.work_folder_output_path(work_folder)
        } else {
            job.full_output_path(media_root)
        };

        info!("🚀 Starting conversion for: {:?}", input_path);

        // Ensure input file exists
        if !input_path.exists() {
            return Err(anyhow!("Input file does not exist: {input_path:?}"));
        }

        // Every operation maps the picture, and a queue built by extension
        // alone can hold a file without one. Mapping it optionally instead
        // would deliver a movie with no picture and disable its source, when
        // the source is more likely misfiled or broken. FFprobe also
        // finds no stream in a file it cannot read, which is no more a thing
        // to convert.
        if self.probe_video_stream_count(&input_path).await == Some(0) {
            return Err(anyhow!(
                "FFprobe finds no video stream in {input_path:?}; plexify only converts video"
            ));
        }

        // Create output directory if it doesn't exist
        if let Some(parent) = output_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // A WebM job exists to attach an external subtitle, so a missing one is
        // an error rather than something to encode around. An MKV carries its
        // own subtitle streams, and may carry none.
        //
        // Decided once, here, rather than in each encode path: a source over
        // the chunking threshold must carry the same subtitles as one under it,
        // and the surest way to keep that true is for both to be handed the
        // same answer. `None` is that answer too - an operation that drops the
        // tracks does not probe for them, which is one FFprobe per file saved
        // across a library.
        let subtitles = match (job.operation.keeps_subtitles(), &job.file_type) {
            (false, _) => None,
            (true, MediaFileType::WebM) => {
                let vtt_path = job
                    .full_subtitle_path(media_root)
                    .ok_or_else(|| anyhow!("WebM job missing subtitle path"))?;
                if !vtt_path.exists() {
                    return Err(anyhow!("Required subtitle file not found: {vtt_path:?}"));
                }
                Some(SubtitleSource::External(vtt_path))
            }
            (true, MediaFileType::Mkv | MediaFileType::Avi) => Some(SubtitleSource::Embedded(
                self.select_subtitles(&input_path).await?,
            )),
        };

        // Read before either path is chosen, because both of them write audio
        // and both have to keep it inside what the client will take. Two things
        // come off one probe: where an added track lands, and whether a cap
        // binds at all - `-ac` sets a channel count rather than limiting one,
        // so a cap above the source would invent channels nothing recorded.
        let source_channels = if job.operation.touches_audio() {
            match self.probe_audio_channels(&input_path).await {
                Some(channels) => channels,
                None => {
                    return Err(anyhow!(
                        "FFprobe could not read the audio layout of {input_path:?}, and a track \
                         cannot be made to fit a client without knowing what the source carries"
                    ))
                }
            }
        } else {
            Vec::new()
        };

        let source = SourceFacts {
            path: &input_path,
            subtitles: subtitles.as_ref(),
            channels: &source_channels,
        };

        // A long source is encoded a piece at a time, so that a worker which is
        // interrupted part-way leaves something the next one can carry on from
        // rather than hours of work that has to be done again. That needs
        // somewhere to keep the pieces, and a duration to divide up: without
        // either, fall back to encoding the file in one pass.
        //
        // Only a re-encode is chunked. A remux copies the bitstream and runs at
        // disk speed, so there is no work worth protecting - and the chunk path
        // re-encodes the video, which would throw away the copy that is the
        // whole point of the operation.
        let chunked = match (work_folder, job.operation) {
            (Some(work_folder), Operation::Reencode { .. }) => {
                match self.probe_duration(&input_path).await {
                    Some(duration) if duration >= self.chunking.min_source_seconds => {
                        self.process_in_chunks(job, &source, &output_path, work_folder, duration)
                            .await?;
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        };

        if !chunked {
            self.process_in_one_pass(job, &source, &output_path).await?;
        }

        // Only once the media file itself is written. A sidecar beside an
        // output that never arrived is a subtitle for nothing.
        if job.operation.extracts_subtitles() {
            self.extract_sidecars(&input_path, &output_path).await?;
        }

        Ok(())
    }

    /// Convert the whole source in a single FFmpeg run.
    async fn process_in_one_pass(
        &self,
        job: &Job,
        source: &SourceFacts<'_>,
        output_path: &Path,
    ) -> Result<()> {
        let SourceFacts {
            path: input_path,
            subtitles,
            channels: source_channels,
        } = *source;

        let ffmpeg_builder = with_operation(
            FFmpegCommandBuilder::new().with_generated_pts(),
            job,
            source_channels,
        )
        .with_overwrite()
        .with_negative_composition_offsets()
        .with_input(input_path)
        .with_output(output_path);

        let adds_a_track = job.operation.adds_an_audio_track() && !source_channels.is_empty();

        let (ffmpeg_builder, subtitle_mappings) = match subtitles {
            // A track the target burns into the picture is left out of the
            // codecs and the mappings both. Half of that is not enough: a
            // mapped subtitle stream with no subtitle codec fails the mux.
            None => (ffmpeg_builder, Vec::new()),
            // The subtitle is the whole reason the second input is here, so it
            // is not optional.
            Some(SubtitleSource::External(vtt_path)) => (
                ffmpeg_builder
                    .with_subtitle_encoding()
                    .with_input(vtt_path.as_path()),
                vec!["1:s".to_string()],
            ),
            // Every stream, not the first of each: a second audio track is
            // usually a commentary or another language, and dropping it while
            // renaming the source to `.disabled` loses it for good. The
            // subtitle streams are named one by one because the source may
            // carry one MP4 cannot hold, and `0:s?` would take that one too.
            Some(SubtitleSource::Embedded(selection)) => (
                ffmpeg_builder.with_subtitle_encoding(),
                selection.mappings(0),
            ),
        };
        let ffmpeg_builder =
            ffmpeg_builder.with_stream_mapping(&mappings(0, adds_a_track, &subtitle_mappings));

        self.run(ffmpeg_builder.build(), "conversion").await?;

        info!(
            "✅ Conversion successful: {:?} -> {:?}",
            input_path, output_path
        );
        Ok(())
    }

    /// How many lines FFprobe emits for a file's video streams.
    ///
    /// Not a stream count: `-of csv` gives a transport stream both a
    /// `program,stream,N` line and a `stream,N` line, so one video stream comes
    /// back as 2. Only `Some(0)` is ever consulted - "FFprobe found no video
    /// stream here" - and that reading is sound whatever the count means above
    /// zero. Video rather than any stream, because the concat demuxer needs
    /// every file in its list to declare the same streams. Cover art is not
    /// counted, because it is not mapped: a file whose only picture is its
    /// cover has nothing to convert.
    pub(super) async fn probe_video_stream_count(&self, path: &Path) -> Option<usize> {
        let output = self
            .probe(
                &[
                    "-v",
                    "error",
                    "-select_streams",
                    "V",
                    "-show_entries",
                    "stream=index",
                    "-of",
                    "csv",
                ],
                path,
            )
            .await
            .ok()?;

        Some(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count(),
        )
    }

    /// How many channels each of the source's audio streams carries, in order.
    ///
    /// Two things are read off this. The length is the output index an added
    /// track lands on, and each count is what stops a downmix becoming an
    /// upmix: `-ac` *sets* the channel count rather than capping it, so asking
    /// for six on a stereo source invents four channels that were never
    /// recorded. Only the source can say whether a cap binds.
    async fn probe_audio_channels(&self, path: &Path) -> Option<Vec<u32>> {
        let output = self
            .probe(
                &[
                    "-v",
                    "error",
                    "-select_streams",
                    "a",
                    "-show_entries",
                    "stream=channels",
                    "-of",
                    "csv=p=0",
                ],
                path,
            )
            .await
            .ok()?;

        if !output.status.success() {
            return None;
        }

        // A stream whose channel count FFprobe will not state reads as 0, which
        // is below every cap and so is never capped. Guessing a layout for it
        // is the one thing that could invent channels.
        Some(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(|line| line.parse().unwrap_or(0))
                .collect(),
        )
    }

    /// How long the source runs, as far as FFprobe can tell.
    ///
    /// `None` covers everything from FFprobe not being installed to a container
    /// that does not declare a duration. It is not an error: it only means the
    /// file cannot be divided up, so it is encoded in one pass instead.
    async fn probe_duration(&self, input_path: &Path) -> Option<f64> {
        let output = self
            .probe(
                &[
                    "-v",
                    "error",
                    "-show_entries",
                    "format=duration",
                    "-of",
                    "default=noprint_wrappers=1:nokey=1",
                ],
                input_path,
            )
            .await
            .ok()?;

        if !output.status.success() {
            debug!("FFprobe could not read a duration from {input_path:?}");
            return None;
        }

        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|duration| duration.is_finite() && *duration > 0.0)
    }

    /// Run one FFmpeg invocation, turning a non-zero exit into an error that
    /// names what was being attempted.
    ///
    /// The error ends with the line that names the cause, after what FFmpeg
    /// said: a job file keeps the tail of an error, and a summary shows its
    /// last line.
    pub(super) async fn run(&self, args: Vec<String>, what: &str) -> Result<()> {
        let mut cmd = self.ffmpeg_command();
        cmd.args(&args);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // Tokio leaves a child running when the future holding it is dropped.
        // Ctrl-C cancels this future mid-encode, and the job it belonged to is
        // swept back to the queue a few minutes later - so without this, the
        // worker that picks the job up starts a second FFmpeg writing the same
        // chunk path as the first one, which is still going.
        cmd.kill_on_drop(true);

        debug!("Executing FFmpeg command ({what}): {cmd:?}");

        let output = cmd
            .output()
            .await
            .map_err(|e| anyhow!("Failed to start FFmpeg for {what}: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("FFmpeg failed during {what}: {stderr}");
            let status = output.status.to_string();
            let cause = failure_cause(&stderr).unwrap_or(&status);
            return Err(anyhow!(
                "{}\nFFmpeg {what} failed: {cause}",
                stderr.trim_end()
            ));
        }

        Ok(())
    }
}

/// The line of what FFmpeg said on failing that names the cause.
///
/// FFmpeg reports one failure as a cascade: the component that hit the problem
/// logs it, then every layer the error passes back through logs again. With the
/// level marks [`FFmpegProcessor::ffmpeg_command`] asks for, the cause is the
/// first line marked `[error]` or worse, shown without the marker and the
/// component address before it. Output without the marks says nothing about
/// which line is the cause, so it gets the last one. `None` when FFmpeg said
/// nothing.
fn failure_cause(stderr: &str) -> Option<&str> {
    const ERROR_OR_WORSE: [&str; 3] = ["[panic] ", "[fatal] ", "[error] "];

    let lines = || {
        stderr
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
    };
    lines()
        .find_map(|line| {
            ERROR_OR_WORSE
                .iter()
                .find_map(|level| line.split_once(level))
                .map(|(_, cause)| cause.trim())
        })
        .or_else(|| lines().next_back())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::test_support::*;
    use crate::job::{AudioAction, PostProcessingSettings, QualitySettings, SubtitleAction};
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_ffmpeg_processor_creation() {
        let processor = FFmpegProcessor::new(false);
        assert!(!processor.background_mode);
    }

    #[tokio::test]
    async fn test_background_mode() {
        let processor = FFmpegProcessor::new(true);
        assert!(processor.background_mode);
    }

    /// Where the container says the file begins.
    fn probed_start_time(path: &Path) -> f64 {
        let output = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=start_time",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap_or(0.0)
    }

    /// The presentation timestamps of one stream's packets as a player that
    /// does not read the edit list sees them.
    ///
    /// Edit-list handling is inconsistent across hardware decoders and set-top
    /// clients, and this is what the ones that skip it get: the media timeline
    /// as it was actually written, with nothing rebased on the way out.
    fn packet_times_ignoring_edit_list(path: &Path, stream: &str) -> Vec<f64> {
        let output = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-ignore_editlist",
                "1",
                "-select_streams",
                stream,
                "-show_entries",
                "packet=pts_time",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().trim_end_matches(',').parse().ok())
            .collect()
    }

    /// What the output timeline owes to the source's, on the one-pass path.
    ///
    /// The source's audio is FLAC rather than the AAC every other fixture here
    /// uses, and that is load-bearing rather than incidental. An AAC encoder
    /// emits a priming frame, so an MKV built with one declares a container
    /// `start_time` of -23ms, and FFmpeg 6.1 offsets a whole input forward by
    /// that much to bring it back to zero. Video and audio return to zero
    /// through the filter graph; a transcoded subtitle stream never enters one,
    /// so it keeps the offset and `mov_text` fills the gap it left at the head.
    /// That is FFmpeg's own arithmetic on the way in - measured identically
    /// with the audio track dropped altogether, with ALAC, and with `-c:a
    /// copy`, and unmoved by every muxer option that could plausibly answer it
    /// - so it is not what this test is here to measure, and on a source that
    /// starts at zero it does not arise. FFmpeg 9.0 does not do it at all.
    ///
    /// What the test does measure is unaffected by the choice: the *output*
    /// audio is AAC either way, so the muxer still has a priming frame's
    /// negative timestamp in front of it, and `-avoid_negative_ts make_zero`
    /// still moves the picture to 23ms and still splits the first subtitle
    /// event around the seam. Both assertions below fail against pre-fix code
    /// on FFmpeg 6.1.1 and on 9.0 alike.
    #[tokio::test]
    async fn the_one_pass_encode_starts_the_output_where_the_source_starts() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("clip.mkv");
        // A subtitle on the first frame, which is where a shifted timeline
        // shows itself: the event cannot move without something being invented
        // to fill the gap it left behind.
        build_source_with_audio(
            &input,
            4,
            "1\n00:00:00,000 --> 00:00:01,000\nfirst line\n\n\
             2\n00:00:01,000 --> 00:00:02,000\nsecond line\n\n\
             3\n00:00:02,000 --> 00:00:03,000\nthird line\n",
            "flac",
        );

        // The fixture is only measuring the muxer if the source itself does not
        // begin before zero. Said out loud so that changing its audio codec
        // back fails here, naming the reason, rather than in an assertion that
        // looks like the fix regressed.
        let source_start = probed_start_time(&input);
        assert!(
            source_start.abs() < 0.001,
            "the source starts at {source_start}s, so FFmpeg's own input offset is in the \
             measurement as well as the muxer's"
        );

        let work_folder = temp.path().join("work");
        std::fs::create_dir_all(&work_folder).unwrap();

        let job = chunking_job(&input);
        FFmpegProcessor::new(false)
            .with_chunking(Chunking {
                chunk_seconds: 2.0,
                // Above the source length, so this is the one-pass path.
                min_source_seconds: 60.0,
            })
            .process_job(&job, None, Some(&work_folder))
            .await
            .unwrap();

        let output = job.work_folder_output_path(&work_folder);

        // The AAC encoder's priming frame carries a negative timestamp, and MP4
        // has an edit list to say so. Shifting the whole output forward to keep
        // it off zero instead moves the picture one AAC frame - 21ms at 48kHz -
        // later than the source it came from.
        let first_frame = packet_times(&output, "v")
            .first()
            .copied()
            .expect("the output should carry video");
        assert!(
            first_frame < 0.005,
            "the first picture is at {first_frame}s, not at the start of the file"
        );

        // And the first subtitle event is the one that cannot survive the
        // shift: moving it off zero leaves a gap that mov_text fills with a
        // sliver of an event, which shows as a flicker of the first line.
        let events = subtitle_events(&subtitle_timings(&output));
        assert_eq!(
            events.first(),
            Some(&(0, 1000)),
            "the first subtitle should be the source's own first event: {events:?}"
        );
        for (start, duration) in &events {
            assert!(
                *duration > 100,
                "a {duration}ms subtitle at {start}ms is the seam a shifted timeline leaves \
                 behind, not an event the source has: {events:?}"
            );
        }
    }

    /// A job whose preset reorders frames, which is what puts a reorder delay
    /// in front of the muxer at all.
    ///
    /// `chunking_job` asks for `ultrafast`, and x264 turns B-frames off there,
    /// so a fixture built on it has no delay to express and would pass this
    /// whatever the muxer was told. `veryfast` is what the project actually
    /// ships, and it reorders.
    fn reordering_job(input: &Path) -> Job {
        Job::new(
            input.to_path_buf(),
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
            QualitySettings {
                ffmpeg_preset: "veryfast".to_string(),
                ffmpeg_crf: "30".to_string(),
                ffmpeg_audio_bitrate: "64k".to_string(),
            },
            PostProcessingSettings::default(),
            input.parent().unwrap(),
        )
    }

    /// The picture has to be in the media timeline, not only in the edit list.
    ///
    /// x264 emits the first picture with a decode timestamp below its
    /// presentation timestamp, and the MP4 muxer's default answer is the video
    /// track's `elst` `media_time`. A player that skips the edit list - and
    /// hardware decoders and set-top clients are inconsistent about it - then
    /// plays the picture `has_b_frames` frame intervals behind the sound: 200ms
    /// on this 10fps fixture, 80ms at 25fps.
    ///
    /// Both encode paths are measured, because the delay reaches the muxer by
    /// two different routes. The one-pass encode hands it over from x264
    /// directly; the join is fed by the concat demuxer, whose first video DTS
    /// is a reorder delay before its PTS, so a join that assumed its input was
    /// already non-negative would leave this in place on exactly the files that
    /// are long enough to matter.
    ///
    /// The second half of each check is what says the fix is not a trade: the
    /// edit-list-honouring reading has to be unchanged, or a player that was
    /// right before has been broken to fix one that was wrong.
    #[tokio::test]
    async fn the_picture_is_in_the_timeline_and_not_only_in_the_edit_list() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("film.mkv");
        build_chunking_source(&input, 6);

        for (path, chunking) in [
            (
                "one-pass",
                Chunking {
                    chunk_seconds: 2.0,
                    // Above the source length, so the whole file is one run.
                    min_source_seconds: 60.0,
                },
            ),
            ("chunked", TEST_CHUNKING),
        ] {
            let work_folder = temp.path().join(format!("work-{path}"));
            std::fs::create_dir_all(&work_folder).unwrap();

            let job = reordering_job(&input);
            FFmpegProcessor::new(false)
                .with_chunking(chunking)
                .process_job(&job, None, Some(&work_folder))
                .await
                .unwrap();

            let output = job.work_folder_output_path(&work_folder);

            let raw_picture = *packet_times_ignoring_edit_list(&output, "v")
                .first()
                .expect("the output should carry video");
            let raw_sound = *packet_times_ignoring_edit_list(&output, "a")
                .first()
                .expect("the output should carry audio");
            assert!(
                (raw_picture - raw_sound).abs() < 0.02,
                "on the {path} output a player ignoring the edit list starts the picture at \
                 {raw_picture}s and the sound at {raw_sound}s"
            );

            // And the players that were already right are still right: the
            // presentation timeline the edit list describes is untouched.
            let picture = *packet_times(&output, "v")
                .first()
                .expect("the output should carry video");
            assert!(
                picture.abs() < 0.005,
                "the {path} output's first picture is at {picture}s, not at the start of the file"
            );
        }
    }

    /// A queue built by extension holds whatever has a video file's name. A
    /// source with no picture is refused for that, before FFmpeg is asked to
    /// map a stream it does not have, so the reason a parked job gives is the
    /// one somebody can act on.
    #[tokio::test]
    async fn a_source_with_no_video_stream_is_refused_for_that() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("sound.mkv");
        let built = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i", "sine=duration=2"])
            .args(["-c:a", "aac"])
            .arg(&input)
            .status()
            .unwrap();
        assert!(built.success(), "could not build the audio-only source");

        let work_folder = temp.path().join("work");
        std::fs::create_dir_all(&work_folder).unwrap();

        for operation in [
            Operation::Reencode { channels: None },
            Operation::Remux {
                audio: AudioAction::Transcode { channels: None },
                subtitles: SubtitleAction::Keep,
            },
        ] {
            let job = Job {
                operation,
                ..chunking_job(&input)
            };

            let error = FFmpegProcessor::new(false)
                .process_job(&job, None, Some(&work_folder))
                .await
                .expect_err("a source with no picture was converted");

            assert!(
                format!("{error:#}").contains("plexify only converts video"),
                "{operation:?}: {error:#}"
            );
        }
    }

    /// Build an MKV of `sources` with a JPEG attached the way MKV carries
    /// cover art. FFmpeg reads the attachment back as a video stream marked
    /// `attached_pic`.
    fn build_with_cover_art(dir: &Path, sources: &[&str], output: &Path) {
        let cover = dir.join("cover.jpg");
        let built = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i", "testsrc=size=64x64"])
            .args(["-frames:v", "1", "-y"])
            .arg(&cover)
            .status()
            .unwrap();
        assert!(built.success(), "could not build the cover");

        let mut command = std::process::Command::new("ffmpeg");
        command.args(["-v", "error"]);
        for source in sources {
            command.args(["-f", "lavfi", "-i", source]);
        }
        let built = command
            .args(["-c:v", "libx264", "-c:a", "aac", "-attach"])
            .arg(&cover)
            .args(["-metadata:s:t", "mimetype=image/jpeg", "-y"])
            .arg(output)
            .status()
            .unwrap();
        assert!(built.success(), "could not build the source with cover art");
    }

    /// Cover art is a picture of the film, not the film. Mapped as video, a
    /// re-encode turns it into an H.264 track the MP4 muxer refuses, and a
    /// remux delivers it as a second video track. Plex shows its own poster,
    /// so it is left out.
    #[tokio::test]
    async fn cover_art_is_left_out_of_the_output() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("film.mkv");
        build_with_cover_art(
            temp.path(),
            &["testsrc=duration=6:size=160x120:rate=10", "sine=duration=6"],
            &input,
        );

        let remux = Operation::Remux {
            audio: AudioAction::Copy,
            subtitles: SubtitleAction::Keep,
        };
        let reencode = Operation::Reencode { channels: None };
        for (operation, chunking) in [
            (remux, Chunking::default()),
            (reencode, Chunking::default()),
            (reencode, TEST_CHUNKING),
        ] {
            let work_folder = TempDir::new().unwrap();
            let job = Job {
                operation,
                ..chunking_job(&input)
            };

            FFmpegProcessor::new(false)
                .with_chunking(chunking)
                .process_job(&job, None, Some(work_folder.path()))
                .await
                .unwrap_or_else(|error| panic!("{operation:?}, {chunking:?}: {error:#}"));

            let kinds: Vec<String> = streams_of(&job.work_folder_output_path(work_folder.path()))
                .iter()
                .map(|stream| stream.split(',').next().unwrap().to_string())
                .collect();
            assert_eq!(kinds, ["video", "audio"], "{operation:?}, {chunking:?}");
        }
    }

    /// A file whose only picture is its cover art is as audio-only as one with
    /// none, and is refused for the same reason.
    #[tokio::test]
    async fn a_source_whose_only_picture_is_cover_art_is_refused() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("album.mkv");
        build_with_cover_art(temp.path(), &["sine=duration=2"], &input);

        let error = FFmpegProcessor::new(false)
            .process_job(&chunking_job(&input), None, Some(temp.path()))
            .await
            .expect_err("a source with only cover art was converted");

        assert!(
            format!("{error:#}").contains("plexify only converts video"),
            "{error:#}"
        );
    }

    fn transcode(input: &Path) -> PathBuf {
        let job = Job::new(
            input.to_path_buf(),
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
            QualitySettings::default(),
            PostProcessingSettings::default(),
            input.parent().expect("input has a parent"),
        );

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime
            .block_on(FFmpegProcessor::new(false).process_job(&job, None, None))
            .expect("transcode should succeed");

        job.output_path
    }

    #[test]
    fn keeps_every_audio_track_and_its_language() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("multi.mkv");

        let built = std::process::Command::new("ffmpeg")
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=160x120:rate=5",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=880:duration=1",
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "2:a",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
                "-metadata:s:a:0",
                "language=eng",
                "-metadata:s:a:1",
                "language=fra",
                "-y",
            ])
            .arg(&input)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("ffmpeg should run");
        assert!(built.success(), "could not build the test input");

        let output = transcode(&input);

        assert_eq!(
            streams_of(&output),
            vec!["video,und", "audio,eng", "audio,fra"],
            "a second audio track is usually another language, and the source is              disabled straight afterwards - losing it here loses it for good"
        );
    }

    #[test]
    fn transcodes_a_file_that_has_no_subtitles() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("plain.mkv");

        let built = std::process::Command::new("ffmpeg")
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=160x120:rate=5",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=1",
                "-c:v",
                "libx264",
                "-c:a",
                "aac",
                "-y",
            ])
            .arg(&input)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("ffmpeg should run");
        assert!(built.success(), "could not build the test input");

        let output = transcode(&input);

        assert_eq!(
            streams_of(&output),
            vec!["video,und", "audio,und"],
            "the subtitle mapping is optional, so a file without one still converts"
        );
    }

    /// A failed run ends its error with the line that names the cause, which
    /// is what a parked job's summary shows.
    ///
    /// Copying generated frames into an MP4 is refused by the muxer, and the
    /// refusal is followed by the layers that repeat it, so the cause is
    /// neither the first line of what FFmpeg said nor, without the level
    /// marks `ffmpeg_command` asks for, the last.
    #[tokio::test]
    async fn a_failed_run_ends_its_error_with_the_cause() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let output = temp.path().join("out.mp4");
        let args = ["-f", "lavfi", "-i", "testsrc=duration=1", "-c:v", "copy"]
            .iter()
            .map(|arg| arg.to_string())
            .chain([output.to_string_lossy().into_owned()])
            .collect();

        let error = FFmpegProcessor::new(false)
            .run(args, "conversion")
            .await
            .expect_err("an MP4 cannot hold uncoded frames");
        let error = format!("{error:#}");
        let last = error.lines().next_back().unwrap_or_default();

        assert!(
            last.starts_with("FFmpeg conversion failed: Could not find tag for codec")
                && last.ends_with("codec not currently supported in container"),
            "{error}"
        );
    }

    #[test]
    fn the_cause_is_the_first_line_ffmpeg_marked_as_an_error() {
        // What FFmpeg 9 printed for an option value it could not parse.
        let stderr = "\
[libx264 @ 000001f1ef706280] [Eval @ 00000030f3dfda60] [error] Undefined constant or missing '(' in 'banana'
[libx264 @ 000001f1ef706280] [error] Unable to parse \"crf\" option value \"banana\"
[vost#0:0/libx264 @ 000001f1ef702000] [error] Error applying encoder options: Invalid argument
[error] Error opening output file c.mp4.
[fatal] Error opening output files: Invalid argument
";
        assert_eq!(
            failure_cause(&format!("[in#0 @ 0x1] [warning] Format detected\n{stderr}")),
            Some("Undefined constant or missing '(' in 'banana'")
        );
    }

    #[test]
    fn output_without_level_marks_is_summarised_by_its_last_line() {
        assert_eq!(
            failure_cause("Could not write header\nConversion failed!\n\n"),
            Some("Conversion failed!")
        );
        assert_eq!(failure_cause("\n"), None);
    }
}
