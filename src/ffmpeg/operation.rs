//! The codec options and stream mappings a job's operation asks for.

use super::FFmpegCommandBuilder;
use crate::job::{AudioAction, Job, Operation};

/// Every stream an output takes: the picture from input 0, every audio track
/// from `source`, the one an added track is derived from, and `subtitles`.
///
/// Input 0 is the source for a one-pass encode and the list of chunks for the
/// join that follows a chunked one, and in both it is where the picture comes
/// from. On the one-pass path everything is input 0; the join takes only the
/// picture from the chunks, and reads the sound and the subtitles from the
/// source.
///
/// The derived track is mapped after the copies, which is what makes
/// `-c:a:{originals}` name it. First rather than loudest or longest, because
/// the first audio track is the one a container conventionally leads with, and
/// nothing here has measured a better rule. Every path maps through here
/// because FFmpeg only warns about codec options for a track nothing mapped:
/// the job succeeds, and the output lacks the track it was run to add.
///
/// The picture is `0:V`, which leaves out cover art: an MKV carries it as a
/// video stream marked as an attached picture. Plex shows its own poster, and
/// mapped as video the cover fails a re-encode's MP4 mux or rides a remux as a
/// second video track.
pub(super) fn mappings(source: usize, adds_a_track: bool, subtitles: &[String]) -> Vec<String> {
    let mut mappings = vec!["0:V".to_string(), format!("{source}:a")];
    if adds_a_track {
        mappings.push(format!("{source}:a:0"));
    }
    mappings.extend_from_slice(subtitles);
    mappings
}

/// The codec options a job's operation asks for.
///
/// `source_channels` is how many channels each of the source's audio streams
/// carries. A cap is only ever applied to a stream that exceeds it, which is
/// what makes `channels` a maximum rather than an instruction.
pub(super) fn with_operation(
    builder: FFmpegCommandBuilder,
    job: &Job,
    source_channels: &[u32],
) -> FFmpegCommandBuilder {
    match job.operation {
        // The index goes to the front only on this path. A remux exists because
        // the container is what the player choked on, and leaving the new one
        // indexed from the end would answer the wrong half of the question.
        Operation::Remux { audio, .. } => {
            let builder = builder.with_video_copy().with_faststart();
            match audio {
                AudioAction::Copy => builder.with_audio_copy(),
                // A file with no audio has nothing to add a track beside, and
                // nothing to derive one from.
                AudioAction::Add { .. } if source_channels.is_empty() => builder.with_audio_copy(),
                AudioAction::Add { channels } => {
                    let originals = source_channels.len();
                    let builder = builder.with_added_audio_track(originals, &job.quality_settings);
                    // The new track is derived from the first, so the first is
                    // the layout the cap has to bind against.
                    match capped(channels, source_channels[0]) {
                        Some(cap) => builder.with_audio_channel_cap(originals, cap),
                        None => builder,
                    }
                }
                AudioAction::Transcode { channels } => {
                    with_encoded_audio(builder, job, channels, source_channels)
                }
            }
        }
        Operation::Reencode { channels } => with_encoded_audio(
            builder.with_video_encoding(&job.quality_settings),
            job,
            channels,
            source_channels,
        ),
    }
}

/// Encode every audio stream, capping the ones that carry more channels than
/// the client will take.
///
/// Shared by the two paths that write a new audio track, because a client that
/// refuses 5.1 refuses it whether the picture beside it was copied or encoded.
/// Re-encoding the video does not make the audio somebody else's problem.
pub(super) fn with_encoded_audio(
    builder: FFmpegCommandBuilder,
    job: &Job,
    cap: Option<u32>,
    source_channels: &[u32],
) -> FFmpegCommandBuilder {
    let builder = builder.with_audio_encoding(&job.quality_settings);

    // Per stream, because a commentary track in mono beside a 5.1 feature is
    // not a reason to upmix the commentary.
    source_channels
        .iter()
        .enumerate()
        .fold(builder, |builder, (stream, &carried)| {
            match capped(cap, carried) {
                Some(cap) => builder.with_audio_channel_cap(stream, cap),
                None => builder,
            }
        })
}

