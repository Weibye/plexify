//! FFmpeg's command line, assembled in the order FFmpeg reads it.

use std::path::Path;
use tokio::process::Command;

use crate::job::QualitySettings;

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
    /// carrying its own encoder delay in an edit list the demuxer does not
    /// read.
    ///
    /// What MPEG-TS cannot do is carry an encoder delay at all, since it has no
    /// edit list and no negative timestamp. That is why a chunk holds the
    /// picture alone: an AAC frame of priming per chunk would have to occupy
    /// real time on the chunk's own timeline, and the sound would step at every
    /// boundary. See
    /// [`FFmpegProcessor::process_in_chunks`](super::FFmpegProcessor::process_in_chunks).
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

    /// Keep every audio track the source has and encode one more beside them.
    ///
    /// `originals` is how many audio streams the source carries, which is the
    /// output index the new track lands on - the copies occupy everything
    /// below it.
    ///
    /// The disposition is cleared explicitly, and that is not tidiness.
    /// Measured: a track mapped from a default one inherits `default`, so
    /// leaving it alone produces a file with two default audio tracks and no
    /// answer to which one a client picks. The original keeps the flag; the
    /// track we invented does not ask to be chosen.
    pub fn with_added_audio_track(
        mut self,
        originals: usize,
        quality_settings: &QualitySettings,
    ) -> Self {
        self.output_options.extend_from_slice(&[
            "-c:a".to_string(),
            "copy".to_string(),
            format!("-c:a:{originals}"),
            "aac".to_string(),
            format!("-b:a:{originals}"),
            quality_settings.ffmpeg_audio_bitrate.clone(),
        ]);

        self.output_options
            .extend_from_slice(&[format!("-disposition:a:{originals}"), "0".to_string()]);

        self
    }

    /// Mix one output audio stream down to `cap`.
    ///
    /// Emitted per stream and only where the source is above the cap, because
    /// `-ac` sets a channel count rather than limiting one: applied to a track
    /// already inside the cap it upmixes, inventing channels nothing recorded.
    /// A cap is a maximum, so a source below it is left exactly as it is.
    pub fn with_audio_channel_cap(mut self, stream: usize, cap: u32) -> Self {
        self.output_options
            .extend_from_slice(&[format!("-ac:a:{stream}"), cap.to_string()]);
        self
    }

    /// Write the encoder's reorder delay into the samples, not only into the
    /// edit list.
    ///
    /// x264 at any preset that reorders frames emits the first picture with a
    /// decode timestamp below its presentation timestamp. The MP4 muxer's
    /// default answer is the video track's edit list `media_time`, and nothing
    /// then says so in the media timeline - so the file is only in sync on a
    /// player that honours `elst`. One that does not plays the picture
    /// `has_b_frames` frame intervals late: measured 80ms at 25fps and 200ms at
    /// 10fps, on all three MP4s this project writes.
    ///
    /// `+negative_cts_offsets` says the same thing as a signed composition
    /// offset instead, which lives in the sample table every player reads. It
    /// is not a second answer to the first. Measured with FFmpeg 9.0 on
    /// Windows, an edit-list-honouring reading of the output is unchanged by
    /// it: the same first video packet at 0.000000 and the same first audio
    /// packet at -0.021333, while the edit-list-ignoring reading comes back
    /// from 0.080000 to 0.000000. So it corrects the players that were wrong
    /// and leaves the ones that were right alone.
    ///
    /// `-movflags` accumulates across repeats rather than replacing, so this
    /// and [`Self::with_faststart`] can both be asked for; measured, the
    /// remux's `moov` still comes out in front of its `mdat`.
    pub fn with_negative_composition_offsets(mut self) -> Self {
        self.output_options
            .extend_from_slice(&["-movflags".to_string(), "+negative_cts_offsets".to_string()]);
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

/// Render a number of seconds for an FFmpeg time argument.
pub(super) fn format_seconds(seconds: f64) -> String {
    format!("{seconds:.3}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

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
}
