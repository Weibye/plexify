use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, error, info, warn};

use crate::job::{Job, MediaFileType, QualitySettings};
use crate::paths::to_forward_slashes;

/// Builder for constructing FFmpeg commands with a fluent API.
///
/// FFmpeg's command line is positional: an option applies to the next file that
/// follows it, and the output file must come last. Options after the output are
/// silently discarded, which is a quiet way to lose a stream mapping.
///
/// So the builder does not append to one list. It assembles the command in
/// FFmpeg's order at `build` time:
///
/// ```text
/// {global} ({options for input n} -i {input n})... {output options} {output}
/// ```
///
/// Output options can be chained in whatever order reads best. Input options
/// attach to the **next input declared**, which is what lets a command carry a
/// different option for each of its inputs - `-fix_sub_duration` on the source
/// a join takes subtitles from, but not on the list of chunks it copies. An
/// earlier version kept every input option in one bucket ahead of every input,
/// which silently put them all on input 0.
#[derive(Debug, Default)]
pub struct FFmpegCommandBuilder {
    global: Vec<String>,
    /// Options waiting for the input they belong to.
    pending_input_options: Vec<String>,
    inputs: Vec<Input>,
    output_options: Vec<String>,
    output: Option<String>,
}

/// One input file and the options that apply to it.
#[derive(Debug)]
struct Input {
    options: Vec<String>,
    path: String,
}

impl FFmpegCommandBuilder {
    /// Create a new FFmpeg command builder
    pub fn new() -> Self {
        Self::default()
    }

    /// Add common FFmpeg flags for media processing.
    ///
    /// `+genpts` is a demuxer flag and belongs to the input; `avoid_negative_ts`
    /// is a muxer option and belongs to the output.
    pub fn with_common_flags(self) -> Self {
        self.with_generated_pts().with_negative_timestamp_fix()
    }

    /// Have the demuxer generate presentation timestamps for the next input.
    pub fn with_generated_pts(self) -> Self {
        self.with_input_options(&["-fflags", "+genpts"])
    }

    /// Shift the output so no timestamp is negative.
    pub fn with_negative_timestamp_fix(mut self) -> Self {
        self.output_options
            .extend_from_slice(&["-avoid_negative_ts".to_string(), "make_zero".to_string()]);
        self
    }

    /// Trim subtitle events in the next input so they do not overlap.
    ///
    /// Belongs to the input the subtitles are decoded from. Without it a
    /// subtitle keeps whatever duration the source declares, which for an
    /// overlapping event means `mov_text` gets something it handles badly.
    pub fn with_subtitle_duration_fix(self) -> Self {
        self.with_input_options(&["-fix_sub_duration"])
    }

    /// Hold options for the next input declared.
    pub fn with_input_options(mut self, options: &[&str]) -> Self {
        self.pending_input_options
            .extend(options.iter().map(|option| option.to_string()));
        self
    }

    /// Add a single input file, taking whatever options are waiting for it.
    pub fn with_input<P: AsRef<Path>>(mut self, input_path: P) -> Self {
        self.inputs.push(Input {
            options: std::mem::take(&mut self.pending_input_options),
            path: input_path.as_ref().to_string_lossy().to_string(),
        });
        self
    }

    /// Add multiple input files
    pub fn with_inputs<P: AsRef<Path>>(mut self, input_paths: &[P]) -> Self {
        for input_path in input_paths {
            self = self.with_input(input_path);
        }
        self
    }

    /// Add stream mapping arguments
    pub fn with_stream_mapping(mut self, mappings: &[&str]) -> Self {
        for mapping in mappings {
            self.output_options.push("-map".to_string());
            self.output_options.push(mapping.to_string());
        }
        self
    }

    /// Add video encoding settings using H.264 with configurable preset and CRF
    pub fn with_video_encoding(mut self, quality_settings: &QualitySettings) -> Self {
        self.output_options.extend_from_slice(&[
            "-c:v".to_string(),
            "libx264".to_string(),
            "-preset".to_string(),
            quality_settings.ffmpeg_preset.clone(),
            "-crf".to_string(),
            quality_settings.ffmpeg_crf.clone(),
        ]);
        self
    }

    /// Add audio encoding settings using AAC with configurable bitrate
    pub fn with_audio_encoding(mut self, quality_settings: &QualitySettings) -> Self {
        self.output_options.extend_from_slice(&[
            "-c:a".to_string(),
            "aac".to_string(),
            "-b:a".to_string(),
            quality_settings.ffmpeg_audio_bitrate.clone(),
        ]);
        self
    }

    /// Add subtitle encoding settings using mov_text format for MP4 containers
    pub fn with_subtitle_encoding(mut self) -> Self {
        self.output_options
            .extend_from_slice(&["-c:s".to_string(), "mov_text".to_string()]);
        self
    }

