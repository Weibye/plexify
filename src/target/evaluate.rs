//! What one file needs before a given client will Direct Play it.
//!
//! A pure function of a [`MediaProbe`] (facts about a file) and a
//! [`PlaybackTarget`] (beliefs about a device). It proposes nothing and touches
//! nothing.

use serde::{Deserialize, Serialize};

use super::{PlaybackTarget, Provenance};
use crate::probe::{AudioStream, MediaProbe};

/// What fixing a file costs. The two are not comparable: re-encoding video on
/// a Pi 4 runs slower than realtime, while a remux copies the video bitstream
/// untouched. Tallying them together hides the difference that matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cost {
    Remux,
    Reencode,
}

/// The property a check was about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Field {
    NoVideoStream,
    VideoCodec,
    VideoProfile,
    VideoLevel,
    BitDepth,
    PixelFormat,
    Resolution,
    RefFrames,
    AudioCodec,
    AudioChannels,
    Subtitles,
}

impl Field {
    /// What it costs to make this property conform.
    ///
    /// Anything the video bitstream itself carries has to be re-encoded.
    /// Audio and subtitle tracks are containers' worth of work: swap the track
    /// and copy the video across.
    pub fn cost(self) -> Cost {
        match self {
            Field::NoVideoStream
            | Field::VideoCodec
            | Field::VideoProfile
            | Field::VideoLevel
            | Field::BitDepth
            | Field::PixelFormat
            | Field::Resolution
            | Field::RefFrames => Cost::Reencode,
            Field::AudioCodec | Field::AudioChannels | Field::Subtitles => Cost::Remux,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Field::NoVideoStream => "no video stream",
            Field::VideoCodec => "video codec",
            Field::VideoProfile => "video profile",
            Field::VideoLevel => "level",
            Field::BitDepth => "bit depth",
            Field::PixelFormat => "pixel format",
            Field::Resolution => "resolution",
            Field::RefFrames => "reference frames",
            Field::AudioCodec => "audio codec",
            Field::AudioChannels => "audio channels",
            Field::Subtitles => "subtitles",
        }
    }
}

/// One check, and the provenance of the target's claim it was checked against.
///
/// `source` describes the claim, not the file: an observed rejection means the
/// device was watched refusing this, and an assumed one means nobody has tried.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub field: Field,
    /// What the file has.
    pub value: String,
    /// What the client is claimed to accept. `None` where the claim is about
    /// this exact value, as it is for a codec: there the two would repeat.
    pub claim: Option<String>,
    pub source: Provenance,
}

impl Finding {
    /// What the file has, against what the client takes. The line a reader
    /// acts on.
    pub fn describe(&self) -> String {
        match &self.claim {
            Some(claim) => format!("{}: {} (accepts {claim})", self.field.label(), self.value),
            None => format!("{}: {}", self.field.label(), self.value),
        }
    }

    /// The belief alone, so a report can tally which claims its conclusions
    /// rest on rather than listing one line per file.
    pub fn claim(&self) -> String {
        format!(
            "{}: {}",
            self.field.label(),
            self.claim.as_ref().unwrap_or(&self.value)
        )
    }
}

/// What a client needs done to a file before it will Direct Play it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Conformance {
    Conforms {
        unverified: Vec<Finding>,
    },
    /// Audio or subtitles have to change; the video bitstream is copied.
    Remux {
        reasons: Vec<Finding>,
        unverified: Vec<Finding>,
    },
    /// The video itself has to be re-encoded.
    Reencode {
        reasons: Vec<Finding>,
        unverified: Vec<Finding>,
    },
}

impl Conformance {
    pub fn reasons(&self) -> &[Finding] {
        match self {
            Conformance::Conforms { .. } => &[],
            Conformance::Remux { reasons, .. } | Conformance::Reencode { reasons, .. } => reasons,
        }
    }