/// The cap to apply to a stream carrying `carried` channels, or `None` where it
/// is already inside the cap and nothing should be said about it.
fn capped(cap: Option<u32>, carried: u32) -> Option<u32> {
    cap.filter(|cap| carried > *cap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffmpeg::test_support::*;
    use crate::ffmpeg::{FFmpegProcessor, SubtitleSelection};
    use crate::job::{MediaFileType, PostProcessingSettings, QualitySettings, SubtitleAction};
    use std::path::Path;
    use std::process::Stdio;
    use tempfile::TempDir;

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
        let joined = with_operation(FFmpegCommandBuilder::new(), &job, &[])
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

        let joined = with_operation(FFmpegCommandBuilder::new(), &job, &[6])
            .with_output("/work/show.mp4")
            .build()
            .join(" ");

        assert!(joined.contains("-c:v copy"), "{joined}");
        // A 5.1 track in a codec the client decodes fails on its layout, and
        // the fix for that is a downmix, not a codec swap.
        assert!(joined.contains("-c:a aac -b:a 128k"), "{joined}");
        assert!(joined.contains("-ac:a:0 2"), "{joined}");
    }

    /// A re-encode writes a new audio track too, so it has to keep it inside
    /// what the client takes. 961 files are a re-encode for the Chromecast and
    /// every 5.1 one of them came out 5.1 AAC: a codec it decodes, at a layout
    /// it refuses.
    #[test]
    fn a_re_encode_caps_its_audio_the_way_a_remux_does() {
        let job = job_for(
            "/media/film.mkv",
            MediaFileType::Mkv,
            Operation::Reencode { channels: Some(2) },
        );

        let joined = with_operation(FFmpegCommandBuilder::new(), &job, &[6])
            .with_output("/work/film.mp4")
            .build()
            .join(" ");

        assert!(joined.contains("-c:v libx264"), "{joined}");
        assert!(joined.contains("-c:a aac"), "{joined}");
        assert!(joined.contains("-ac:a:0 2"), "{joined}");
    }

    /// `-ac` sets a channel count rather than limiting one, so a cap applied
    /// to a source already inside it invents channels nothing recorded.
    ///
    /// Measured on the shape this used to build: a stereo Opus source folded
    /// to a cap of six came out `opus,2` plus `aac,6`. Every Opus file in this
    /// library is stereo or mono, and the LG's cap is six, so that was the
    /// whole population.
    #[test]
    fn a_cap_the_source_is_already_inside_changes_nothing() {
        let transcode = job_for(
            "/media/show.mkv",
            MediaFileType::Mkv,
            Operation::Remux {
                audio: AudioAction::Transcode { channels: Some(6) },
                subtitles: SubtitleAction::Keep,
            },
        );

        let joined = with_operation(FFmpegCommandBuilder::new(), &transcode, &[2])
            .with_output("/work/show.mp4")
            .build()
            .join(" ");
        assert!(!joined.contains("-ac"), "stereo must stay stereo: {joined}");

        let added = job_for(
            "/media/show.mkv",
            MediaFileType::Mkv,
            Operation::Remux {
                audio: AudioAction::Add { channels: Some(6) },
                subtitles: SubtitleAction::Keep,
            },
        );

        let joined = with_operation(FFmpegCommandBuilder::new(), &added, &[2])
            .with_output("/work/show.mp4")
            .build()
            .join(" ");
        assert!(joined.contains("-c:a:1 aac"), "{joined}");
        assert!(
            !joined.contains("-ac"),
            "a track derived from stereo is stereo: {joined}"
        );
    }

    /// A cap binds one stream and not another, so it is stated per stream.
    #[test]
    fn a_cap_binds_only_the_streams_that_exceed_it() {
        let job = job_for(
            "/media/show.mkv",
            MediaFileType::Mkv,
            Operation::Remux {
                audio: AudioAction::Transcode { channels: Some(2) },
                subtitles: SubtitleAction::Keep,
            },
        );

        // A 5.1 feature, a stereo dub, and a mono commentary.
        let joined = with_operation(FFmpegCommandBuilder::new(), &job, &[6, 2, 1])
            .with_output("/work/show.mp4")
            .build()
            .join(" ");

        assert!(joined.contains("-ac:a:0 2"), "{joined}");
        assert!(!joined.contains("-ac:a:1"), "stereo is inside it: {joined}");
        assert!(
            !joined.contains("-ac:a:2"),
            "and a mono commentary is not a reason to upmix it: {joined}"
        );
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

        let joined = with_operation(FFmpegCommandBuilder::new(), &job, &[])
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
        assert!(Operation::Reencode { channels: None }.keeps_subtitles());
    }

    #[test]
    fn a_re_encode_is_left_exactly_as_it_was() {
        let job = job_for(
            "/media/show.mkv",
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
        );
        let joined = with_operation(FFmpegCommandBuilder::new(), &job, &[])
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

    #[test]
    fn maps_every_stream_rather_than_the_first_of_each() {
        let selection = SubtitleSelection {
            kept: vec![0, 1],
            dropped: Vec::new(),
        };

        let args = FFmpegCommandBuilder::new()
            .with_input("/in.mkv")
            .with_stream_mapping(&mappings(0, false, &selection.mappings(0)))
            .with_output("/out.mp4")
            .build();

        let mappings: Vec<&String> = args
            .iter()
            .skip_while(|arg| *arg != "-map")
            .filter(|arg| !arg.starts_with('-'))
            .collect();

        assert_eq!(
            mappings,
            vec!["0:V", "0:a", "0:s:0", "0:s:1", "/out.mp4"],
            "audio is mapped as a group, and every subtitle stream that can be carried by name"
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

    /// The whole of #161, end to end: the original survives with its layout
    /// and its default flag, and the added track is stereo AAC, not default,
    /// and carries the language it inherited rather than one we chose.
    #[test]
    fn adding_a_track_keeps_the_original_and_leaves_the_new_one_unchosen() {
        if !ffmpeg_present() {
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let input = temp_dir.path().join("show.mkv");

        // AC3 5.1, which the Chromecast is measured to refuse in every layout
        // and the LG plays. Tagged, so inheritance can be checked.
        let built = std::process::Command::new("ffmpeg")
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=160x120:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=1",
                "-c:v",
                "libx264",
                "-c:a",
                "ac3",
                "-ac",
                "6",
                "-metadata:s:a:0",
                "language=eng",
                "-y",
            ])
            .arg(&input)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(built.success(), "could not build the test source");

        let job = Job::new(
            input.clone(),
            MediaFileType::Mkv,
            Operation::Remux {
                audio: AudioAction::Add { channels: Some(2) },
                subtitles: SubtitleAction::Keep,
            },
            QualitySettings::default(),
            PostProcessingSettings::default(),
            temp_dir.path(),
        );

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(FFmpegProcessor::new(false).process_job(&job, None, None))
            .expect("adding a track should succeed");

        let tracks = audio_tracks_of(&job.output_path);
        assert_eq!(
            tracks,
            vec![
                // Untouched, still the one a client is pointed at first.
                "ac3,6,1,eng".to_string(),
                // Ours: stereo, and explicitly not asking to be chosen.
                // Measured: without clearing this, it inherits `default` from
                // the track it was derived from and the file has two.
                "aac,2,0,eng".to_string(),
            ]
        );

        // The picture was never touched, which is the point of doing this as a
        // remux rather than a re-encode.
        assert_eq!(codec_of(&job.output_path, "v"), "h264");
    }

    /// The reported case, through the real pipeline: every Opus file in this
    /// library is stereo or mono, and the LG's cap is six, so the fold hands
    /// this path `Add { channels: Some(6) }` for all 342 of them. A cap above
    /// the source must do nothing at all.
    #[test]
    fn a_stereo_source_never_gains_channels_it_did_not_have() {
        if !ffmpeg_present() {
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let input = temp_dir.path().join("show.mkv");

        let built = std::process::Command::new("ffmpeg")
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=160x120:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=1",
                "-c:v",
                "libx264",
                "-c:a",
                "libopus",
                "-ac",
                "2",
                "-y",
            ])
            .arg(&input)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        if !built.success() {
            eprintln!("skipping: this FFmpeg cannot encode Opus");
            return;
        }

        let job = Job::new(
            input.clone(),
            MediaFileType::Mkv,
            Operation::Remux {
                audio: AudioAction::Add { channels: Some(6) },
                subtitles: SubtitleAction::Keep,
            },
            QualitySettings::default(),
            PostProcessingSettings::default(),
            temp_dir.path(),
        );

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(FFmpegProcessor::new(false).process_job(&job, None, None))
            .expect("adding a track should succeed");

        let tracks = audio_tracks_of(&job.output_path);
        assert_eq!(
            tracks.len(),
            2,
            "the original and the one added beside it: {tracks:?}"
        );
        assert!(tracks[0].starts_with("opus,2"), "{tracks:?}");
        assert!(
            tracks[1].starts_with("aac,2"),
            "a track derived from stereo is stereo, not 5.1 invented from it: {tracks:?}"
        );
    }

    /// What `scan --target` makes of a VP9 and Opus WebM for a client that
    /// refuses Opus. Its sidecar arrives as a second input, and the added track
    /// has to be mapped there too: FFmpeg only warns about codec options for a
    /// track nothing mapped, so the job would succeed and disable a source
    /// whose replacement still does not play.
    #[test]
    fn a_webm_that_gains_a_track_writes_it_beside_its_sidecar() {
        if !ffmpeg_present() {
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let input = temp_dir.path().join("show.webm");
        build_webm_source(&input, 2, "libvpx-vp9");

        let job = Job::new(
            input.clone(),
            MediaFileType::WebM,
            Operation::Remux {
                audio: AudioAction::Add { channels: Some(2) },
                subtitles: SubtitleAction::Keep,
            },
            QualitySettings::default(),
            PostProcessingSettings::default(),
            temp_dir.path(),
        );

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(FFmpegProcessor::new(false).process_job(&job, None, None))
            .expect("adding a track should succeed");

        assert_eq!(
            stream_codecs(&job.output_path),
            vec!["vp9", "opus", "aac", "mov_text"]
        );
    }

    /// Codec, channels, default flag and language of every audio track.
    fn audio_tracks_of(path: &Path) -> Vec<String> {
        let output = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "a",
                "-show_entries",
                "stream=codec_name,channels:stream_disposition=default:stream_tags=language",
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
