//! What a media file actually contains, read from FFprobe.
//!
//! Everything else in plexify infers a file's type from its extension. An
//! extension cannot say what codec, profile, level or pixel format is inside,
//! and those are the properties a client bases a Direct Play decision on.
//!
//! This module reports what is there and decides nothing. What a given client
//! will accept lives in [`crate::target`].

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::process::Command;

/// The streams and container properties of one media file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MediaProbe {
    pub container: Option<String>,
    pub duration_seconds: Option<f64>,
    /// Container bitrate in bits per second.
    pub bit_rate: Option<u64>,
    /// The first video stream. A file may carry cover art as a second one, and
    /// no client picks that to play.
    pub video: Option<VideoStream>,
    pub audio: Vec<AudioStream>,
    pub subtitles: Vec<SubtitleStream>,
}

/// Codec names, profiles and pixel formats are lowercased on the way in.
/// FFprobe's casing varies by codec - `High` for H.264, `Main 10` for HEVC -
/// and a target envelope should not have to spell each variant.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VideoStream {
    pub codec: String,
    pub profile: Option<String>,
    /// FFprobe's integer encoding of the codec level: 41 is H.264 level 4.1.
    pub level: Option<i64>,
    pub pixel_format: Option<String>,
    pub bit_depth: Option<u32>,
    /// Reference frames. A decoder with too small a buffer cannot play the
    /// file even when codec, profile and level all fit.
    pub ref_frames: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioStream {
    pub codec: String,
    pub channels: Option<u32>,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubtitleStream {
    pub codec: String,
    pub language: Option<String>,
}

/// Read one file with FFprobe.
pub fn probe(path: &Path) -> Result<MediaProbe> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()
        .with_context(|| format!("could not run ffprobe on {}", path.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.lines().last().unwrap_or("ffprobe failed").trim();
        return Err(anyhow!("ffprobe rejected {}: {detail}", path.display()));
    }

    parse(&String::from_utf8_lossy(&output.stdout))
        .with_context(|| format!("could not read what ffprobe said about {}", path.display()))
}

/// Build a probe from FFprobe's JSON.
///
/// Separate from running the process so the mapping can be tested against
/// captured output, without a media file and without FFmpeg installed.
pub fn parse(json: &str) -> Result<MediaProbe> {
    let root: Value = serde_json::from_str(json).context("ffprobe did not return JSON")?;
    let format = root.get("format");
    let streams = root
        .get("streams")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();

    let of_type = |kind: &'static str| {
        streams
            .iter()
            .filter(move |s| text(s, "codec_type").as_deref() == Some(kind))
    };

    Ok(MediaProbe {
        container: format.and_then(|f| text(f, "format_name")),
        duration_seconds: format.and_then(|f| number(f, "duration")),
        bit_rate: format
            .and_then(|f| number(f, "bit_rate"))
            .filter(|b| *b > 0.0)
            .map(|b| b as u64),
        video: of_type("video").next().map(video_stream),
        audio: of_type("audio").map(audio_stream).collect(),
        subtitles: of_type("subtitle")
            .map(|s| SubtitleStream {
                codec: codec_name(s),
                language: language(s),
            })
            .collect(),
    })
}

fn video_stream(stream: &Value) -> VideoStream {
    let pixel_format = text(stream, "pix_fmt").map(|p| p.to_lowercase());

    VideoStream {
        codec: codec_name(stream),
        profile: text(stream, "profile").map(|p| p.to_lowercase()),
        // Unknown levels come back as 0 or -99 depending on the codec, and
        // both would read as "well within any cap".
        level: positive(stream, "level").map(|l| l as i64),
        // `bits_per_raw_sample` is absent on plenty of real files. The pixel
        // format still carries the depth in its name, which is a reading of
        // what the file says rather than a guess about it.
        bit_depth: positive(stream, "bits_per_raw_sample")
            .map(|d| d as u32)
            .or_else(|| depth_from_pixel_format(pixel_format.as_deref())),
        ref_frames: positive(stream, "refs").map(|r| r as u32),
        width: positive(stream, "width").map(|w| w as u32),
        height: positive(stream, "height").map(|h| h as u32),
        pixel_format,
    }
}

fn audio_stream(stream: &Value) -> AudioStream {
    AudioStream {
        codec: codec_name(stream),
        channels: positive(stream, "channels").map(|c| c as u32),
        language: language(stream),
    }
}

/// The depth a pixel format names, for the files that do not state it outright.
fn depth_from_pixel_format(pixel_format: Option<&str>) -> Option<u32> {
    let format = pixel_format?;
    for depth in [16, 14, 12, 10, 9] {
        if format.contains(&depth.to_string()) {
            return Some(depth);
        }
    }
    Some(8)
}

fn codec_name(stream: &Value) -> String {
    text(stream, "codec_name")
        .map(|c| c.to_lowercase())
        .unwrap_or_default()
}

fn language(stream: &Value) -> Option<String> {
    text(stream.get("tags")?, "language").map(|l| l.to_lowercase())
}