    /// Checks that passed only because an untested claim allowed them. A
    /// verdict resting on these is a prediction, not a measurement.
    pub fn unverified(&self) -> &[Finding] {
        match self {
            Conformance::Conforms { unverified }
            | Conformance::Remux { unverified, .. }
            | Conformance::Reencode { unverified, .. } => unverified,
        }
    }

    pub fn cost(&self) -> Option<Cost> {
        match self {
            Conformance::Conforms { .. } => None,
            Conformance::Remux { .. } => Some(Cost::Remux),
            Conformance::Reencode { .. } => Some(Cost::Reencode),
        }
    }
}

/// Everything the checks below have to say about one file.
#[derive(Default)]
struct Findings {
    reasons: Vec<Finding>,
    unverified: Vec<Finding>,
}

impl Findings {
    /// Record one check's outcome. A pass that rests on an assumed claim is
    /// kept: it is the case where this tool is most likely to be wrong.
    fn check(
        &mut self,
        (passed, source): (bool, Provenance),
        field: Field,
        value: String,
        claim: Option<String>,
    ) {
        let finding = Finding {
            field,
            value,
            claim,
            source,
        };

        if !passed {
            self.reasons.push(finding);
        } else if source.is_assumed() {
            self.unverified.push(finding);
        }
    }
}

/// What `target` needs done to the file `probe` describes.
pub fn evaluate(probe: &MediaProbe, target: &PlaybackTarget) -> Conformance {
    let mut findings = Findings::default();

    video(probe, target, &mut findings);
    audio(probe, target, &mut findings);
    subtitles(probe, target, &mut findings);

    let Findings {
        reasons,
        unverified,
    } = findings;

    if reasons.is_empty() {
        Conformance::Conforms { unverified }
    } else if reasons.iter().any(|r| r.field.cost() == Cost::Reencode) {
        Conformance::Reencode {
            reasons,
            unverified,
        }
    } else {
        Conformance::Remux {
            reasons,
            unverified,
        }
    }
}

fn video(probe: &MediaProbe, target: &PlaybackTarget, findings: &mut Findings) {
    let envelope = &target.video;

    let Some(video) = &probe.video else {
        // Not a fixable property, but a file in a video library that has no
        // video in it is not something to report as conforming either.
        findings.check(
            (false, Provenance::Observed),
            Field::NoVideoStream,
            "none".to_string(),
            None,
        );
        return;
    };

    let (plays, source) = envelope.codecs.verdict(&video.codec);
    findings.check(
        (plays, source),
        Field::VideoCodec,
        video.codec.clone(),
        None,
    );

    // Profile and level only mean something once the codec is one the client
    // decodes; "High profile AV1" is not a second reason, it is the same one.
    if plays {
        if let (Some(accepted), Some(profile)) =
            (envelope.profiles.get(&video.codec), video.profile.as_ref())
        {
            let source = accepted.provenance_of(profile);
            findings.check(
                (source.is_some(), source.unwrap_or(Provenance::Assumed)),
                Field::VideoProfile,
                format!("{} {profile}", video.codec),
                None,
            );
        }

        if let (Some(limit), Some(level)) = (envelope.max_level.get(&video.codec), video.level) {
            findings.check(
                limit.verdict(level.max(0) as u32),
                Field::VideoLevel,
                tenths(level.max(0) as u32),
                Some(format!("up to {}", tenths(limit.value))),
            );
        }
    }

    if let Some(depth) = video.bit_depth {
        findings.check(
            envelope.max_bit_depth.verdict(depth),
            Field::BitDepth,
            format!("{depth}-bit"),
            Some(format!("up to {}-bit", envelope.max_bit_depth.value)),
        );
    }

    if let Some(pixel_format) = &video.pixel_format {
        findings.check(
            envelope.pixel_formats.verdict(pixel_format),
            Field::PixelFormat,
            pixel_format.clone(),
            None,
        );
    }

    if let Some(height) = video.height {
        findings.check(
            envelope.max_height.verdict(height),
            Field::Resolution,
            format!("{height}p"),
            Some(format!("up to {}p", envelope.max_height.value)),
        );
    }

    if let Some(refs) = video.ref_frames {
        findings.check(
            envelope.max_ref_frames.verdict(refs),
            Field::RefFrames,
            refs.to_string(),
            Some(format!("up to {}", envelope.max_ref_frames.value)),
        );
    }
}

