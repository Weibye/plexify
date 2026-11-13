//! What one file needs before a given client will Direct Play it: a pure
//! function of a [`MediaProbe`] (facts about a file) and a [`PlaybackTarget`]
//! (beliefs about a device).

use super::{Limit, PlaybackTarget, Provenance, Support};
use crate::probe::{AudioStream, MediaProbe, VideoStream};

/// What fixing a file costs. The two are not comparable: re-encoding video on
/// a Pi 4 runs slower than realtime, while a remux copies the video bitstream
/// untouched. Tallying them together hides the difference that matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cost {
    Remux,
    Reencode,
}

/// The property a check was about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Field {
    Container,
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
    /// Every property a client's Direct Play decision turns on.
    ///
    /// It says what evaluating a file consists of, so that the probe and the
    /// checks cannot drift apart in silence: a variant added here fails
    /// `every_field_is_reachable` until a check produces it.
    pub const ALL: [Field; 11] = [
        Field::Container,
        Field::VideoCodec,
        Field::VideoProfile,
        Field::VideoLevel,
        Field::BitDepth,
        Field::PixelFormat,
        Field::Resolution,
        Field::RefFrames,
        Field::AudioCodec,
        Field::AudioChannels,
        Field::Subtitles,
    ];

    /// What it costs to make this property conform.
    ///
    /// Anything the video bitstream itself carries has to be re-encoded.
    /// Audio and subtitle tracks are containers' worth of work: swap the track
    /// and copy the video across.
    pub fn cost(self) -> Cost {
        match self {
            Field::VideoCodec
            | Field::VideoProfile
            | Field::VideoLevel
            | Field::BitDepth
            | Field::PixelFormat
            | Field::Resolution
            | Field::RefFrames => Cost::Reencode,
            // The container is the cheapest thing that can be wrong: the
            // bitstream is copied into a new one untouched.
            Field::Container | Field::AudioCodec | Field::AudioChannels | Field::Subtitles => {
                Cost::Remux
            }
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Field::Container => "container",
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
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
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
    /// Audio only, or only cover art: nothing to judge, so neither Direct Play
    /// nor work. See D34.
    NoPicture,
}

impl Conformance {
    /// Whether the client plays the file as it is. A file with no picture
    /// needs no job, and does not Direct Play either.
    pub fn direct_plays(&self) -> bool {
        matches!(self, Conformance::Conforms { .. })
    }

    pub fn reasons(&self) -> &[Finding] {
        match self {
            Conformance::Conforms { .. } | Conformance::NoPicture => &[],
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
            Conformance::NoPicture => &[],
        }
    }

    /// What the job costs, or `None` where there is no job. That is not
    /// Direct Play: ask [`Conformance::direct_plays`].
    pub fn cost(&self) -> Option<Cost> {
        match self {
            Conformance::Conforms { .. } | Conformance::NoPicture => None,
            Conformance::Remux { .. } => Some(Cost::Remux),
            Conformance::Reencode { .. } => Some(Cost::Reencode),
        }
    }
}

/// How a finding shows a value the probe could not read.
const NOT_REPORTED: &str = "not reported";

/// Whether the client decodes a codec. One FFprobe could not name is not one
/// the client is known to play, so it is refused, on an assumption.
fn codec_verdict(codecs: &Support, codec: Option<&str>) -> (bool, Provenance) {
    codec.map_or((false, Provenance::Assumed), |codec| codecs.verdict(codec))
}

fn shown(codec: Option<&str>) -> String {
    codec.unwrap_or(NOT_REPORTED).to_string()
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

    /// A property the probe could not read. Nothing follows from an absent
    /// value, so it passes - but as an assumption, never in silence. A field
    /// skipped by an `if let Some(..)` is an unverified pass wearing the
    /// clothes of a conformance.
    fn unread(&mut self, field: Field, claim: Option<String>) {
        self.check(
            (true, Provenance::Assumed),
            field,
            NOT_REPORTED.to_string(),
            claim,
        );
    }

    /// A property the target sets no bound on for this codec. Nothing refuses
    /// it, and nothing measured that every value of it plays either, so it
    /// passes on an assumption.
    fn unbounded(&mut self, field: Field, value: Option<String>, claim: String) {
        self.check(
            (true, Provenance::Assumed),
            field,
            value.unwrap_or_else(|| NOT_REPORTED.to_string()),
            Some(claim),
        );
    }

    /// One value against one ceiling, recording an absent value rather than
    /// stepping over it.
    fn within(
        &mut self,
        value: Option<u32>,
        limit: &Limit,
        field: Field,
        show: impl Fn(u32) -> String,
        claim: String,
    ) {
        match value {
            Some(value) => self.check(limit.verdict(value), field, show(value), Some(claim)),
            None => self.unread(field, Some(claim)),
        }
    }
}

/// What `target` needs done to the file `probe` describes.
pub fn evaluate(probe: &MediaProbe, target: &PlaybackTarget) -> Conformance {
    // Its audio is not judged either: a refusal there would queue a job with
    // no picture to copy.
    let Some(video_stream) = &probe.video else {
        return Conformance::NoPicture;
    };

    let mut findings = Findings::default();

    container(probe, target, &mut findings);
    video(video_stream, target, &mut findings);
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

/// The container a client cannot index is as unplayable as a codec it cannot
/// decode, and costs a copy into a different one to fix.
///
/// FFprobe names the demuxer, not the format: an MP4 and a QuickTime file both
/// arrive as `mov,mp4,m4a,3gp,3g2,mj2`. Nothing here can tell them apart, so
/// the claim is about the whole name and is matched as one (D29).
fn container(probe: &MediaProbe, target: &PlaybackTarget, findings: &mut Findings) {
    let Some(container) = &probe.container else {
        findings.unread(Field::Container, None);
        return;
    };

    findings.check(
        target.containers.verdict(container),
        Field::Container,
        container.clone(),
        None,
    );
}

fn video(video: &VideoStream, target: &PlaybackTarget, findings: &mut Findings) {
    let envelope = &target.video;

    let codec = video.codec.as_deref();
    let (plays, source) = codec_verdict(&envelope.codecs, codec);
    findings.check((plays, source), Field::VideoCodec, shown(codec), None);

    // Profile and level only mean something once the codec is one the client
    // decodes; "High profile AV1" is not a second reason, it is the same one.
    if let (true, Some(codec)) = (plays, codec) {
        match envelope.profiles.get(codec) {
            Some(accepted) => match &video.profile {
                Some(profile) => {
                    let source = accepted.provenance_of(profile);
                    findings.check(
                        (source.is_some(), source.unwrap_or(Provenance::Assumed)),
                        Field::VideoProfile,
                        format!("{codec} {profile}"),
                        None,
                    );
                }
                None => findings.unread(Field::VideoProfile, None),
            },
            None => findings.unbounded(
                Field::VideoProfile,
                video
                    .profile
                    .as_ref()
                    .map(|profile| format!("{codec} {profile}")),
                format!("any {codec} profile"),
            ),
        }

        match envelope.max_level.get(codec) {
            Some(limit) => findings.within(
                video.level.map(|level| level.max(0) as u32),
                limit,
                Field::VideoLevel,
                tenths,
                format!("up to {}", tenths(limit.value)),
            ),
            // FFprobe's integer, as it reported it: the 41-is-4.1 reading is
            // H.264's, and HEVC writes its levels on another scale.
            None => findings.unbounded(
                Field::VideoLevel,
                video.level.map(|level| level.to_string()),
                format!("any {codec} level"),
            ),
        }
    }

    findings.within(
        video.bit_depth,
        &envelope.max_bit_depth,
        Field::BitDepth,
        |depth| format!("{depth}-bit"),
        format!("up to {}-bit", envelope.max_bit_depth.value),
    );

    match &video.pixel_format {
        Some(pixel_format) => findings.check(
            envelope.pixel_formats.verdict(pixel_format),
            Field::PixelFormat,
            pixel_format.clone(),
            None,
        ),
        None => findings.unread(Field::PixelFormat, None),
    }

    findings.within(
        video.height,
        &envelope.max_height,
        Field::Resolution,
        |height| format!("{height}p"),
        format!("up to {}p", envelope.max_height.value),
    );

    findings.within(
        video.ref_frames,
        &envelope.max_ref_frames,
        Field::RefFrames,
        |refs| refs.to_string(),
        format!("up to {}", envelope.max_ref_frames.value),
    );
}

/// A file conforms on audio when *one* of its tracks plays; the client picks
/// that one. A file with no audio at all needs nothing done to it - there is no
/// track to swap - so it is not reported.
fn audio(probe: &MediaProbe, target: &PlaybackTarget, findings: &mut Findings) {
    let envelope = &target.audio;
    if probe.audio.is_empty() {
        return;
    }
    let codec = |stream: &AudioStream| codec_verdict(&envelope.codecs, stream.codec.as_deref());

    let playable = probe
        .audio
        .iter()
        .filter_map(|stream| {
            let (plays, codec_source) = codec(stream);
            let (fits, channel_source) = channels(stream, target);

            (plays && fits).then(|| (codec_source.weakest(channel_source), stream))
        })
        .min_by_key(|(source, _)| *source);

    // Codec and channel count are recorded separately: a track that plays on
    // an assumed channel cap is a different unverified belief from one that
    // plays on an assumed codec, and lumping them mislabels which is untested.
    if let Some((_, stream)) = playable {
        findings.check(
            codec(stream),
            Field::AudioCodec,
            shown(stream.codec.as_deref()),
            None,
        );
        findings.within(
            stream.channels,
            &envelope.max_channels,
            Field::AudioChannels,
            |count| count.to_string(),
            format!("up to {}", envelope.max_channels.value),
        );
        return;
    }

    // Nothing plays, and the two ways that can be true are reported separately
    // even when both hold. A verdict that stopped at the codec could not say
    // whether the track a fix produces needs mixing down as well, and the
    // caller would have to guess: assume it does and a stereo source is
    // upmixed to a layout it never had, assume it does not and a 5.1 source
    // comes back in a layout the client refuses.

    // The codec is only the fault if it is the fault everywhere. A file whose
    // AAC track is 5.1 and whose AC3 track is stereo needs no new codec - it
    // needs the AAC mixed down - and "only aac/ac3" would point at the wrong
    // property.
    let codec_plays_somewhere = probe.audio.iter().any(|stream| codec(stream).0);

    if !codec_plays_somewhere {
        // Rejecting on measured evidence only if every track was measured to
        // fail.
        let source = probe
            .audio
            .iter()
            .map(|stream| codec(stream).1)
            .fold(Provenance::Observed, Provenance::weakest);

        let mut codecs: Vec<&str> = probe
            .audio
            .iter()
            .map(|s| s.codec.as_deref().unwrap_or(NOT_REPORTED))
            .collect();
        codecs.sort_unstable();
        codecs.dedup();

        findings.check(
            (false, source),
            Field::AudioCodec,
            format!("only {}", codecs.join("/")),
            None,
        );
    }

    // Independent of the codec: a re-encode maps every audio track, so a
    // layout above the cap survives one unless the cap is asked for. Reported
    // against the narrowest track that is still too wide, which is the closest
    // the file comes to fitting.
    let too_wide = probe
        .audio
        .iter()
        .filter(|stream| !channels(stream, target).0)
        .filter_map(|stream| Some((stream.channels?, channels(stream, target).1)))
        .min_by_key(|(count, _)| *count);

    if let Some((count, source)) = too_wide {
        findings.check(
            (false, source),
            Field::AudioChannels,
            count.to_string(),
            Some(format!("up to {}", envelope.max_channels.value)),
        );
    }
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
            let codec = stream.codec.as_deref()?;
            Some((codec, target.subtitles.burns_in.provenance_of(codec)?))
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
            container: Some("mov,mp4,m4a,3gp,3g2,mj2".to_string()),
            video: Some(VideoStream {
                codec: Some("h264".to_string()),
                profile: Some("high".to_string()),
                level: Some(41),
                pixel_format: Some("yuv420p".to_string()),
                bit_depth: Some(8),
                ref_frames: Some(4),
                width: Some(1920),
                height: Some(1080),
            }),
            audio: vec![AudioStream {
                codec: Some("aac".to_string()),
                channels: Some(2),
                language: Some("eng".to_string()),
            }],
            ..Default::default()
        }
    }

    /// Every field in `Field::ALL` is produced by some file, so the list of
    /// properties and the checks that read them cannot drift apart in silence.
    ///
    /// It is deliberately not "is this struct field read": a `pub` field has no
    /// reader the compiler can see, and the question that matters is whether
    /// the decision accounts for the property, which is what walking `ALL` asks.
    ///
    /// One file cannot exercise them all, and that is a property of the model
    /// rather than a gap in the test: a file with no video stream has no bit
    /// depth to be wrong about, and profile and level are only asked once the
    /// codec is one the client decodes.
    #[test]
    fn every_field_is_reachable() {
        let broken: Vec<(&str, MediaProbe)> = vec![
            (
                "container",
                MediaProbe {
                    container: Some("avi".to_string()),
                    ..plexify_output()
                },
            ),
            (
                "codec",
                with_video(|video| video.codec = Some("av1".to_string())),
            ),
            (
                "profile",
                with_video(|video| video.profile = Some("extended".to_string())),
            ),
            ("level", with_video(|video| video.level = Some(50))),
            ("bit depth", with_video(|video| video.bit_depth = Some(10))),
            (
                "pixel format",
                with_video(|video| video.pixel_format = Some("yuv444p".to_string())),
            ),
            ("resolution", with_video(|video| video.height = Some(2160))),
            (
                "ref frames",
                with_video(|video| video.ref_frames = Some(16)),
            ),
            (
                "audio codec",
                MediaProbe {
                    audio: vec![AudioStream {
                        codec: Some("dts".to_string()),
                        channels: Some(2),
                        language: None,
                    }],
                    ..plexify_output()
                },
            ),
            (
                "audio channels",
                MediaProbe {
                    audio: vec![AudioStream {
                        codec: Some("aac".to_string()),
                        channels: Some(6),
                        language: None,
                    }],
                    ..plexify_output()
                },
            ),
            (
                "subtitles",
                MediaProbe {
                    subtitles: vec![SubtitleStream {
                        codec: Some("hdmv_pgs_subtitle".to_string()),
                        language: None,
                    }],
                    ..plexify_output()
                },
            ),
        ];

        let mut reached = std::collections::BTreeSet::new();
        for (what, probe) in &broken {
            let verdict = evaluate(probe, &chromecast());
            assert!(
                !verdict.reasons().is_empty(),
                "the {what} file was meant to fail and did not"
            );
            reached.extend(verdict.reasons().iter().map(|reason| reason.field));
        }

        let all: std::collections::BTreeSet<Field> = Field::ALL.into_iter().collect();
        assert_eq!(
            all.difference(&reached).collect::<Vec<_>>(),
            Vec::<&Field>::new(),
            "a Field nothing produces is a property the checks do not account for"
        );
    }

    /// A conforming file with one thing wrong with its video.
    fn with_video(break_it: impl FnOnce(&mut VideoStream)) -> MediaProbe {
        let mut probe = plexify_output();
        break_it(
            probe
                .video
                .as_mut()
                .expect("the fixture has a video stream"),
        );
        probe
    }

    fn claims(conformance: &Conformance) -> Vec<String> {
        conformance.reasons().iter().map(Finding::claim).collect()
    }

    #[test]
    fn a_conforming_file_needs_nothing_done_to_it() {
        let verdict = evaluate(&plexify_output(), &chromecast());

        assert!(verdict.direct_plays(), "{:?}", claims(&verdict));
    }

    /// What 598 files in this library are, and the one playback failure we have
    /// watched happen.
    fn library_avi() -> MediaProbe {
        MediaProbe {
            container: Some("avi".to_string()),
            video: Some(VideoStream {
                codec: Some("mpeg4".to_string()),
                profile: Some("simple profile".to_string()),
                level: Some(1),
                pixel_format: Some("yuv420p".to_string()),
                bit_depth: Some(8),
                ref_frames: Some(1),
                width: Some(640),
                height: Some(480),
            }),
            audio: vec![AudioStream {
                codec: Some("mp3".to_string()),
                channels: Some(2),
                language: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn an_avi_the_lg_stalls_on_needs_a_remux_not_nothing() {
        let verdict = evaluate(&library_avi(), &lg());

        assert_eq!(verdict.cost(), Some(Cost::Remux), "{:?}", claims(&verdict));
        let container = verdict
            .reasons()
            .iter()
            .find(|reason| reason.field == Field::Container)
            .expect("the container is the reason");
        // Watched stalling on this hardware. Not inferred from the format.
        assert_eq!(container.source, Provenance::Observed);
    }

    /// Nobody has tried an AVI on the Chromecast, so the container is refused
    /// on an assumption - while the video codec fails on its own account.
    #[test]
    fn the_same_avi_needs_a_reencode_for_the_chromecast() {
        let verdict = evaluate(&library_avi(), &chromecast());

        assert_eq!(verdict.cost(), Some(Cost::Reencode));
        let container = verdict
            .reasons()
            .iter()
            .find(|reason| reason.field == Field::Container)
            .expect("an unlisted container is still refused");
        assert_eq!(container.source, Provenance::Assumed);
    }

    fn container_is_unverified(verdict: &Conformance, name: &str) -> bool {
        verdict
            .unverified()
            .iter()
            .any(|finding| finding.field == Field::Container && finding.value == name)
    }

    /// FFprobe names an MKV and a WebM alike. An MKV has been watched playing
    /// on the LG and a WebM has not, so a WebM plays on an assumption.
    #[test]
    fn a_webm_on_the_lg_is_unverified() {
        let mut probe = plexify_output();
        probe.container = Some("matroska,webm".to_string());

        let verdict = evaluate(&probe, &lg());

        assert!(verdict.direct_plays(), "{:?}", claims(&verdict));
        assert!(
            container_is_unverified(&verdict, "matroska,webm"),
            "{:?}",
            verdict.unverified()
        );
    }

    /// A QuickTime file reads through the same demuxer as an MP4, and nobody
    /// has tried one on the Chromecast.
    #[test]
    fn the_mp4_demuxer_passes_the_chromecast_only_on_an_assumption() {
        let verdict = evaluate(&plexify_output(), &chromecast());

        assert!(verdict.direct_plays(), "{:?}", claims(&verdict));
        assert!(
            container_is_unverified(&verdict, "mov,mp4,m4a,3gp,3g2,mj2"),
            "{:?}",
            verdict.unverified()
        );
    }

    /// A field the probe could not read is the definition of an unverified
    /// pass. Skipping it silently is how a guess becomes a conformance.
    #[test]
    fn fields_the_probe_could_not_read_are_marked_rather_than_skipped() {
        let mut probe = plexify_output();
        let video = probe.video.as_mut().unwrap();
        video.profile = None;
        video.level = None;
        video.bit_depth = None;
        video.pixel_format = None;
        video.height = None;
        video.ref_frames = None;
        probe.container = None;
        probe.audio[0].channels = None;

        let verdict = evaluate(&probe, &chromecast());

        assert!(
            verdict.direct_plays(),
            "an absent value fails nothing: {:?}",
            claims(&verdict)
        );
        for field in [
            Field::Container,
            Field::VideoProfile,
            Field::VideoLevel,
            Field::BitDepth,
            Field::PixelFormat,
            Field::Resolution,
            Field::RefFrames,
            Field::AudioChannels,
        ] {
            assert!(
                verdict
                    .unverified()
                    .iter()
                    .any(|finding| finding.field == field && finding.value == "not reported"),
                "{field:?} passed in silence: {:?}",
                verdict.unverified()
            );
        }
    }

    /// The measured Chromecast fact: AC3 is refused in every layout, stereo
    /// included, and the fix is an audio swap rather than a re-encode.
    #[test]
    fn ac3_stereo_costs_a_remux_on_the_chromecast_and_that_is_measured() {
        let mut probe = plexify_output();
        probe.audio = vec![AudioStream {
            codec: Some("ac3".to_string()),
            channels: Some(2),
            ..Default::default()
        }];

        let verdict = evaluate(&probe, &chromecast());

        assert_eq!(verdict.cost(), Some(Cost::Remux));
        assert_eq!(verdict.reasons()[0].field, Field::AudioCodec);
        assert_eq!(verdict.reasons()[0].source, Provenance::Observed);
    }

    /// What plexify's own transcoder writes into every .mp4 it produces, and
    /// it costs nothing on either shipped client. The LG converts the track
    /// into a sidecar rather than burning it in; `targets/lg-cx-webos.toml`
    /// has the measurement.
    #[test]
    fn a_mov_text_track_costs_nothing_on_either_target() {
        let mut probe = plexify_output();
        probe.subtitles = vec![SubtitleStream {
            codec: Some("mov_text".to_string()),
            language: Some("eng".to_string()),
        }];

        for target in [lg(), chromecast()] {
            let verdict = evaluate(&probe, &target);
            assert!(
                verdict.direct_plays(),
                "{}: {:?}",
                target.name,
                claims(&verdict)
            );
        }
    }

    #[test]
    fn hevc_10_bit_is_a_reencode_for_the_chromecast_and_plays_on_the_lg() {
        let mut probe = plexify_output();
        probe.video = Some(VideoStream {
            codec: Some("hevc".to_string()),
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

        let lg = evaluate(&probe, &lg());
        assert!(lg.direct_plays(), "{:?}", claims(&lg));
    }

    /// FFprobe names no codec for a stream it has no decoder for. That is not
    /// a codec the client is known to play, so it is refused, on an assumption.
    #[test]
    fn a_video_codec_ffprobe_could_not_name_is_refused_on_an_assumption() {
        let verdict = evaluate(&with_video(|video| video.codec = None), &chromecast());

        assert_eq!(verdict.cost(), Some(Cost::Reencode));
        assert_eq!(verdict.reasons()[0].describe(), "video codec: not reported");
        assert_eq!(verdict.reasons()[0].source, Provenance::Assumed);
    }

    /// A codec nobody has watched play is not a conformance the tool should
    /// state flatly.
    #[test]
    fn vp9_passes_the_chromecast_only_on_an_assumption() {
        let mut probe = plexify_output();
        probe.video = Some(VideoStream {
            codec: Some("vp9".to_string()),
            profile: Some("profile 0".to_string()),
            ..plexify_output().video.unwrap()
        });

        let verdict = evaluate(&probe, &chromecast());

        assert!(verdict.direct_plays(), "{:?}", claims(&verdict));
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
            codec: Some("av1".to_string()),
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

    fn fields_of(conformance: &Conformance) -> Vec<Field> {
        let mut fields: Vec<Field> = conformance
            .reasons()
            .iter()
            .map(|reason| reason.field)
            .collect();
        fields.sort();
        fields
    }

    /// Both faults at once, because both are true: a fix that changed only the
    /// codec would produce a 5.1 AAC track the Chromecast refuses. Both are
    /// remux work, so the second fault does not move the file to a dearer
    /// bucket.
    #[test]
    fn a_51_track_in_a_refused_codec_reports_the_codec_and_the_layout() {
        let mut probe = plexify_output();
        probe.audio = vec![AudioStream {
            codec: Some("ac3".to_string()),
            channels: Some(6),
            ..Default::default()
        }];

        let verdict = evaluate(&probe, &chromecast());

        assert_eq!(verdict.cost(), Some(Cost::Remux));
        assert_eq!(
            fields_of(&verdict),
            [Field::AudioCodec, Field::AudioChannels]
        );
    }

    /// The other half: 342 Opus files in this library are stereo or mono, and the LG's cap is
    /// six. Reporting a layout fault here would have the fix upmix stereo into
    /// 5.1 to reach a ceiling the file was never near.
    #[test]
    fn a_stereo_track_in_a_refused_codec_reports_only_the_codec() {
        let mut probe = plexify_output();
        probe.audio = vec![AudioStream {
            codec: Some("opus".to_string()),
            channels: Some(2),
            ..Default::default()
        }];

        let verdict = evaluate(&probe, &lg());

        assert_eq!(verdict.cost(), Some(Cost::Remux));
        assert_eq!(fields_of(&verdict), [Field::AudioCodec]);
    }

    /// A codec the client decodes on one track is not a codec fault, whatever
    /// the layouts are. "only aac/ac3" would point the reader at the wrong
    /// property when what the file needs is a mix-down.
    #[test]
    fn a_codec_that_plays_on_some_track_is_not_reported_as_the_fault() {
        let mut probe = plexify_output();
        probe.audio = vec![
            AudioStream {
                codec: Some("aac".to_string()),
                channels: Some(6),
                ..Default::default()
            },
            AudioStream {
                codec: Some("ac3".to_string()),
                channels: Some(2),
                ..Default::default()
            },
        ];

        let verdict = evaluate(&probe, &chromecast());

        assert_eq!(fields_of(&verdict), [Field::AudioChannels]);
    }

    /// The worst case in this library, and the one the owner complained about:
    /// 120 .webm files, 500 GB, VP9 or AV1 with Opus audio and an embedded
    /// WebVTT track. They fail the LG on audio, and the re-encode the AV1 half
    /// needs is the one the Pi cannot sustain.
    fn critical_role(video_codec: &str) -> MediaProbe {
        MediaProbe {
            container: Some("matroska,webm".to_string()),
            video: Some(VideoStream {
                codec: Some(video_codec.to_string()),
                profile: Some("profile 0".to_string()),
                pixel_format: Some("yuv420p".to_string()),
                bit_depth: Some(8),
                width: Some(1920),
                height: Some(1080),
                ..Default::default()
            }),
            audio: vec![AudioStream {
                codec: Some("opus".to_string()),
                channels: Some(2),
                ..Default::default()
            }],
            subtitles: vec![SubtitleStream {
                codec: Some("webvtt".to_string()),
                language: Some("eng".to_string()),
            }],
            ..Default::default()
        }
    }

    /// Opus, and only Opus: the LG is not claimed to burn WebVTT in.
    #[test]
    fn the_webm_population_fails_the_lg_on_its_audio() {
        let verdict = evaluate(&critical_role("vp9"), &lg());

        assert_eq!(fields_of(&verdict), [Field::AudioCodec]);
        assert_eq!(verdict.cost(), Some(Cost::Remux));
    }

    /// The 22 AV1 files among them are the expensive half: no envelope claims
    /// this device decodes AV1, and that is a re-encode whatever the audio and
    /// subtitles do.
    #[test]
    fn the_av1_half_of_that_population_needs_a_reencode() {
        let verdict = evaluate(&critical_role("av1"), &lg());

        assert_eq!(verdict.cost(), Some(Cost::Reencode));
    }

    /// VP9 on this app is untested in both directions: the one session that
    /// carried it was under a client cap, which re-encodes any source. The
    /// marker is what tells a reader the verdict is a prediction.
    #[test]
    fn vp9_on_the_lg_still_reads_as_unverified() {
        let verdict = evaluate(&critical_role("vp9"), &lg());

        assert!(verdict
            .unverified()
            .iter()
            .any(|finding| finding.field == Field::VideoCodec && finding.value == "vp9"));
    }

    /// The client plays whichever track suits it, so one good track is enough.
    #[test]
    fn a_file_conforms_when_any_one_of_its_tracks_plays() {
        let mut probe = plexify_output();
        probe.audio = vec![
            AudioStream {
                codec: Some("ac3".to_string()),
                channels: Some(6),
                ..Default::default()
            },
            AudioStream {
                codec: Some("aac".to_string()),
                channels: Some(2),
                ..Default::default()
            },
        ];

        assert!(evaluate(&probe, &chromecast()).direct_plays());
    }

    /// Nothing can be remuxed into a file that has no audio, and a silent file
    /// still Direct Plays.
    #[test]
    fn a_file_with_no_audio_is_not_reported_as_needing_work() {
        let mut probe = plexify_output();
        probe.audio.clear();

        assert!(evaluate(&probe, &chromecast()).direct_plays());
    }

    fn unverified_claims(conformance: &Conformance) -> Vec<String> {
        conformance
            .unverified()
            .iter()
            .map(Finding::claim)
            .collect()
    }

    /// A file with no picture has nothing a video check can judge, and no
    /// target says what a client does with one. A re-encode cannot add the
    /// missing stream, so it is not a fix to prescribe, and nothing measured
    /// that it plays, so it does not conform either.
    #[test]
    fn a_file_with_no_video_stream_is_unassessable_rather_than_refused() {
        let mut probe = plexify_output();
        probe.video = None;

        assert_eq!(evaluate(&probe, &chromecast()), Conformance::NoPicture);
    }

    /// Audio the client refuses is still no reason to queue a file with no
    /// picture: every job copies or encodes a video stream, so a remux of one
    /// can only fail, and the file is more likely misfiled than in need of a
    /// conversion.
    #[test]
    fn a_file_with_no_video_stream_is_not_refused_for_its_audio_either() {
        let probe = MediaProbe {
            container: Some("matroska,webm".to_string()),
            video: None,
            audio: vec![AudioStream {
                codec: Some("dts".to_string()),
                channels: Some(6),
                language: None,
            }],
            ..Default::default()
        };

        assert_eq!(evaluate(&probe, &lg()), Conformance::NoPicture);
    }

    /// The LG's envelope bounds neither MPEG-4 Part 2 profiles nor HEVC
    /// levels. A pass with no bound to check against is a belief about every
    /// profile and level, and none of them was measured.
    #[test]
    fn a_profile_or_level_the_target_does_not_bound_passes_on_an_assumption() {
        let xvid = with_video(|video| {
            video.codec = Some("mpeg4".to_string());
            video.profile = Some("advanced simple profile".to_string());
            video.level = Some(5);
        });
        let verdict = evaluate(&xvid, &lg());

        assert!(verdict.direct_plays(), "{:?}", claims(&verdict));
        let unverified = unverified_claims(&verdict);
        assert!(
            unverified.contains(&"video profile: any mpeg4 profile".to_string()),
            "{unverified:?}"
        );
        assert!(
            unverified.contains(&"level: any mpeg4 level".to_string()),
            "{unverified:?}"
        );
    }

    /// Up to `observed_up_to` a level was seen playing, so a pass there is
    /// measured; above it nothing was, so a pass or a refusal is an assumption.
    #[test]
    fn a_level_is_measured_only_up_to_what_was_observed() {
        let limit = Limit {
            value: 41,
            source: Provenance::Assumed,
            observed_up_to: Some(31),
            note: None,
        };
        limit
            .check("video.max_level.h264")
            .expect("a limit the loader accepts");
        let mut target = lg();
        target.video.max_level.insert("h264".to_string(), limit);
        let at = |level| evaluate(&with_video(|video| video.level = Some(level)), &target);
        let level_is_unverified = |verdict: &Conformance| {
            verdict
                .unverified()
                .iter()
                .any(|finding| finding.field == Field::VideoLevel)
        };

        assert!(!level_is_unverified(&at(31)), "3.1 was seen playing");
        assert!(level_is_unverified(&at(41)), "4.1 passes, unmeasured");

        let refused = at(50);
        let level = refused
            .reasons()
            .iter()
            .find(|reason| reason.field == Field::VideoLevel)
            .expect("5.0 is above the cap");
        assert_eq!(level.source, Provenance::Assumed);
    }

    /// Both LG readings behind its audio codecs were stereo Direct Plays, so
    /// stereo is measured and anything wider is not.
    #[test]
    fn stereo_on_the_lg_is_measured_and_surround_is_not() {
        let stereo = evaluate(&plexify_output(), &lg());
        assert!(
            !stereo
                .unverified()
                .iter()
                .any(|finding| finding.field == Field::AudioChannels),
            "{:?}",
            unverified_claims(&stereo)
        );

        let mut surround = plexify_output();
        surround.audio[0].channels = Some(6);
        let surround = evaluate(&surround, &lg());
        assert!(surround.direct_plays(), "{:?}", claims(&surround));
        assert!(
            surround
                .unverified()
                .iter()
                .any(|finding| finding.field == Field::AudioChannels && finding.value == "6"),
            "{:?}",
            unverified_claims(&surround)
        );
    }

    /// The expensive verdict wins: a file that needs its video re-encoded is
    /// not filed under the cheap bucket because its audio also needs a swap.
    #[test]
    fn a_video_reason_outranks_an_audio_one() {
        let mut probe = plexify_output();
        probe.video.as_mut().unwrap().codec = Some("mpeg4".to_string());
        probe.audio[0].codec = Some("ac3".to_string());

        let verdict = evaluate(&probe, &chromecast());

        assert_eq!(verdict.cost(), Some(Cost::Reencode));
        assert_eq!(verdict.reasons().len(), 2);
    }
}