/// FFprobe types the same field differently between codecs and builds - a
/// level is a number, `bits_per_raw_sample` a string, a nameless profile an
/// integer - so every field is read through a form that accepts either.
fn text(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn number(value: &Value, key: &str) -> Option<f64> {
    match value.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn positive(value: &Value, key: &str) -> Option<f64> {
    number(value, key).filter(|n| *n > 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    const H264_AAC: &str = r#"{
      "streams": [
        {"codec_type": "video", "codec_name": "h264", "profile": "High", "level": 41,
         "pix_fmt": "yuv420p", "refs": 4, "width": 1920, "height": 1080},
        {"codec_type": "audio", "codec_name": "aac", "channels": 2,
         "tags": {"language": "eng"}},
        {"codec_type": "subtitle", "codec_name": "mov_text", "tags": {"language": "eng"}}
      ],
      "format": {"format_name": "mov,mp4,m4a", "duration": "1.024000", "bit_rate": "1200000"}
    }"#;

    #[test]
    fn reads_every_field_a_client_decides_on() {
        let probe = parse(H264_AAC).unwrap();
        let video = probe.video.unwrap();

        assert_eq!(video.codec, "h264");
        assert_eq!(video.profile.as_deref(), Some("high"));
        assert_eq!(video.level, Some(41));
        assert_eq!(video.pixel_format.as_deref(), Some("yuv420p"));
        assert_eq!(video.bit_depth, Some(8));
        assert_eq!(video.ref_frames, Some(4));
        assert_eq!(video.height, Some(1080));
        assert_eq!(probe.audio[0].codec, "aac");
        assert_eq!(probe.audio[0].channels, Some(2));
        assert_eq!(probe.audio[0].language.as_deref(), Some("eng"));
        assert_eq!(probe.subtitles[0].codec, "mov_text");
        assert_eq!(probe.container.as_deref(), Some("mov,mp4,m4a"));
        assert_eq!(probe.bit_rate, Some(1_200_000));
    }

    #[test]
    fn takes_bit_depth_from_the_pixel_format_when_it_is_not_stated() {
        let json = r#"{"streams": [{"codec_type": "video", "codec_name": "hevc",
                        "profile": "Main 10", "pix_fmt": "yuv420p10le"}]}"#;
        let video = parse(json).unwrap().video.unwrap();

        assert_eq!(video.bit_depth, Some(10));
        assert_eq!(video.profile.as_deref(), Some("main 10"));
    }

    #[test]
    fn reads_a_numeric_profile_and_a_string_level() {
        let json = r#"{"streams": [{"codec_type": "video", "codec_name": "vp9",
                        "profile": 0, "level": "31", "bits_per_raw_sample": "8"}]}"#;
        let video = parse(json).unwrap().video.unwrap();

        assert_eq!(video.profile.as_deref(), Some("0"));
        assert_eq!(video.level, Some(31));
        assert_eq!(video.bit_depth, Some(8));
    }

    /// An unknown level is reported as 0 or -99, and neither is a level.
    #[test]
    fn does_not_report_an_unknown_level_as_a_low_one() {
        let json = r#"{"streams": [{"codec_type": "video", "codec_name": "mpeg4",
                        "level": -99, "refs": 0}]}"#;
        let video = parse(json).unwrap().video.unwrap();

        assert_eq!(video.level, None);
        assert_eq!(video.ref_frames, None);
    }

    #[test]
    fn a_file_with_no_streams_is_a_probe_with_no_streams() {
        let probe = parse(r#"{"format": {"format_name": "matroska"}}"#).unwrap();

        assert!(probe.video.is_none());
        assert!(probe.audio.is_empty());
        assert!(probe.subtitles.is_empty());
    }

    #[test]
    fn refuses_output_that_is_not_json() {
        assert!(parse("ffprobe: command not found").is_err());
    }

    fn ffmpeg_present() -> bool {
        let available = Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);

        if !available {
            assert!(
                std::env::var("CI").is_err(),
                "FFmpeg must be installed in CI: this is the only check that the probe reads a real file rather than a fixture"
            );
            eprintln!("skipping: ffmpeg is not on PATH");
        }

        available
    }

    #[test]
    fn probes_a_real_file() {
        if !ffmpeg_present() {
            return;
        }

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("sample.mp4");
        let built = Command::new("ffmpeg")
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=320x240:rate=24",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=1",
                "-c:v",
                "libx264",
                "-profile:v",
                "high",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-ac",
                "2",
                "-y",
            ])
            .arg(&path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(built.success(), "could not build the test source");

        let probe = probe(&path).unwrap();
        let video = probe.video.expect("the file has a video stream");

        assert_eq!(video.codec, "h264");
        assert_eq!(video.profile.as_deref(), Some("high"));
        assert_eq!(video.pixel_format.as_deref(), Some("yuv420p"));
        assert_eq!(video.bit_depth, Some(8));
        assert_eq!(video.width, Some(320));
        assert!(video.level.is_some(), "h264 always reports a level");
        assert_eq!(probe.audio.len(), 1);
        assert_eq!(probe.audio[0].codec, "aac");
    }

    #[test]
    fn reports_a_file_ffprobe_cannot_read() {
        if !ffmpeg_present() {
            return;
        }

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("not-media.mp4");
        std::fs::write(&path, b"this is not a media file").unwrap();

        let error = probe(&path).unwrap_err().to_string();
        assert!(error.contains("not-media.mp4"), "got: {error}");
    }
}
