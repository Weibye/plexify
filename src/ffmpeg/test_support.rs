//! Fixtures and FFprobe readings the transcoding tests share.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use super::Chunking;
use crate::job::{Job, MediaFileType, Operation, PostProcessingSettings, QualitySettings};

/// A job for one path, carrying the operation the caller chose.
pub(super) fn job_for(path: &str, file_type: MediaFileType, operation: Operation) -> Job {
    Job::new(
        PathBuf::from(path),
        file_type,
        operation,
        QualitySettings::default(),
        PostProcessingSettings::default(),
        Path::new("/media"),
    )
}

/// Chunking small enough that a few seconds of test footage exercises it.
pub(super) const TEST_CHUNKING: Chunking = Chunking {
    chunk_seconds: 2.0,
    min_source_seconds: 3.0,
};

/// Build a short MKV carrying video, audio and one subtitle track.
pub(super) fn build_chunking_source(path: &Path, seconds: u32) {
    build_source(
        path,
        seconds,
        "1\n00:00:00,500 --> 00:00:01,500\nfirst line\n\n\
             2\n00:00:01,600 --> 00:00:03,000\nspanning a chunk boundary\n",
    )
}

/// The same, with subtitles the caller chooses.
pub(super) fn build_source(path: &Path, seconds: u32, srt: &str) {
    build_source_with_audio(path, seconds, srt, "aac")
}

/// The same again, with the source's audio codec the caller's choice.
///
/// Which codec that is decides whether the container starts before zero,
/// and one test turns on that - see
/// `the_one_pass_encode_starts_the_output_where_the_source_starts`.
pub(super) fn build_source_with_audio(path: &Path, seconds: u32, srt: &str, audio_codec: &str) {
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

/// The presentation timestamps of one stream's packets, in order.
pub(super) fn packet_times(path: &Path, stream: &str) -> Vec<f64> {
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
pub(super) fn stream_codecs(path: &Path) -> Vec<String> {
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

pub(super) fn chunking_job(input: &Path) -> Job {
    Job::new(
        input.to_path_buf(),
        MediaFileType::Mkv,
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

/// The `pts_time,duration_time` of every subtitle event in a file.
pub(super) fn subtitle_timings(path: &Path) -> Vec<(f64, f64)> {
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

/// Build a short WebM with the `.vtt` beside it that a WebM job exists to
/// attach.
///
/// VP8 and Opus rather than H.264 and AAC, because that is what a WebM in
/// this library holds and the chunk path seeks it: what `-ss` lands on in a
/// VP8 stream is not what the MKV fixtures measure. `-deadline realtime
/// -cpu-used 8` is libvpx's fastest setting, which is all a few seconds of
/// `testsrc` needs. The video codec is the caller's, because a remux
/// carries it into the MP4 untouched, and VP9 is the one an MP4 holds.
pub(super) fn build_webm_source(path: &Path, seconds: u32, video_codec: &str) {
    let subtitles = path.with_extension("vtt");
    std::fs::write(
        &subtitles,
        "WEBVTT\n\n\
             00:00:00.500 --> 00:00:01.500\nfirst line\n\n\
             00:00:01.600 --> 00:00:03.000\nspanning a chunk boundary\n",
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
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-c:v",
            video_codec,
            "-deadline",
            "realtime",
            "-cpu-used",
            "8",
            "-b:v",
            "200k",
            "-c:a",
            "libopus",
            "-y",
        ])
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(
        built.success(),
        "could not build the WebM test source; libvpx and libopus are needed"
    );
}

/// The text of every subtitle event in a file, in order.
///
/// Read back through ASS, whose `Dialogue` lines put the text last after
/// nine fixed fields, so an event that reached the file can be told from
/// one that did not.
pub(super) fn subtitle_texts(path: &Path) -> Vec<String> {
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

/// Subtitle events as whole milliseconds, so the comparison is not about
/// float noise.
///
/// Nothing else is normalised away. Both paths take their subtitles from the
/// same source with the same flags into the same container, and neither
/// moves the timeline off zero, so the lists are the same list.
pub(super) fn subtitle_events(timings: &[(f64, f64)]) -> Vec<(i64, i64)> {
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

/// Codec type and language of every stream in a file, via ffprobe.
pub(super) fn streams_of(path: &Path) -> Vec<String> {
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