/// A file conforms on audio when *one* of its tracks plays; the client picks
/// that one. A file with no audio at all needs nothing done to it - there is no
/// track to swap - so it is not reported.
fn audio(probe: &MediaProbe, target: &PlaybackTarget, findings: &mut Findings) {
    let envelope = &target.audio;
    if probe.audio.is_empty() {
        return;
    }

    let playable = probe
        .audio
        .iter()
        .filter_map(|stream| {
            let (plays, codec_source) = envelope.codecs.verdict(&stream.codec);
            let (fits, channel_source) = channels(stream, target);

            (plays && fits).then(|| (codec_source.weakest(channel_source), stream))
        })
        .min_by_key(|(source, _)| *source);

    // Codec and channel count are recorded separately: a track that plays on
    // an assumed channel cap is a different unverified belief from one that
    // plays on an assumed codec, and lumping them mislabels which is untested.
    if let Some((_, stream)) = playable {
        findings.check(
            envelope.codecs.verdict(&stream.codec),
            Field::AudioCodec,
            stream.codec.clone(),
            None,
        );
        if let Some(count) = stream.channels {
            findings.check(
                channels(stream, target),
                Field::AudioChannels,
                count.to_string(),
                Some(format!("up to {}", envelope.max_channels.value)),
            );
        }
        return;
    }

    // A 5.1 track in a codec the client decodes fails on its channel count, and
    // saying "only aac" there would send the reader looking at the wrong thing.
    let too_many_channels = probe
        .audio
        .iter()
        .filter(|stream| envelope.codecs.verdict(&stream.codec).0)
        .filter_map(|stream| Some((stream.channels?, channels(stream, target).1)))
        .min_by_key(|(count, _)| *count);

    if let Some((count, source)) = too_many_channels {
        findings.check(
            (false, source),
            Field::AudioChannels,
            count.to_string(),
            Some(format!("up to {}", envelope.max_channels.value)),
        );
        return;
    }

    // Rejecting on measured evidence only if every track was measured to fail.
    let source = probe
        .audio
        .iter()
        .map(|stream| envelope.codecs.verdict(&stream.codec).1)
        .fold(Provenance::Observed, Provenance::weakest);

    let mut codecs: Vec<&str> = probe.audio.iter().map(|s| s.codec.as_str()).collect();
    codecs.sort_unstable();
    codecs.dedup();

    findings.check(
        (false, source),
        Field::AudioCodec,
        format!("only {}", codecs.join("/")),
        None,
    );
}

/// A track with no channel count stated is not a reason to fail a file, but
/// saying it fits is a guess.
fn channels(stream: &AudioStream, target: &PlaybackTarget) -> (bool, Provenance) {
    match stream.channels {
        Some(count) => target.audio.max_channels.verdict(count),
        None => (true, Provenance::Assumed),
    }
}

/// A subtitle format the client cannot render as an overlay is burned into the
/// picture, which costs a full video encode at playback time. Stripping the
/// track is a remux, so that is what this costs us.
fn subtitles(probe: &MediaProbe, target: &PlaybackTarget, findings: &mut Findings) {
    let mut burned: Vec<(&str, Provenance)> = probe
        .subtitles
        .iter()
        .filter_map(|stream| {
            let source = target.subtitles.burns_in.provenance_of(&stream.codec)?;
            Some((stream.codec.as_str(), source))
        })
        .collect();
    burned.sort_unstable();
    burned.dedup();

    for (codec, source) in burned {
        findings.check(
            (false, source),
            Field::Subtitles,
            format!("{codec} is burned in"),
            None,
        );
    }
}