    /// Enable output file overwriting
    pub fn with_overwrite(mut self) -> Self {
        self.global.push("-y".to_string());
        self
    }

    /// Start reading the input this far in.
    ///
    /// An input option, so it seeks the file rather than trimming the output,
    /// which is what makes a chunk cost only its own length to produce.
    pub fn with_seek(self, seconds: f64) -> Self {
        let seconds = format_seconds(seconds);
        self.with_input_options(&["-ss", &seconds])
    }

    /// Stop after this much output.
    pub fn with_duration(mut self, seconds: f64) -> Self {
        self.output_options.push("-t".to_string());
        self.output_options.push(format_seconds(seconds));
        self
    }

    /// Read the inputs listed in a concat list file.
    ///
    /// `-safe 0` because the list holds absolute paths, which the demuxer
    /// refuses by default. Both options attach to this input alone, so a later
    /// input is still read as an ordinary file.
    pub fn with_concat_list<P: AsRef<Path>>(self, list_path: P) -> Self {
        self.with_input_options(&["-f", "concat", "-safe", "0"])
            .with_input(list_path)
    }

    /// Write an MPEG-TS stream rather than guessing the format from the name.
    ///
    /// Chunks are written as transport streams because that is the container
    /// designed to be concatenated: timestamps are explicit and continuous
    /// across a join, where stitching separately encoded MP4s leaves each part
    /// carrying its own encoder delay and the audio drifting a little further
    /// out of sync at every boundary.
    pub fn with_transport_stream_output(mut self) -> Self {
        self.output_options.extend_from_slice(&[
            "-muxdelay".to_string(),
            "0".to_string(),
            "-muxpreload".to_string(),
            "0".to_string(),
            "-f".to_string(),
            "mpegts".to_string(),
        ]);
        self
    }

    /// Copy video and audio through untouched.
    pub fn with_video_and_audio_copy(mut self) -> Self {
        self.output_options.extend_from_slice(&[
            "-c:v".to_string(),
            "copy".to_string(),
            "-c:a".to_string(),
            "copy".to_string(),
        ]);
        self
    }

    /// Add the output file path
    pub fn with_output<P: AsRef<Path>>(mut self, output_path: P) -> Self {
        self.output = Some(output_path.as_ref().to_string_lossy().to_string());
        self
    }

    /// Build the final command arguments as a vector of strings
    pub fn build(self) -> Vec<String> {
        let mut args = self.global;

        for input in self.inputs {
            args.extend(input.options);
            args.push("-i".to_string());
            args.push(input.path);
        }

        // Options nothing claimed. A builder used for flags alone, with no
        // input at all, still has to emit them.
        args.extend(self.pending_input_options);

        args.extend(self.output_options);
        args.extend(self.output);
        args
    }

