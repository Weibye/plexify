use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, error, info, warn};

use uuid::Uuid;

use crate::job::{AudioAction, Job, MediaFileType, Operation, QualitySettings};
use crate::paths::to_forward_slashes;
use crate::subtitles::{self, Sidecar, SidecarFormat};

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
/// different option for each of its inputs - `-f concat` on the list of chunks
/// a join reads, but not on the source it takes subtitles from. An earlier
/// version kept every input option in one bucket ahead of every input, which
/// silently put them all on input 0.
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

    /// Have the demuxer generate presentation timestamps for the next input.
    ///
    /// Nothing here shifts the output timeline to keep it off zero, and
    /// `-avoid_negative_ts make_zero` is deliberately absent from all three
    /// outputs.
    ///
    /// Two things put a negative timestamp in front of an MP4 mux. An AAC
    /// encoder emits its priming frame before the content starts, so the
    /// one-pass encode's first audio packet is one frame early; and x264 holds
    /// frames back to reorder them, so the concat demuxer feeding the join
    /// hands over video whose first DTS is a reorder delay before its PTS. MP4
    /// has an edit list, which is the field built to express exactly that, and
    /// both are already carried there.
    ///
    /// `make_zero` answers the same question a second time, by shifting *every*
    /// stream forward until nothing is negative. On the one-pass encode that is
    /// one AAC frame. On the join it is the reorder delay, which leaves the
    /// picture starting at that delay rather than where the chunks put it -
    /// measured 0.080s at 25fps, 0.200s at 10fps, 0.500s at 4fps. Either way
    /// the audio and the subtitles are dragged along with it and the first
    /// subtitle event is split around the seam.
    ///
    /// The shift is itself written as an edit, so a player that ignores edit
    /// lists sees the same samples with or without the flag. That is why it
    /// could sit here unnoticed, and is not a reason to put it back.
    ///
    /// Only on the chunks is it genuinely inert: they are MPEG-TS, which cannot
    /// carry a negative timestamp at all, so FFmpeg shifts it there without
    /// being asked and the chunk files come out byte-identical either way.
    pub fn with_generated_pts(self) -> Self {
        self.with_input_options(&["-fflags", "+genpts"])
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
    pub fn with_stream_mapping<S: AsRef<str>>(mut self, mappings: &[S]) -> Self {
        for mapping in mappings {
            self.output_options.push("-map".to_string());
            self.output_options.push(mapping.as_ref().to_string());
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

    /// Copy the video bitstream through untouched.
    pub fn with_video_copy(mut self) -> Self {
        self.output_options
            .extend_from_slice(&["-c:v".to_string(), "copy".to_string()]);
        self
    }

    /// Copy the audio through untouched.
    pub fn with_audio_copy(mut self) -> Self {
        self.output_options
            .extend_from_slice(&["-c:a".to_string(), "copy".to_string()]);
        self
    }

    /// Copy video and audio through untouched.
    pub fn with_video_and_audio_copy(self) -> Self {
        self.with_video_copy().with_audio_copy()
    }

    /// Mix the audio down to this many channels.
    ///
    /// A layout the client cannot render is a reason to touch the audio all on
    /// its own: a 5.1 AAC track is in a codec the Chromecast decodes and a
    /// layout its receiver does not.
    pub fn with_audio_channels(mut self, channels: u32) -> Self {
        self.output_options
            .extend_from_slice(&["-ac".to_string(), channels.to_string()]);
        self
    }

    /// Put the MP4 index in front of the media data.
    ///
    /// The muxer writes it last either way; this rewrites the finished file to
    /// move it, so it costs one extra pass over the output.
    pub fn with_faststart(mut self) -> Self {
        self.output_options
            .extend_from_slice(&["-movflags".to_string(), "+faststart".to_string()]);
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

/// Where a finished output is copied to before it is renamed onto its
/// destination.
///
/// It sits in the destination directory, because a rename is only atomic within
/// one filesystem, and carries a suffix no part of the pipeline treats as media
/// so that a copy which never finished cannot be taken for an encode that did.
///
/// The name carries `worker_id` because a staging name shared between workers is
/// a second way to arrive at the corrupt output the staging name exists to
/// prevent: two workers holding one input would copy into one file and each
/// rename the splice onto the destination, which every later run then accepts as
/// a finished encode. The id belongs to the worker rather than to the call so
/// that a worker retrying a move writes over its own last attempt instead of
/// leaving another part-copy beside the destination.
pub fn staging_path_for(final_output_path: &Path, worker_id: &Uuid) -> PathBuf {
    let mut staging = final_output_path.as_os_str().to_os_string();
    staging.push(format!(".{worker_id}.partial"));
    PathBuf::from(staging)
}

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
pub enum SubtitleSelection {
    /// The source was probed. `kept` holds the positions, among the source's
    /// subtitle streams, of the ones that can become `mov_text`; `dropped`
    /// holds the ones that cannot, so the run can say what it left out.
    Probed {
        kept: Vec<usize>,
        dropped: Vec<SubtitleStream>,
    },
    /// FFprobe could not read the source, so nothing is known about what its
    /// subtitle streams hold. Map them all, optionally, and leave FFmpeg to
    /// judge: a bitmap stream then fails the job, which is no worse than the
    /// behaviour that existed before the encode could ask, and is a great deal
    /// better than discarding a track that was never identified.
    Unprobed,
}

impl SubtitleSelection {
    /// Mappings for the subtitle streams to carry, reading them from the given
    /// input.
    ///
    /// The source is input 0 for a one-pass encode and input 1 for the join
    /// that follows a chunked one, where input 0 is the list of chunks.
    pub fn mappings(&self, input_index: usize) -> Vec<String> {
        match self {
            Self::Probed { kept, .. } => kept
                .iter()
                .map(|position| format!("{input_index}:s:{position}"))
                .collect(),
            Self::Unprobed => vec![format!("{input_index}:s?")],
        }
    }

    /// The subtitle streams that cannot go into the output.
    pub fn dropped(&self) -> &[SubtitleStream] {
        match self {
            Self::Probed { dropped, .. } => dropped,
            Self::Unprobed => &[],
        }
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

    SubtitleSelection::Probed { kept, dropped }
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
enum SubtitleSource {
    /// A WebM's `.vtt` sidecar, which is declared as its own input.
    External(PathBuf),
    /// The streams the source itself carries, and which of them the output can
    /// hold.
    Embedded(SubtitleSelection),
}

/// Every stream an output takes: video and audio from input 0, and the subtitle
/// streams that can be carried, from `subtitle_input`.
///
/// Input 0 is the source for a one-pass encode and the list of chunks for the
/// join that follows a chunked one, and in both it is where the picture and the
/// sound come from. Only the subtitles are read from somewhere else.
fn media_and_subtitle_mappings(
    subtitle_input: usize,
    selection: &SubtitleSelection,
) -> Vec<String> {
    let mut mappings = vec!["0:v".to_string(), "0:a".to_string()];
    mappings.extend(selection.mappings(subtitle_input));
    mappings
}

/// The codec options a job's operation asks for.
///
/// Whether the output carries subtitles at all is decided by the caller, which
/// has to leave them out of the stream mappings as well as out of the codecs.
fn with_operation(builder: FFmpegCommandBuilder, job: &Job) -> FFmpegCommandBuilder {
    match job.operation {
        // The index goes to the front only on this path. A remux exists because
        // the container is what the player choked on, and leaving the new one
        // indexed from the end would answer the wrong half of the question.
        Operation::Remux { audio, .. } => {
            let builder = builder.with_video_copy().with_faststart();
            match audio {
                AudioAction::Copy => builder.with_audio_copy(),
                AudioAction::Transcode { channels } => {
                    let builder = builder.with_audio_encoding(&job.quality_settings);
                    match channels {
                        Some(channels) => builder.with_audio_channels(channels),
                        None => builder,
                    }
                }
            }
        }
        Operation::Reencode => builder
            .with_video_encoding(&job.quality_settings)
            .with_audio_encoding(&job.quality_settings),
    }
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
    /// Tells this worker's staging files apart from every other worker's. See
    /// [`staging_path_for`].
    worker_id: Uuid,
}

impl FFmpegProcessor {
    pub fn new(background_mode: bool) -> Self {
        Self {
            background_mode,
            chunking: Chunking::default(),
            worker_id: Uuid::new_v4(),
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
                self.select_subtitles(&input_path).await,
            )),
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
            (Some(work_folder), Operation::Reencode) => {
                match self.probe_duration(&input_path).await {
                    Some(duration) if duration >= self.chunking.min_source_seconds => {
                        self.process_in_chunks(
                            job,
                            &input_path,
                            subtitles.as_ref(),
                            &output_path,
                            work_folder,
                            duration,
                        )
                        .await?;
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        };

        if !chunked {
            self.process_in_one_pass(job, &input_path, subtitles.as_ref(), &output_path)
                .await?;
        }

        // Only once the media file itself is written. A sidecar beside an
        // output that never arrived is a subtitle for nothing.
        if job.operation.extracts_subtitles() {
            self.extract_sidecars(&input_path, &output_path).await?;
        }

        Ok(())
    }

    /// Write the source's text subtitle tracks beside the output.
    ///
    /// They are written next to the output wherever that is - the work folder
    /// for a job that has one - and travel with it, for the same reason the
    /// video does: a media server that indexes a half-written subtitle shows a
    /// broken track.
    async fn extract_sidecars(&self, input_path: &Path, output_path: &Path) -> Result<()> {
        let Some(streams) = self.probe_subtitle_streams(input_path).await else {
            warn!("FFprobe could not read the subtitle streams of {input_path:?}; no sidecar was written, and the tracks are still in the source.");
            return Ok(());
        };

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
    async fn select_subtitles(&self, input_path: &Path) -> SubtitleSelection {
        let Some(streams) = self.probe_subtitle_streams(input_path).await else {
            warn!("⚠️ FFprobe could not read the subtitle streams of {input_path:?}; every subtitle stream will be offered to FFmpeg as it was before.");
            return SubtitleSelection::Unprobed;
        };

        let selection = select_subtitle_streams(&streams);

        for stream in selection.dropped() {
            warn!("{}", dropped_stream_warning(stream, input_path));
        }

        selection
    }

    /// Convert the whole source in a single FFmpeg run.
    async fn process_in_one_pass(
        &self,
        job: &Job,
        input_path: &Path,
        subtitles: Option<&SubtitleSource>,
        output_path: &Path,
    ) -> Result<()> {
        let ffmpeg_builder = with_operation(FFmpegCommandBuilder::new().with_generated_pts(), job)
            .with_overwrite()
            .with_output(output_path);

        // Add format-specific flags, inputs, and mappings
        let ffmpeg_builder = match subtitles {
            // A track the target burns into the picture is left out of the
            // codecs and the mappings both. Half of that is not enough: a
            // mapped subtitle stream with no subtitle codec fails the mux.
            None => ffmpeg_builder
                .with_input(input_path)
                .with_stream_mapping(&["0:v", "0:a"]),
            // The subtitle is the whole reason the second input is here, so it
            // is not optional.
            Some(SubtitleSource::External(vtt_path)) => ffmpeg_builder
                .with_subtitle_encoding()
                .with_inputs(&[input_path, vtt_path.as_path()])
                .with_stream_mapping(&["0:v", "0:a", "1:s"]),
            // Every stream, not the first of each: a second audio track is
            // usually a commentary or another language, and dropping it while
            // renaming the source to `.disabled` loses it for good. The
            // subtitle streams are named one by one because the source may
            // carry one MP4 cannot hold, and `0:s?` would take that one too.
            Some(SubtitleSource::Embedded(selection)) => ffmpeg_builder
                .with_subtitle_encoding()
                .with_input(input_path)
                .with_stream_mapping(&media_and_subtitle_mappings(0, selection)),
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
        subtitles: Option<&SubtitleSource>,
        output_path: &Path,
        work_folder: &Path,
        duration: f64,
    ) -> Result<()> {
        // `encode_chunks` re-encodes the video, so reaching here with a remux
        // would silently throw away the copy the operation exists for. The
        // caller checks; this says so where the code that depends on it lives.
        debug_assert!(
            job.operation == Operation::Reencode,
            "only a re-encode is chunked"
        );

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
            .with_concat_list(&list_path)
            .with_video_and_audio_copy()
            .with_output(output_path);

        let mux_builder = match subtitles {
            None => mux_builder.with_stream_mapping(&["0:v", "0:a"]),
            Some(SubtitleSource::External(vtt_path)) => mux_builder
                .with_subtitle_encoding()
                .with_input(vtt_path.as_path())
                .with_stream_mapping(&["0:v", "0:a", "1:s"]),
            // The same selection the one-pass encode was handed, read from the
            // source rather than from the chunks - which hold no subtitles,
            // because a subtitle event straddling a boundary would have been
            // cut in half by it. Video and audio still come from the chunk
            // list, input 0; the subtitles come from input 1.
            Some(SubtitleSource::Embedded(selection)) => mux_builder
                .with_subtitle_encoding()
                .with_input(input_path)
                .with_stream_mapping(&media_and_subtitle_mappings(1, selection)),
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
                .with_generated_pts()
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

            // The plan came from the container's duration, which is the longest
            // of its streams - so the final chunk can begin after the last
            // video frame, and `plan_chunks` rounds up, so it can begin after
            // the audio has ended too. What comes out is a chunk of a few
            // hundred bytes that ffprobe can find no video stream in at all.
            //
            // The concat demuxer needs every file in its list to declare the
            // same streams. Given one that does not, the joined MP4's video
            // track is left running a whole frame interval past its own last
            // picture. So the criterion is exactly what is asked below: a chunk
            // no video stream can be found in is the end of the file.
            //
            // That is narrower than "produced no video", and deliberately. A
            // chunk that holds a real audio tail and no picture - a source
            // whose sound outlives its image - still declares a video stream in
            // its PMT, so it does not trip this and is joined in with its audio
            // intact. Measured: 2s chunks past the last frame carry 0 video and
            // 95 audio packets and are kept; only the sub-kilobyte final chunk
            // is dropped.
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

    /// How many lines FFprobe emits for a file's video streams.
    ///
    /// Not a stream count: `-of csv` gives a transport stream both a
    /// `program,stream,N` line and a `stream,N` line, so one video stream comes
    /// back as 2. Only `Some(0)` is ever consulted - "FFprobe found no video
    /// stream here" - and that reading is sound whatever the count means above
    /// zero. Video rather than any stream, because the concat demuxer needs
    /// every file in its list to declare the same streams.
    async fn probe_video_stream_count(&self, path: &Path) -> Option<usize> {
        let output = self
            .probe_command()
            .args([
                "-v",
                "error",
                "-select_streams",
                "v",
                "-show_entries",
                "stream=index",
                "-of",
                "csv",
            ])
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

    /// The subtitle streams a source carries, in the order it lists them.
    ///
    /// That order is what makes the result usable as a mapping: the nth line
    /// here is the stream FFmpeg calls `0:s:n`. A source with no subtitles
    /// probes successfully and comes back empty, which is a different answer
    /// from `None` - that means FFprobe could not read the file at all.
    async fn probe_subtitle_streams(&self, input_path: &Path) -> Option<Vec<SubtitleStream>> {
        let output = self
            .probe_command()
            .args([
                "-v",
                "error",
                "-select_streams",
                "s",
                "-show_entries",
                "stream=codec_name:stream_disposition=forced:stream_tags=language",
                "-of",
                "csv=p=0",
            ])
            .arg(input_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .ok()?;

        if !output.status.success() {
            return None;
        }

        Some(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(parse_probed_subtitle_line)
                .collect(),
        )
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

    /// Move completed file from work folder to media folder.
    ///
    /// The work root and the media root are routinely on different volumes, so
    /// the file has to be copied rather than renamed - and a copy is not atomic.
    /// It therefore goes to a staging name beside the destination and is renamed
    /// onto the destination from there, which within one directory is atomic. A
    /// media server sees the output either absent or whole, and a copy that dies
    /// part-way leaves a `.partial` that no later run mistakes for a finished
    /// encode: [`Job::output_exists`] does not accept it, so the job is tried
    /// again rather than recorded as done.
    ///
    /// The staging name is this worker's alone ([`staging_path_for`]), so two
    /// workers that both end up holding one input cannot splice their copies
    /// together into one file and land it at the destination.
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
        let staging_path = staging_path_for(&final_output_path, &self.worker_id);

        if let Err(e) = tokio::fs::copy(&work_output_path, &staging_path).await {
            let _ = tokio::fs::remove_file(&staging_path).await;
            return Err(anyhow!("Failed to copy output to {staging_path:?}: {e}"));
        }

        if let Err(e) = tokio::fs::rename(&staging_path, &final_output_path).await {
            let _ = tokio::fs::remove_file(&staging_path).await;
            return Err(anyhow!(
                "Failed to move output into place at {final_output_path:?}: {e}"
            ));
        }

        // A media file and the files named after it move as a group, and in
        // this order: a sidecar that arrives first is a subtitle for a file
        // that is not there yet, and one that never arrives is a track the
        // library will never show.
        self.move_sidecars(&work_output_path, &final_output_path)
            .await;

        // The output is where it belongs, so the move has succeeded. A work file
        // that will not delete is worth a word but not a failed job: reporting
        // failure here would send the job round again to find its own finished
        // output at the destination.
        if let Err(e) = tokio::fs::remove_file(&work_output_path).await {
            warn!("Could not remove {work_output_path:?} after moving it: {e}");
        }

        info!(
            "📁 Moved completed file: {:?} -> {:?}",
            work_output_path, final_output_path
        );

        Ok(())
    }
    /// Move the subtitle sidecars written beside a work-folder output to sit
    /// beside the finished file.
    ///
    /// Only the subtitle extensions this project writes are carried, rather
    /// than everything sharing the stem. The work folder also holds this job's
    /// chunk directory and its partial files, and a mover that took anything
    /// with the right prefix would eventually take one of those.
    ///
    /// A sidecar that will not move is a warning, not a failed job: the media
    /// file is already at its destination, and failing here would send the job
    /// round again to find its own finished output in place.
    async fn move_sidecars(&self, work_output: &Path, final_output: &Path) {
        const SIDECAR_EXTENSIONS: [&str; 5] = ["srt", "ass", "ssa", "vtt", "ttxt"];

        let (Some(directory), Some(work_stem), Some(final_stem)) = (
            work_output.parent(),
            work_output.file_stem().map(|stem| stem.to_string_lossy()),
            final_output.file_stem().map(|stem| stem.to_string_lossy()),
        ) else {
            return;
        };

        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().to_string();
            let Some(suffix) = name
                .strip_prefix(work_stem.as_ref())
                .and_then(|rest| rest.strip_prefix('.'))
            else {
                continue;
            };

            let extension = suffix.rsplit('.').next().unwrap_or_default();
            if !SIDECAR_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()) {
                continue;
            }

            let destination = final_output.with_file_name(format!("{final_stem}.{suffix}"));
            if let Err(e) = tokio::fs::copy(&path, &destination).await {
                warn!("Could not put the subtitle {destination:?} beside its file: {e}");
                continue;
            }

            if let Err(e) = tokio::fs::remove_file(&path).await {
                warn!("Could not remove {path:?} after moving it: {e}");
            }

            info!("Moved subtitle: {:?} -> {:?}", path, destination);
        }
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
    use crate::job::{
        AudioAction, Job, MediaFileType, PostProcessingSettings, QualitySettings, SubtitleAction,
    };
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
            .with_generated_pts()
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

    /// A job for one path, carrying the operation the caller chose.
    fn job_for(path: &str, file_type: MediaFileType, operation: Operation) -> Job {
        Job::new(
            PathBuf::from(path),
            file_type,
            operation,
            QualitySettings::default(),
            PostProcessingSettings::default(),
            Path::new("/media"),
        )
    }

    /// The operation an `.avi` is queued with today.
    fn avi_remux() -> Operation {
        Operation::Remux {
            audio: AudioAction::Copy,
            subtitles: SubtitleAction::Keep,
        }
    }

    #[test]
    fn a_remux_copies_the_video_and_puts_the_index_in_front() {
        let job = job_for("/media/show.avi", MediaFileType::Avi, avi_remux());
        let joined = with_operation(FFmpegCommandBuilder::new(), &job)
            .with_output("/work/show.mp4")
            .build()
            .join(" ");

        assert!(joined.contains("-c:v copy"), "{joined}");
        assert!(joined.contains("-c:a copy"), "{joined}");
        assert!(joined.contains("-movflags +faststart"), "{joined}");
        assert!(
            !joined.contains("libx264"),
            "a remux that re-encodes is not a remux: {joined}"
        );
    }

    #[test]
    fn a_remux_whose_audio_the_target_cannot_play_still_copies_the_video() {
        let job = job_for(
            "/media/show.mkv",
            MediaFileType::Mkv,
            Operation::Remux {
                audio: AudioAction::Transcode { channels: Some(2) },
                subtitles: SubtitleAction::Keep,
            },
        );

        let joined = with_operation(FFmpegCommandBuilder::new(), &job)
            .with_output("/work/show.mp4")
            .build()
            .join(" ");

        assert!(joined.contains("-c:v copy"), "{joined}");
        // A 5.1 track in a codec the client decodes fails on its layout, and
        // the fix for that is a downmix, not a codec swap.
        assert!(joined.contains("-c:a aac -b:a 128k -ac 2"), "{joined}");
    }

    #[test]
    fn a_remux_that_keeps_its_audio_asks_for_no_channel_count() {
        let job = job_for(
            "/media/show.mkv",
            MediaFileType::Mkv,
            Operation::Remux {
                audio: AudioAction::Transcode { channels: None },
                subtitles: SubtitleAction::Keep,
            },
        );

        let joined = with_operation(FFmpegCommandBuilder::new(), &job)
            .with_output("/work/show.mp4")
            .build()
            .join(" ");

        assert!(joined.contains("-c:a aac"), "{joined}");
        assert!(!joined.contains("-ac"), "{joined}");
    }

    /// The measured LG fact: its Plex app burns `mov_text` into the picture
    /// and re-encodes the video to do it. A remux that carried the track would
    /// cost the transcode it exists to avoid.
    #[test]
    fn dropping_subtitles_leaves_out_both_the_codec_and_the_mapping() {
        let dropped = Operation::Remux {
            audio: AudioAction::Copy,
            subtitles: SubtitleAction::Drop,
        };

        assert!(!dropped.keeps_subtitles());
        assert!(avi_remux().keeps_subtitles());
        assert!(Operation::Reencode.keeps_subtitles());
    }

    #[test]
    fn a_re_encode_is_left_exactly_as_it_was() {
        let job = job_for("/media/show.mkv", MediaFileType::Mkv, Operation::Reencode);
        let joined = with_operation(FFmpegCommandBuilder::new(), &job)
            .with_output("/work/show.mp4")
            .build()
            .join(" ");

        assert!(
            joined.contains("-c:v libx264 -preset veryfast -crf 23 -c:a aac -b:a 128k"),
            "{joined}"
        );
        // Rewriting the file to move the index costs a pass over the output,
        // and nothing has measured that a re-encode needs it.
        assert!(!joined.contains("faststart"), "{joined}");
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
            .with_generated_pts()
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
            .with_generated_pts()
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
            "-i",
            "/path/to/video.mkv",
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
            .with_generated_pts()
            .with_video_encoding(&quality)
            .with_subtitle_encoding()
            .with_overwrite()
            .with_output("/path/to/output.mp4")
            .with_seek(30.0)
            .with_input("/path/to/video.mkv")
            .with_stream_mapping(&["0:v", "0:a", "0:s:0"])
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
            position("-ss") < position("-i"),
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
        let selection = SubtitleSelection::Probed {
            kept: vec![0, 1],
            dropped: Vec::new(),
        };

        let args = FFmpegCommandBuilder::new()
            .with_input("/in.mkv")
            .with_stream_mapping(&media_and_subtitle_mappings(0, &selection))
            .with_output("/out.mp4")
            .build();

        let mappings: Vec<&String> = args
            .iter()
            .skip_while(|arg| *arg != "-map")
            .filter(|arg| !arg.starts_with('-'))
            .collect();

        assert_eq!(
            mappings,
            vec!["0:v", "0:a", "0:s:0", "0:s:1", "/out.mp4"],
            "audio is mapped as a group, and every subtitle stream that can be carried by name"
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
        build_source(
            path,
            seconds,
            "1\n00:00:00,500 --> 00:00:01,500\nfirst line\n\n\
             2\n00:00:01,600 --> 00:00:03,000\nspanning a chunk boundary\n",
        )
    }

    /// The same, with subtitles the caller chooses.
    fn build_source(path: &Path, seconds: u32, srt: &str) {
        build_source_with_audio(path, seconds, srt, "aac")
    }

    /// The same again, with the source's audio codec the caller's choice.
    ///
    /// Which codec that is decides whether the container starts before zero,
    /// and one test turns on that - see
    /// `the_one_pass_encode_starts_the_output_where_the_source_starts`.
    fn build_source_with_audio(path: &Path, seconds: u32, srt: &str, audio_codec: &str) {
        let subtitles = path.with_extension("srt");
        std::fs::write(&subtitles, srt).unwrap();

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
                audio_codec,
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

    /// The presentation timestamps of one stream's packets, in order.
    fn packet_times(path: &Path, stream: &str) -> Vec<f64> {
        let output = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
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
            Operation::Reencode,
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
            .process_job(&job, None, Some(&work_folder))
            .await
            .unwrap();

        let output = job.work_folder_output_path(&work_folder);

        // The container's duration is the longest of its streams, so the plan's
        // final chunk can begin after the last picture and hold nothing but the
        // audio encoder's padding. Joined in, that chunk leaves the video track
        // running a whole frame interval past its own last frame.
        //
        // The same figure catches a boundary that stopped declaring its length,
        // because that error is per boundary and this source crosses ten of
        // them.
        let video = probed_stream_duration(&output, "v");
        assert!(
            (video - source_duration).abs() < 0.02,
            "the joined video runs {video}s against a {source_duration}s source"
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
            .with_generated_pts()
            .with_input("/media/film.mkv")
            .with_output("/work/film.mp4")
            .build();

        // `-f concat -safe 0` describes the list; `-fflags +genpts` describes
        // the source the subtitles come from. Held in one bucket ahead of every
        // input, as an earlier version did, both would apply to the list.
        assert_eq!(
            args.join(" "),
            "-f concat -safe 0 -i /work/chunks.txt -fflags +genpts -i /media/film.mkv \
             /work/film.mp4"
        );
    }

    #[test]
    fn options_with_no_input_to_attach_to_are_still_emitted() {
        // A builder used for flags alone has nowhere to put them, and dropping
        // them silently is exactly the failure the buckets exist to avoid.
        let args = FFmpegCommandBuilder::new()
            .with_generated_pts()
            .with_overwrite()
            .build();

        assert_eq!(args, vec!["-y", "-fflags", "+genpts"]);
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

    /// The text of every subtitle event in a file, in order.
    ///
    /// Read back through ASS, whose `Dialogue` lines put the text last after
    /// nine fixed fields, so an event that reached the file can be told from
    /// one that did not.
    fn subtitle_texts(path: &Path) -> Vec<String> {
        let output = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(path)
            .args(["-map", "0:s:0", "-c:s", "ass", "-f", "ass", "-"])
            .output()
            .expect("ffmpeg should run");

        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.strip_prefix("Dialogue:"))
            .filter_map(|line| line.splitn(10, ',').nth(9))
            .map(|text| text.trim().to_string())
            .collect()
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
                .process_job(&job, None, Some(&work_folder))
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
                .process_job(&job, None, Some(&work_folder))
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
                .process_job(&job, None, Some(&work_folder))
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
            .process_job(&job, None, Some(&work_folder))
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

    #[test]
    fn a_source_that_could_not_be_probed_falls_back_to_the_optional_group() {
        // Nothing is known about what the streams hold, so FFmpeg is left to
        // judge. That can still fail on a bitmap stream - but silently
        // discarding a stream nobody identified would be worse.
        assert_eq!(SubtitleSelection::Unprobed.mappings(0), vec!["0:s?"]);
        assert_eq!(SubtitleSelection::Unprobed.mappings(1), vec!["1:s?"]);
        assert!(SubtitleSelection::Unprobed.dropped().is_empty());
    }

    /// Subtitle events as whole milliseconds, so the comparison is not about
    /// float noise.
    ///
    /// Nothing else is normalised away. Both paths take their subtitles from the
    /// same source with the same flags into the same container, and neither
    /// moves the timeline off zero, so the lists are the same list.
    fn subtitle_events(timings: &[(f64, f64)]) -> Vec<(i64, i64)> {
        timings
            .iter()
            .map(|(start, duration)| {
                (
                    (start * 1000.0).round() as i64,
                    (duration * 1000.0).round() as i64,
                )
            })
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
            Operation::Reencode,
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
            .with_generated_pts()
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
            .with_generated_pts()
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
            Operation::Reencode,
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
            Operation::Reencode,
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

    /// A job whose output goes to `media/`, with the finished encode already
    /// sitting in the work folder waiting to be moved.
    async fn job_ready_to_move(temp_dir: &TempDir) -> (Job, PathBuf) {
        let work_folder = temp_dir.path().join("work");
        let media_folder = temp_dir.path().join("media");
        tokio::fs::create_dir_all(&work_folder).await.unwrap();
        tokio::fs::create_dir_all(&media_folder).await.unwrap();

        let job = Job::new(
            PathBuf::from("film.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode,
            QualitySettings::default(),
            PostProcessingSettings {
                disable_source_files: false,
            },
            &media_folder,
        );

        tokio::fs::write(job.work_folder_output_path(&work_folder), "a whole encode")
            .await
            .unwrap();

        (job, work_folder)
    }

    #[tokio::test]
    async fn a_move_that_fails_leaves_nothing_the_pipeline_calls_finished() {
        let temp_dir = TempDir::new().unwrap();
        let (job, work_folder) = job_ready_to_move(&temp_dir).await;
        let final_path = job.full_output_path(None);

        let processor = FFmpegProcessor::new(false);

        // A directory in the way of the staging name is a copy that cannot
        // start, standing in for the disk filling up or the share dropping out
        // half way through one.
        let staging_path = staging_path_for(&final_path, &processor.worker_id);
        tokio::fs::create_dir(&staging_path).await.unwrap();

        assert!(processor
            .move_to_destination(&job, None, &work_folder)
            .await
            .is_err());

        assert!(
            !final_path.exists(),
            "the destination name is only ever taken by a finished file"
        );
        assert!(
            !job.output_exists(None),
            "a job whose move failed must be tried again, not recorded as done"
        );
        assert!(
            job.work_folder_output_path(&work_folder).exists(),
            "the encode that was not moved stays where the next attempt looks"
        );

        // And the next attempt, with the obstruction gone, completes the move.
        tokio::fs::remove_dir(&staging_path).await.unwrap();
        processor
            .move_to_destination(&job, None, &work_folder)
            .await
            .unwrap();

        assert_eq!(
            tokio::fs::read_to_string(&final_path).await.unwrap(),
            "a whole encode"
        );
        assert!(!staging_path.exists());
        assert!(!job.work_folder_output_path(&work_folder).exists());
    }

    #[tokio::test]
    async fn a_part_copied_file_left_by_a_killed_move_is_not_taken_for_the_output() {
        let temp_dir = TempDir::new().unwrap();
        let (job, work_folder) = job_ready_to_move(&temp_dir).await;
        let final_path = job.full_output_path(None);
        let processor = FFmpegProcessor::new(false);
        let staging_path = staging_path_for(&final_path, &processor.worker_id);

        // What a worker killed mid-copy leaves behind: some of the bytes, under
        // the staging name. This worker's own, because a worker retrying its
        // move is the case that has to write over it rather than around it.
        tokio::fs::write(&staging_path, "a whole").await.unwrap();

        assert!(
            !job.output_exists(None),
            "debris from an interrupted copy is not an output"
        );

        processor
            .move_to_destination(&job, None, &work_folder)
            .await
            .unwrap();

        assert_eq!(
            tokio::fs::read_to_string(&final_path).await.unwrap(),
            "a whole encode",
            "the retry writes over the debris rather than around it"
        );
        assert!(!staging_path.exists());
    }

    #[test]
    fn two_workers_stage_one_destination_under_names_of_their_own() {
        let final_path = PathBuf::from("/library/Series/Show/Show - S01E01.mp4");

        let one = FFmpegProcessor::new(false);
        let other = FFmpegProcessor::new(false);

        assert_ne!(
            staging_path_for(&final_path, &one.worker_id),
            staging_path_for(&final_path, &other.worker_id),
            "two workers holding one input must not copy into one file"
        );

        // A worker's own staging name does not move under it between the copy
        // and the rename, so a retry writes over its last part-copy.
        assert_eq!(
            staging_path_for(&final_path, &one.worker_id),
            staging_path_for(&final_path, &one.worker_id)
        );

        // And whatever the name, the pipeline still reads it as staging debris
        // rather than as media.
        assert_eq!(
            staging_path_for(&final_path, &one.worker_id)
                .extension()
                .unwrap(),
            "partial"
        );
    }

    #[tokio::test]
    async fn two_workers_moving_one_output_do_not_splice_their_copies() {
        let temp_dir = TempDir::new().unwrap();
        let media_folder = temp_dir.path().join("media");
        tokio::fs::create_dir_all(&media_folder).await.unwrap();

        let job = Job::new(
            PathBuf::from("film.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode,
            QualitySettings::default(),
            PostProcessingSettings {
                disable_source_files: false,
            },
            &media_folder,
        );
        let final_path = job.full_output_path(None);

        // Two workers that both ended up holding this input - the queue is
        // meant to prevent it, but the destination is a media library and the
        // cost of being wrong is a file nobody can reconstruct. Each has its
        // own encode of the same source, big enough that the two copies are in
        // flight at the same time.
        let encodes = [(b'a', "one"), (b'b', "other")].map(|(byte, name)| {
            let work_folder = temp_dir.path().join(name);
            std::fs::create_dir_all(&work_folder).unwrap();
            let content = vec![byte; 8 * 1024 * 1024];
            std::fs::write(job.work_folder_output_path(&work_folder), &content).unwrap();
            (FFmpegProcessor::new(false), work_folder, content)
        });
        let [(one, one_work, one_content), (other, other_work, other_content)] = &encodes;

        let (first, second) = tokio::join!(
            one.move_to_destination(&job, None, one_work),
            other.move_to_destination(&job, None, other_work)
        );
        first.unwrap();
        second.unwrap();

        // Whichever rename landed last, the destination holds one worker's
        // encode from end to end. A shared staging name would leave the two
        // interleaved, and `output_exists` would call that finished.
        let landed = tokio::fs::read(&final_path).await.unwrap();
        assert!(
            &landed == one_content || &landed == other_content,
            "the destination holds {} bytes that are neither encode",
            landed.len()
        );

        let mut left_behind = Vec::new();
        let mut entries = std::fs::read_dir(&media_folder).unwrap();
        while let Some(Ok(entry)) = entries.next() {
            left_behind.push(entry.file_name().to_string_lossy().to_string());
        }
        assert_eq!(
            left_behind,
            vec![final_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()],
            "each worker takes its own staging file with it"
        );
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

    /// The whole point of #162: the tracks leave the container so nothing is
    /// burned in, and they land beside it rather than being lost.
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

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(FFmpegProcessor::new(false).process_job(&job, None, None))
            .expect("the remux should succeed");

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

    /// The population this whole issue is about: an MP4 this project produced
    /// itself, carrying `mov_text`. Proposing an original for it asked FFmpeg
    /// for a muxer that does not exist, and the job failed *after* the media
    /// file had been written - at its destination, with no work folder to hold
    /// it back, and an error naming no subtitle.
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
            .block_on(FFmpegProcessor::new(false).process_job(&job, None, Some(&work_folder)))
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

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(FFmpegProcessor::new(false).process_job(&job, None, None))
            .expect("the remux should succeed");

        let names = sidecar_names(&source);
        assert!(names.contains(&"show.eng.srt".to_string()), "{names:?}");
        assert!(
            !names.iter().any(|name| name == "show.eng.ass"),
            "an unstyled original is not worth keeping: {names:?}"
        );
    }

    /// A sidecar written into the work folder has to arrive beside the finished
    /// file, or the library never sees the track.
    #[tokio::test]
    async fn sidecars_move_out_of_the_work_folder_with_their_file() {
        let temp_dir = TempDir::new().unwrap();
        let work_folder = temp_dir.path().join("work");
        let media = temp_dir.path().join("media");
        std::fs::create_dir_all(&work_folder).unwrap();
        std::fs::create_dir_all(&media).unwrap();

        let job = Job::new(
            media.join("show.mkv"),
            MediaFileType::Mkv,
            Operation::Remux {
                audio: AudioAction::Copy,
                subtitles: SubtitleAction::Extract,
            },
            QualitySettings::default(),
            PostProcessingSettings::default(),
            &media,
        );

        let work_output = job.work_folder_output_path(&work_folder);
        let work_stem = work_output
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_string();
        std::fs::write(&work_output, "video").unwrap();
        std::fs::write(work_folder.join(format!("{work_stem}.eng.srt")), "subs").unwrap();
        // Not a sidecar, and it must not be dragged along.
        std::fs::write(work_folder.join(format!("{work_stem}.log")), "noise").unwrap();

        FFmpegProcessor::new(false)
            .move_to_destination(&job, None, &work_folder)
            .await
            .expect("the move should succeed");

        assert!(media.join("show.eng.srt").exists(), "the subtitle followed");
        assert!(!work_folder.join(format!("{work_stem}.eng.srt")).exists());
        assert!(
            work_folder.join(format!("{work_stem}.log")).exists(),
            "only subtitles move"
        );
    }

    /// An AVI carrying what this library's AVIs carry: MPEG-4 Part 2 video and
    /// AC-3 audio.
    fn build_avi_source(path: &Path) {
        let built = std::process::Command::new("ffmpeg")
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=160x120:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:v",
                "mpeg4",
                "-c:a",
                "ac3",
                "-y",
            ])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(built.success(), "could not build the test source");
    }

    /// The codec name FFprobe reports for a file's first stream of `kind`.
    fn codec_of(path: &Path, kind: &str) -> String {
        let output = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                kind,
                "-show_entries",
                "stream=codec_name",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .expect("ffprobe should run");

        String::from_utf8_lossy(&output.stdout)
            .trim()
            .trim_end_matches(',')
            .to_string()
    }

    #[test]
    fn an_avi_becomes_an_mp4_without_the_video_being_touched() {
        if !ffmpeg_present() {
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let input = temp_dir.path().join("show.avi");
        build_avi_source(&input);

        let job = Job::new(
            input.clone(),
            MediaFileType::Avi,
            avi_remux(),
            QualitySettings::default(),
            PostProcessingSettings::default(),
            temp_dir.path(),
        );

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(FFmpegProcessor::new(false).process_job(&job, None, None))
            .expect("remux should succeed");

        let output = &job.output_path;
        assert_eq!(output.extension().unwrap(), "mp4");

        // The bitstream came through as it was. Anything else here means the
        // Pi was asked to spend days re-encoding a picture that already plays.
        assert_eq!(codec_of(output, "v"), "mpeg4");
        assert_eq!(codec_of(output, "a"), "ac3");

        // And the index is in front of the media, which is the whole reason
        // this file is being rewritten: the measured stall was a client waiting
        // on an index the container did not carry up front.
        let head = std::fs::read(output).unwrap();
        let position = |atom: &[u8]| {
            head.windows(4)
                .position(|window| window == atom)
                .unwrap_or_else(|| {
                    panic!("no {} atom in the output", String::from_utf8_lossy(atom))
                })
        };
        assert!(position(b"moov") < position(b"mdat"));
    }

    /// The LG's app burns `mov_text` in and re-encodes the video to do it, so
    /// a remux that carried the track would cost the transcode it exists to
    /// avoid. Dropping it has to leave the picture alone.
    #[test]
    fn a_remux_that_drops_subtitles_produces_a_file_with_none() {
        if !ffmpeg_present() {
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let input = temp_dir.path().join("show.mkv");
        build_source(&input, 2, "1\n00:00:00,000 --> 00:00:01,000\nhello\n\n");

        let job = Job::new(
            input.clone(),
            MediaFileType::Mkv,
            Operation::Remux {
                audio: AudioAction::Copy,
                subtitles: SubtitleAction::Drop,
            },
            QualitySettings::default(),
            PostProcessingSettings::default(),
            temp_dir.path(),
        );

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(FFmpegProcessor::new(false).process_job(&job, None, None))
            .expect("remux should succeed");

        let output = &job.output_path;
        assert_eq!(codec_of(output, "s"), "", "the track has to be gone");
        assert_eq!(codec_of(output, "v"), "h264", "and the picture untouched");
        assert_eq!(codec_of(output, "a"), "aac");
    }
}