/// FFprobe's integer level as it is written down: 41 is 4.1.
fn tenths(level: u32) -> String {
    format!("{}.{}", level / 10, level % 10)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{AudioStream, SubtitleStream, VideoStream};

    fn chromecast() -> PlaybackTarget {
        PlaybackTarget::builtin("chromecast-gen2-3")
            .unwrap()
            .unwrap()
    }

    fn lg() -> PlaybackTarget {
        PlaybackTarget::builtin("lg-cx-webos").unwrap().unwrap()
    }

    /// What plexify's own transcoder is supposed to produce.
    fn plexify_output() -> MediaProbe {
        MediaProbe {
            container: Some("mov,mp4,m4a".to_string()),
            video: Some(VideoStream {
                codec: "h264".to_string(),
                profile: Some("high".to_string()),
                level: Some(41),
                pixel_format: Some("yuv420p".to_string()),
                bit_depth: Some(8),
                ref_frames: Some(4),
                width: Some(1920),
                height: Some(1080),
            }),
            audio: vec![AudioStream {
                codec: "aac".to_string(),
                channels: Some(2),
                language: Some("eng".to_string()),
            }],
            ..Default::default()
        }
    }

    fn claims(conformance: &Conformance) -> Vec<String> {
        conformance.reasons().iter().map(Finding::claim).collect()
    }

    #[test]
    fn a_conforming_file_needs_nothing_done_to_it() {
        let verdict = evaluate(&plexify_output(), &chromecast());

        assert_eq!(verdict.cost(), None, "{:?}", claims(&verdict));
    }

    /// The measured Chromecast fact: AC3 is refused in every layout, stereo
    /// included, and the fix is an audio swap rather than a re-encode.
    #[test]
    fn ac3_stereo_costs_a_remux_on_the_chromecast_and_that_is_measured() {
        let mut probe = plexify_output();
        probe.audio = vec![AudioStream {
            codec: "ac3".to_string(),
            channels: Some(2),
            ..Default::default()
        }];

        let verdict = evaluate(&probe, &chromecast());

        assert_eq!(verdict.cost(), Some(Cost::Remux));
        assert_eq!(verdict.reasons()[0].field, Field::AudioCodec);
        assert_eq!(verdict.reasons()[0].source, Provenance::Observed);
    }

    /// The measured LG fact. The client burns the track in and re-encodes to do
    /// it, but what *we* have to do is drop the track, and that is a remux.
    #[test]
    fn mov_text_subtitles_cost_a_remux_on_the_lg() {
        let mut probe = plexify_output();
        probe.subtitles = vec![SubtitleStream {
            codec: "mov_text".to_string(),
            language: Some("eng".to_string()),
        }];

        let verdict = evaluate(&probe, &lg());

        assert_eq!(verdict.cost(), Some(Cost::Remux));
        assert_eq!(verdict.reasons()[0].field, Field::Subtitles);
        assert_eq!(verdict.reasons()[0].source, Provenance::Observed);
        // The same file is fine on the Chromecast, which renders it as text.
        assert_eq!(evaluate(&probe, &chromecast()).cost(), None);
    }

    #[test]
    fn hevc_10_bit_is_a_reencode_for_the_chromecast_and_plays_on_the_lg() {
        let mut probe = plexify_output();
        probe.video = Some(VideoStream {
            codec: "hevc".to_string(),
            profile: Some("main 10".to_string()),
            level: None,
            pixel_format: Some("yuv420p10le".to_string()),
            bit_depth: Some(10),
            ref_frames: Some(4),
            width: Some(3840),
            height: Some(2160),
        });

        let chromecast = evaluate(&probe, &chromecast());
        assert_eq!(chromecast.cost(), Some(Cost::Reencode));
        assert!(claims(&chromecast).iter().any(|c| c.contains("hevc")));

        assert_eq!(evaluate(&probe, &lg()).cost(), None);
    }

    /// A codec nobody has watched play is not a conformance the tool should
    /// state flatly.
    #[test]
    fn vp9_passes_the_chromecast_only_on_an_assumption() {
        let mut probe = plexify_output();
        probe.video = Some(VideoStream {
            codec: "vp9".to_string(),
            profile: Some("profile 0".to_string()),
            ..plexify_output().video.unwrap()
        });

        let verdict = evaluate(&probe, &chromecast());

        assert_eq!(verdict.cost(), None);
        assert!(verdict
            .unverified()
            .iter()
            .any(|f| f.field == Field::VideoCodec && f.value == "vp9"));
    }

    /// Profile and level are not reported for a codec the client cannot decode
    /// at all - one file, one reason.
    #[test]
    fn an_undecodable_codec_produces_one_reason_not_three() {
        let mut probe = plexify_output();
        probe.video = Some(VideoStream {
            codec: "av1".to_string(),
            profile: Some("main".to_string()),
            level: Some(60),
            ..plexify_output().video.unwrap()
        });

        let verdict = evaluate(&probe, &chromecast());

        assert_eq!(verdict.reasons().len(), 1);
        assert_eq!(verdict.reasons()[0].field, Field::VideoCodec);
    }

    #[test]
    fn a_level_above_the_cap_needs_a_reencode() {
        let mut probe = plexify_output();
        probe.video.as_mut().unwrap().level = Some(50);

        let verdict = evaluate(&probe, &chromecast());

        assert_eq!(verdict.cost(), Some(Cost::Reencode));
        assert!(claims(&verdict).iter().any(|c| c.contains("4.1")));
    }

    /// The codec is one the client decodes; the layout is not. Reporting this
    /// as "only aac" would point the reader at the wrong property.
    #[test]
    fn a_51_track_in_an_accepted_codec_fails_on_its_channel_count() {
        let mut probe = plexify_output();
        probe.audio[0].channels = Some(6);

        let verdict = evaluate(&probe, &chromecast());

        assert_eq!(verdict.cost(), Some(Cost::Remux));
        assert_eq!(verdict.reasons()[0].field, Field::AudioChannels);
        assert_eq!(
            verdict.reasons()[0].describe(),
            "audio channels: 6 (accepts up to 2)"
        );
    }

    /// The client plays whichever track suits it, so one good track is enough.
    #[test]
    fn a_file_conforms_when_any_one_of_its_tracks_plays() {
        let mut probe = plexify_output();
        probe.audio = vec![
            AudioStream {
                codec: "ac3".to_string(),
                channels: Some(6),
                ..Default::default()
            },
            AudioStream {
                codec: "aac".to_string(),
                channels: Some(2),
                ..Default::default()
            },
        ];

        assert_eq!(evaluate(&probe, &chromecast()).cost(), None);
    }

    /// Nothing can be remuxed into a file that has no audio, and a silent file
    /// still Direct Plays.
    #[test]
    fn a_file_with_no_audio_is_not_reported_as_needing_work() {
        let mut probe = plexify_output();
        probe.audio.clear();

        assert_eq!(evaluate(&probe, &chromecast()).cost(), None);
    }

    #[test]
    fn a_file_with_no_video_stream_is_reported_rather_than_passed() {
        let mut probe = plexify_output();
        probe.video = None;

        let verdict = evaluate(&probe, &chromecast());

        assert_eq!(verdict.cost(), Some(Cost::Reencode));
        assert_eq!(verdict.reasons()[0].field, Field::NoVideoStream);
    }

    /// The expensive verdict wins: a file that needs its video re-encoded is
    /// not filed under the cheap bucket because its audio also needs a swap.
    #[test]
    fn a_video_reason_outranks_an_audio_one() {
        let mut probe = plexify_output();
        probe.video.as_mut().unwrap().codec = "mpeg4".to_string();
        probe.audio[0].codec = "ac3".to_string();

        let verdict = evaluate(&probe, &chromecast());

        assert_eq!(verdict.cost(), Some(Cost::Reencode));
        assert_eq!(verdict.reasons().len(), 2);
    }
}