    /// Build the command arguments and apply them to a tokio Command
    pub fn build_command(self, base_command: &mut Command) {
        base_command.args(self.build());
    }
}

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
pub fn chunk_dir_for(job: &Job, work_folder: &Path) -> PathBuf {
    work_folder.join(format!("{}.chunks", job.id))
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

/// Render a number of seconds for an FFmpeg time argument.
fn format_seconds(seconds: f64) -> String {
    format!("{seconds:.3}")
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

/// FFmpeg wrapper for media transcoding
pub struct FFmpegProcessor {
    background_mode: bool,
    chunking: Chunking,
}

impl FFmpegProcessor {
    pub fn new(background_mode: bool) -> Self {
        Self {
            background_mode,
            chunking: Chunking::default(),
        }
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
        command
    }

    /// Start an FFprobe command, de-prioritised on the same terms as FFmpeg.
    ///
    /// It goes through [`Self::program`] rather than spawning directly so that
    /// there is one place where a background worker's priority is decided, and
    /// no FFmpeg-family process that quietly bypasses it.
    fn probe_command(&self) -> Command {
        self.program("ffprobe")
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

        // Create output directory if it doesn't exist
        if let Some(parent) = output_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // A WebM job exists to attach an external subtitle, so a missing one is
        // an error rather than something to encode around. An MKV carries its
        // own subtitle streams, and may carry none.
        let external_subtitle = match job.file_type {
            MediaFileType::WebM => {
                let vtt_path = job
                    .full_subtitle_path(media_root)
                    .ok_or_else(|| anyhow!("WebM job missing subtitle path"))?;
                if !vtt_path.exists() {
                    return Err(anyhow!("Required subtitle file not found: {vtt_path:?}"));
                }
                Some(vtt_path)
            }
            MediaFileType::Mkv => None,
        };

        // A long source is encoded a piece at a time, so that a worker which is
        // interrupted part-way leaves something the next one can carry on from
        // rather than hours of work that has to be done again. That needs
        // somewhere to keep the pieces, and a duration to divide up: without
        // either, fall back to encoding the file in one pass.
        if let Some(work_folder) = work_folder {
            if let Some(duration) = self.probe_duration(&input_path).await {
                if duration >= self.chunking.min_source_seconds {
                    return self
                        .process_in_chunks(
                            job,
                            &input_path,
                            external_subtitle.as_deref(),
                            &output_path,
                            work_folder,
                            duration,
                        )
                        .await;
                }
            }
        }

        self.process_in_one_pass(job, &input_path, external_subtitle.as_deref(), &output_path)
            .await
    }

    /// Encode the whole source in a single FFmpeg run.
    async fn process_in_one_pass(
        &self,
        job: &Job,
        input_path: &Path,
        external_subtitle: Option<&Path>,
        output_path: &Path,
    ) -> Result<()> {
        let ffmpeg_builder = FFmpegCommandBuilder::new()
            .with_common_flags()
            .with_video_encoding(&job.quality_settings)
            .with_audio_encoding(&job.quality_settings)
            .with_subtitle_encoding()
            .with_overwrite()
            .with_output(output_path);

        // Add format-specific flags, inputs, and mappings
        let ffmpeg_builder = match external_subtitle {
            // The subtitle is the whole reason the second input is here, so it
            // is not optional.
            Some(vtt_path) => ffmpeg_builder
                .with_inputs(&[input_path, vtt_path])
                .with_stream_mapping(&["0:v", "0:a", "1:s"]),
            // Every stream, not the first of each: a second audio track is
            // usually a commentary or another language, and dropping it while
            // renaming the source to `.disabled` loses it for good. Subtitles
            // are optional so a file without any still transcodes.
            None => ffmpeg_builder
                .with_subtitle_duration_fix()
                .with_input(input_path)
                .with_stream_mapping(&["0:v", "0:a", "0:s?"]),
        };

        self.run(ffmpeg_builder.build(), "conversion").await?;

        info!(
            "✅ Conversion successful: {:?} -> {:?}",
            input_path, output_path
        );
        Ok(())
    }

    /// Encode the source in chunks, reusing any chunk an earlier attempt
    /// already finished.
    ///
    /// A chunk is written under a `.part` name and only given its final name
    /// once FFmpeg has exited successfully, so the presence of a chunk file is
    /// what says the work behind it is sound. Nothing else has to be recorded:
    /// the chunk directory *is* the progress, which is the same trade the job
    /// queue makes.
    ///
    /// Subtitles are deliberately left out of the chunks and muxed in at the
    /// end, straight from the source. A subtitle event that straddles a chunk
    /// boundary would otherwise be cut in half by it.
    async fn process_in_chunks(
        &self,
        job: &Job,
        input_path: &Path,
        external_subtitle: Option<&Path>,
        output_path: &Path,
        work_folder: &Path,
        duration: f64,
    ) -> Result<()> {
        let chunk_dir = chunk_dir_for(job, work_folder);
        self.prepare_chunk_dir(job, &chunk_dir).await?;

        let planned = plan_chunks(duration, self.chunking.chunk_seconds);
        let chunks = self
            .encode_chunks(job, input_path, &chunk_dir, &planned)
            .await?;

        let list_path = chunk_dir.join("chunks.txt");
        let last = chunks.len() - 1;
        let list = chunks
            .iter()
            .enumerate()
            .map(|(position, chunk)| {
                let declared = chunk.duration.filter(|_| position != last);
                concat_list_entry(chunk, &chunk_dir, declared)
            })
            .collect::<String>();
        tokio::fs::write(&list_path, list.as_bytes()).await?;

        // Joining the chunks copies the streams rather than touching them
        // again; only the subtitles, which were held back, are encoded here.
        let mux_builder = FFmpegCommandBuilder::new()
            .with_overwrite()
            .with_negative_timestamp_fix()
            .with_concat_list(&list_path)
            .with_video_and_audio_copy()
            .with_subtitle_encoding()
            .with_output(output_path);

        let mux_builder = match external_subtitle {
            // A `.vtt` carries its own durations; there is nothing to trim.
            Some(vtt_path) => mux_builder
                .with_input(vtt_path)
                .with_stream_mapping(&["0:v", "0:a", "1:s"]),
            // The same treatment the one-pass encode gives an MKV's embedded
            // subtitles, and it has to land on *this* input rather than on the
            // chunk list - which is what the builder's per-input options are
            // for. Without it a long source would convert its subtitles
            // differently from a short one.
            None => mux_builder
                .with_subtitle_duration_fix()
                .with_input(input_path)
                .with_stream_mapping(&["0:v", "0:a", "1:s?"]),
        };

        self.run(mux_builder.build(), "joining chunks").await?;

        // The chunks have served their purpose, and are the size of the output
        // again. Anything left here would be re-used by a later run.
        tokio::fs::remove_dir_all(&chunk_dir).await?;

        info!(
            "✅ Conversion successful: {:?} -> {:?}",
            input_path, output_path
        );
        Ok(())
    }

    /// Encode every chunk that is not already on disk.
    ///
    /// A chunk is written under a partial name and only renamed once FFmpeg has
    /// exited successfully, so a finished chunk file existing is what says the
    /// work behind it is sound and can be skipped. That rename is the whole
    /// resume mechanism - nothing else is written down.
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

            let mut builder = FFmpegCommandBuilder::new()
                .with_common_flags()
                .with_overwrite()
                .with_seek(chunk.start)
                .with_input(input_path)
                .with_stream_mapping(&["0:v", "0:a"])
                .with_video_encoding(&job.quality_settings)
                .with_audio_encoding(&job.quality_settings)
                .with_transport_stream_output()
                .with_output(&partial_path);

            if let Some(chunk_duration) = chunk.duration {
                builder = builder.with_duration(chunk_duration);
            }

            self.run(
                builder.build(),
                &format!("chunk {} of {total}", chunk.index + 1),
            )
            .await?;

            // The plan came from a probed duration, and a container can
            // over-report one. A chunk whose seek landed past the end of the
            // source encodes happily and produces nothing; joined in, it would
            // be swallowed without complaint. Treat it as the end of the file.
            if self.probe_stream_count(&partial_path).await == Some(0) {
                let _ = tokio::fs::remove_file(&partial_path).await;
                debug!(
                    "Chunk {} starts past the end of the source; the file is shorter than its \
                     container claims",
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
            return Err(anyhow!("FFmpeg produced no output for {input_path:?}"));
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
                info!("🗑️ Discarding chunks encoded with different settings: {chunk_dir:?}");
                tokio::fs::remove_dir_all(chunk_dir).await?;
            }
        }

        tokio::fs::create_dir_all(chunk_dir).await?;
        tokio::fs::write(&stamp_path, stamp.as_bytes()).await?;
        Ok(())
    }

    /// How many streams a file has, as far as FFprobe can tell.
    async fn probe_stream_count(&self, path: &Path) -> Option<usize> {
        let output = self
            .probe_command()
            .args(["-v", "error", "-show_entries", "stream=index", "-of", "csv"])
            .arg(path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .ok()?;

        Some(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count(),
        )
    }

    /// How long the source runs, as far as FFprobe can tell.
    ///
    /// `None` covers everything from FFprobe not being installed to a container
    /// that does not declare a duration. It is not an error: it only means the
    /// file cannot be divided up, so it is encoded in one pass instead.
    async fn probe_duration(&self, input_path: &Path) -> Option<f64> {
        let output = self
            .probe_command()
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(input_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
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
    async fn run(&self, args: Vec<String>, what: &str) -> Result<()> {
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
            return Err(anyhow!("FFmpeg {what} failed: {stderr}"));
        }

        Ok(())
    }

    /// Throw away everything a job left in the work folder.
    ///
    /// Called when a job is parked in `_failed`, because nothing will ever come
    /// back for it: the job id is derived from the input path, so `job_exists`
    /// sees the parked job and no re-scan will queue that file again. Its
    /// chunks would otherwise sit in `_in_progress` at roughly the size of the
    /// finished output, with nothing pointing at them.
    pub async fn discard_work(&self, job: &Job, work_folder: &Path) {
        self.discard_work_for_id(&job.id, work_folder).await;
    }

    /// The same, for a job known only by its id.
    ///
    /// The startup sweep parks jobs too - one that keeps taking its worker down
    /// ends up in `_failed` the same way one that keeps failing does - and it
    /// has only the job filename to go on. Everything a job leaves in the work
    /// folder is named after its id, so that is enough.
    pub async fn discard_work_for_id(&self, job_id: &str, work_folder: &Path) {
        let chunk_dir = work_folder.join(format!("{job_id}.chunks"));
        if chunk_dir.exists() {
            match tokio::fs::remove_dir_all(&chunk_dir).await {
                Ok(_) => info!("🗑️ Discarded the chunks of a parked job: {chunk_dir:?}"),
                Err(e) => warn!("Could not remove {chunk_dir:?}: {e}"),
            }
        }

        // A part-written output from before this file was chunked, or from the
        // join that was interrupted. Named `{id}_{filename}`, so the filename
        // this job would have used is not needed to find it.
        let prefix = format!("{job_id}_");
        let Ok(mut entries) = tokio::fs::read_dir(work_folder).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
    }

    /// Move completed file from work folder to media folder
    pub async fn move_to_destination(
        &self,
        job: &Job,
        media_root: Option<&Path>,
        work_folder: &Path,
    ) -> Result<()> {
        let work_output_path = job.work_folder_output_path(work_folder);
        let final_output_path = job.full_output_path(media_root);

        // Ensure the work folder output file exists
        if !work_output_path.exists() {
            return Err(anyhow!(
                "Work folder output file does not exist: {work_output_path:?}"
            ));
        }

        // Create final output directory if it doesn't exist
        if let Some(parent) = final_output_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Move the file from work folder to final location
        tokio::fs::copy(&work_output_path, &final_output_path).await?;
        tokio::fs::remove_file(&work_output_path).await?;

        info!(
            "📁 Moved completed file: {:?} -> {:?}",
            work_output_path, final_output_path
        );

        Ok(())
    }
    pub async fn disable_source_files(&self, job: &Job, media_root: Option<&Path>) -> Result<()> {
        let input_path = job.full_input_path(media_root);
        let disabled_input = input_path.with_extension(format!(
            "{}.disabled",
            input_path
                .extension()
                .unwrap_or_default()
                .to_str()
                .unwrap_or("")
        ));

        // Rename input file
        tokio::fs::rename(&input_path, &disabled_input).await?;
        debug!(
            "Renamed input file: {:?} -> {:?}",
            input_path, disabled_input
        );

        // Rename subtitle file if it exists (WebM)
        if let Some(vtt_path) = job.full_subtitle_path(media_root) {
            if vtt_path.exists() {
                let disabled_vtt = vtt_path.with_extension("vtt.disabled");
                tokio::fs::rename(&vtt_path, &disabled_vtt).await?;
                debug!(
                    "Renamed subtitle file: {:?} -> {:?}",
                    vtt_path, disabled_vtt
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{Job, MediaFileType, PostProcessingSettings, QualitySettings};
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn a_list_cut_short_leaves_its_final_entry_open_ended() {
        // The probe over-reported, the chunk past the end came back empty, and
        // the entry that is now last may hold less than the plan asked for.
        // Declaring a length for it would hold the timeline open past the
        // content and freeze the end of the film.
        let chunks = plan_chunks(1000.0, 300.0);
        let kept = &chunks[..2];
        let last = kept.len() - 1;

        let list: String = kept
            .iter()
            .enumerate()
            .map(|(position, chunk)| {
                let declared = chunk.duration.filter(|_| position != last);
                concat_list_entry(chunk, Path::new("/work/x.chunks"), declared)
            })
            .collect();

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
        let last = chunks.len() - 1;
        let list: String = chunks
            .iter()
            .enumerate()
            .map(|(position, chunk)| {
                let declared = chunk.duration.filter(|_| position != last);
                concat_list_entry(chunk, Path::new("/work/x.chunks"), declared)
            })
            .collect();

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

        let args = FFmpegCommandBuilder::new()
            .with_common_flags()
            .with_overwrite()
            .with_seek(600.0)
            .with_input("/media/film.mkv")
            .with_stream_mapping(&["0:v", "0:a"])
            .with_video_encoding(&quality)
            .with_audio_encoding(&quality)
            .with_transport_stream_output()
            .with_output("/work/00002.ts.part")
            .with_duration(300.0)
            .build();

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
    }

    #[test]
    fn the_join_copies_the_chunks_and_takes_subtitles_from_the_source() {
        let args = FFmpegCommandBuilder::new()
            .with_overwrite()
            .with_concat_list("/work/chunks.txt")
            .with_video_and_audio_copy()
            .with_subtitle_encoding()
            .with_output("/work/film.mp4")
            .with_input("/media/film.mkv")
            .with_stream_mapping(&["0:v", "0:a", "1:s?"])
            .build();

        let joined = args.join(" ");

        // `-f concat` applies to the input that follows it, so the chunk list
        // has to be input 0 and the source input 1.
        assert!(
            joined.contains("-f concat -safe 0 -i /work/chunks.txt -i /media/film.mkv"),
            "{joined}"
        );
        assert!(joined.contains("-c:v copy -c:a copy"));
        assert!(joined.contains("-c:s mov_text"));
        assert!(joined.ends_with("/work/film.mp4"));
    }

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

    #[test]
    fn test_ffmpeg_command_builder_webm() {
        let quality = QualitySettings {
            ffmpeg_preset: "fast".to_string(),
            ffmpeg_crf: "20".to_string(),
            ffmpeg_audio_bitrate: "192k".to_string(),
        };

        let args = FFmpegCommandBuilder::new()
            .with_common_flags()
            .with_inputs(&["/path/to/video.webm", "/path/to/video.vtt"])
            .with_stream_mapping(&["0:v:0", "0:a:0", "1:s:0"])
            .with_video_encoding(&quality)
            .with_audio_encoding(&quality)
            .with_subtitle_encoding()
            .with_overwrite()
            .with_output("/path/to/output.mp4")
            .build();

        let expected = vec![
            "-y",
            "-fflags",
            "+genpts",
            "-i",
            "/path/to/video.webm",
            "-i",
            "/path/to/video.vtt",
            "-avoid_negative_ts",
            "make_zero",
            "-map",
            "0:v:0",
            "-map",
            "0:a:0",
            "-map",
            "1:s:0",
            "-c:v",
            "libx264",
            "-preset",
            "fast",
            "-crf",
            "20",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-c:s",
            "mov_text",
            "/path/to/output.mp4",
        ];

        assert_eq!(args, expected);
    }

    #[test]
    fn test_ffmpeg_command_builder_mkv() {
        let quality = QualitySettings {
            ffmpeg_preset: "veryfast".to_string(),
            ffmpeg_crf: "23".to_string(),
            ffmpeg_audio_bitrate: "128k".to_string(),
        };

        let args = FFmpegCommandBuilder::new()
            .with_common_flags()
            .with_subtitle_duration_fix()
            .with_input("/path/to/video.mkv")
            .with_stream_mapping(&["0:v:0", "0:a:0", "0:s:0"])
            .with_video_encoding(&quality)
            .with_audio_encoding(&quality)
            .with_subtitle_encoding()
            .with_overwrite()
            .with_output("/path/to/output.mp4")
            .build();

        let expected = vec![
            "-y",
            "-fflags",
            "+genpts",
            "-fix_sub_duration",
            "-i",
            "/path/to/video.mkv",
            "-avoid_negative_ts",
            "make_zero",
            "-map",
            "0:v:0",
            "-map",
            "0:a:0",
            "-map",
            "0:s:0",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "23",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-c:s",
            "mov_text",
            "/path/to/output.mp4",
        ];

        assert_eq!(args, expected);
    }

    /// The bug this design exists to prevent.
    ///
    /// `process_job` used to call `with_output` before the input and the stream
    /// mappings, and FFmpeg applies options to the file that follows them - so
    /// every `-map` landed after the output and was silently discarded. The unit
    /// tests above did not catch it because they happened to call the builder in
    /// a different order than the code did.
    #[test]
    fn the_output_comes_last_whatever_order_the_caller_used() {
        let quality = QualitySettings::default();

        let args = FFmpegCommandBuilder::new()
            .with_common_flags()
            .with_video_encoding(&quality)
            .with_subtitle_encoding()
            .with_overwrite()
            .with_output("/path/to/output.mp4")
            .with_subtitle_duration_fix()
            .with_input("/path/to/video.mkv")
            .with_stream_mapping(&["0:v", "0:a", "0:s?"])
            .build();

        assert_eq!(
            args.last().map(String::as_str),
            Some("/path/to/output.mp4"),
            "the output path must be the final argument: {args:?}"
        );

        let position = |needle: &str| args.iter().position(|arg| arg == needle).unwrap();
        assert!(
            position("-i") < position("-map"),
            "inputs must precede the mappings that refer to them: {args:?}"
        );
        assert!(
            position("-fix_sub_duration") < position("-i"),
            "an input option must precede the input it applies to: {args:?}"
        );
        assert_eq!(
            args.iter().filter(|arg| *arg == "-map").count(),
            3,
            "every mapping survives: {args:?}"
        );
    }

    #[test]
    fn maps_every_stream_rather_than_the_first_of_each() {
        let args = FFmpegCommandBuilder::new()
            .with_input("/in.mkv")
            .with_stream_mapping(&["0:v", "0:a", "0:s?"])
            .with_output("/out.mp4")
            .build();

        let mappings: Vec<&String> = args
            .iter()
            .skip_while(|arg| *arg != "-map")
            .filter(|arg| !arg.starts_with('-'))
            .collect();

        assert_eq!(
            mappings,
            vec!["0:v", "0:a", "0:s?", "/out.mp4"],
            "audio and subtitles are mapped as groups, and subtitles optionally"
        );
    }

    /// Whether a real FFmpeg is available to exercise the transcoding path.
    ///
    /// The tests below are the only ones that prove what FFmpeg does with the
    /// arguments we build, so a developer without FFmpeg installed skips them
    /// rather than failing - but **CI must actually run them**. A skipped test
    /// and a passing one are indistinguishable in the log, so if these quietly
    /// stopped running nobody would find out until a library lost a track.
    /// Chunking small enough that a few seconds of test footage exercises it.
    const TEST_CHUNKING: Chunking = Chunking {
        chunk_seconds: 2.0,
        min_source_seconds: 3.0,
    };

    /// Build a short MKV carrying video, audio and one subtitle track.
    fn build_chunking_source(path: &Path, seconds: u32) {
        let subtitles = path.with_extension("srt");
        std::fs::write(
            &subtitles,
            "1\n00:00:00,500 --> 00:00:01,500\nfirst line\n\n\
             2\n00:00:01,600 --> 00:00:03,000\nspanning a chunk boundary\n",
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
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(built.success(), "could not build the test source");
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

    /// The codecs of every stream in a file, in order.
    fn stream_codecs(path: &Path) -> Vec<String> {
        let output = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_name",
                "-of",
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect()
    }

    fn chunking_job(input: &Path) -> Job {
        Job::new(
            input.to_path_buf(),
            MediaFileType::Mkv,
            QualitySettings {
                ffmpeg_preset: "ultrafast".to_string(),
                ffmpeg_crf: "30".to_string(),
                ffmpeg_audio_bitrate: "64k".to_string(),
            },
            PostProcessingSettings::default(),
            input.parent().unwrap(),
        )
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
            .process_job(&job, None, Some(&work_folder))
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
        assert!(!work_folder.join(format!("{}.chunks", job.id)).exists());
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
        processor
            .encode_chunks(&job, &input, &chunk_dir, &chunks)
            .await
            .unwrap();

        assert_eq!(
            std::fs::metadata(&first_chunk).unwrap().modified().unwrap(),
            untouched,
            "the chunk the first worker finished was encoded a second time"
        );
        for chunk in &chunks {
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
            .process_job(&job, None, Some(&work_folder))
            .await
            .unwrap();

        assert!(job.work_folder_output_path(&work_folder).exists());
        assert!(
            !work_folder.join(format!("{}.chunks", job.id)).exists(),
            "a source below the threshold should never be divided up"
        );
    }

    #[test]
    fn an_input_option_lands_on_the_input_it_was_chained_before() {
        let args = FFmpegCommandBuilder::new()
            .with_concat_list("/work/chunks.txt")
            .with_subtitle_duration_fix()
            .with_input("/media/film.mkv")
            .with_output("/work/film.mp4")
            .build();

        // `-f concat -safe 0` describes the list; `-fix_sub_duration` describes
        // the source the subtitles come from. Held in one bucket ahead of every
        // input, as an earlier version did, both would apply to the list.
        assert_eq!(
            args.join(" "),
            "-f concat -safe 0 -i /work/chunks.txt -fix_sub_duration -i /media/film.mkv \
             /work/film.mp4"
        );
    }

    #[test]
    fn options_with_no_input_to_attach_to_are_still_emitted() {
        // A builder used for flags alone has nowhere to put them, and dropping
        // them silently is exactly the failure the buckets exist to avoid.
        let args = FFmpegCommandBuilder::new()
            .with_common_flags()
            .with_overwrite()
            .build();

        assert_eq!(
            args,
            vec![
                "-y",
                "-fflags",
                "+genpts",
                "-avoid_negative_ts",
                "make_zero"
            ]
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
        let chunk_dir = chunk_dir_for(&job, &work_folder);

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
        let chunk_dir = chunk_dir_for(&job, &work_folder);

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
        let chunk_dir = chunk_dir_for(&job, &work_folder);

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

    /// The `pts_time,duration_time` of every subtitle event in a file.
    fn subtitle_timings(path: &Path) -> Vec<(f64, f64)> {
        let output = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "s",
                "-show_entries",
                "packet=pts_time,duration_time",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .unwrap();

        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut fields = line.trim().split(',');
                let pts = fields.next()?.parse().ok()?;
                let duration = fields.next()?.parse().ok()?;
                Some((pts, duration))
            })
            .collect()
    }

    #[tokio::test]
    async fn a_long_source_converts_its_subtitles_the_same_way_a_short_one_does() {
        if !ffmpeg_present() {
            return;
        }

        let temp = TempDir::new().unwrap();
        let input = temp.path().join("film.mkv");

        // An event that outlives the one after it. Without -fix_sub_duration it
        // keeps its declared length and overlaps its neighbours, which mov_text
        // handles badly - and the chunked path used to lose that flag, so every
        // file over the threshold converted differently from one under it.
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
            .process_job(&job, None, Some(&one_pass_dir))
            .await
            .unwrap();

        let chunked_dir = temp.path().join("chunked");
        std::fs::create_dir_all(&chunked_dir).unwrap();
        FFmpegProcessor::new(false)
            .with_chunking(TEST_CHUNKING)
            .process_job(&job, None, Some(&chunked_dir))
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

        // And what both produce is non-overlapping, which is the point of the
        // flag rather than an accident of the two agreeing.
        for pair in chunked.windows(2) {
            let ((start, duration), (next_start, _)) = (pair[0], pair[1]);
            assert!(
                start + duration <= next_start + f64::EPSILON,
                "subtitle at {start} runs {duration}s into the one at {next_start}"
            );
        }
    }

    /// Subtitle event start times, in hundredths of a second, with slivers
    /// dropped.
    ///
    /// The one-pass encode carries `-avoid_negative_ts make_zero`, and the audio
    /// it encodes starts one AAC frame - 1024/44100, about 23ms - before zero,
    /// so the whole timeline including the subtitles shifts by that much and the
    /// first event is split around the seam. The join has no negative timestamp
    /// to correct, because the chunks were muxed with `-muxdelay 0`. That offset
    /// is not a difference in subtitle handling, and comparing raw packets would
    /// make this test about it.
    fn subtitle_events(timings: &[(f64, f64)]) -> Vec<i64> {
        timings
            .iter()
            .filter(|(_, duration)| *duration > 0.05)
            .map(|(start, _)| (start * 10.0).round() as i64)
            .collect()
    }

    fn ffmpeg_present() -> bool {
        let available = std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);

        if !available {
            assert!(
                std::env::var("CI").is_err(),
                "FFmpeg must be installed in CI: these tests are the only check that every stream survives a transcode, and skipping them silently would leave that unverified"
            );
            eprintln!("skipping: ffmpeg is not on PATH");
        }

        available
    }

    /// Codec type and language of every stream in a file, via ffprobe.
    fn streams_of(path: &Path) -> Vec<String> {
        let output = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type:stream_tags=language",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .expect("ffprobe should run");

        String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| line.trim().trim_end_matches(',').to_string())
            .filter(|line| !line.is_empty())
            .collect()
    }

    fn transcode(input: &Path) -> PathBuf {
        let job = Job::new(
            input.to_path_buf(),
            MediaFileType::Mkv,
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
    #[test]
    fn test_ffmpeg_command_builder_build_command() {
        let quality = QualitySettings::default();
        let builder = FFmpegCommandBuilder::new()
            .with_common_flags()
            .with_video_encoding(&quality);

        let mut cmd = Command::new("ffmpeg");
        builder.build_command(&mut cmd);

        // We can't easily test the internal state of Command, but we can verify
        // the builder doesn't panic when applied to a command
        assert_eq!(cmd.as_std().get_program(), "ffmpeg");
    }

    #[test]
    fn test_builder_method_chaining() {
        // Test that all methods return Self for fluent chaining
        let quality = QualitySettings::default();

        let _builder = FFmpegCommandBuilder::new()
            .with_common_flags()
            .with_subtitle_duration_fix()
            .with_input("test.mkv")
            .with_stream_mapping(&["0:v:0", "0:a:0", "0:s:0"])
            .with_video_encoding(&quality)
            .with_audio_encoding(&quality)
            .with_subtitle_encoding()
            .with_overwrite()
            .with_output("test.mp4");

        // If we get here without compile errors, method chaining works
    }

    #[test]
    fn test_builder_path_handling() {
        let input_path = PathBuf::from("/test/input.webm");
        let subtitle_path = PathBuf::from("/test/input.vtt");
        let output_path = PathBuf::from("/test/output.mp4");

        let args = FFmpegCommandBuilder::new()
            .with_inputs(&[&input_path, &subtitle_path])
            .with_output(&output_path)
            .build();

        assert!(args.contains(&"/test/input.webm".to_string()));
        assert!(args.contains(&"/test/input.vtt".to_string()));
        assert!(args.contains(&"/test/output.mp4".to_string()));
    }

    #[tokio::test]
    async fn test_work_folder_output_path_generation() {
        let temp_dir = TempDir::new().unwrap();
        let work_folder = temp_dir.path();

        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings {
            disable_source_files: false,
        };
        let media_root = temp_dir.path();
        let job = Job::new(
            PathBuf::from("test.mkv"),
            MediaFileType::Mkv,
            quality,
            post_processing,
            media_root,
        );

        let work_output_path = job.work_folder_output_path(work_folder);

        // Verify the path structure
        assert!(work_output_path.starts_with(work_folder));
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
            .ends_with("test.mp4"));
    }

    #[tokio::test]
    async fn test_move_to_destination() {
        let temp_dir = TempDir::new().unwrap();
        let work_folder = temp_dir.path().join("work");
        let media_folder = temp_dir.path().join("media");

        tokio::fs::create_dir_all(&work_folder).await.unwrap();
        tokio::fs::create_dir_all(&media_folder).await.unwrap();

        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings {
            disable_source_files: false,
        };
        let job = Job::new(
            PathBuf::from("test.mkv"),
            MediaFileType::Mkv,
            quality,
            post_processing,
            &media_folder,
        );

        // Create a dummy file in the work folder
        let work_output_path = job.work_folder_output_path(&work_folder);
        tokio::fs::write(&work_output_path, "test content")
            .await
            .unwrap();

        let processor = FFmpegProcessor::new(false);

        // Move the file - since job now has absolute paths, pass None for media_root
        processor
            .move_to_destination(&job, None, &work_folder)
            .await
            .unwrap();

        // Verify the file was moved
        assert!(!work_output_path.exists());
        let final_path = job.full_output_path(None);
        assert!(final_path.exists());

        let content = tokio::fs::read_to_string(&final_path).await.unwrap();
        assert_eq!(content, "test content");
    }
}
