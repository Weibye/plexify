//! What a client will play, and how much of that was actually measured.
//!
//! A target is data, not code: the envelope of one device, loaded from TOML.
//! Probing a library is slow and its answers do not change; an envelope changes
//! every time something is learned about a client. Keeping them apart means
//! recalibrating costs a re-read rather than a re-probe.
//!
//! Every claim carries its [`Provenance`]. Three spec-derived assumptions in
//! this project have already been measured wrong on the hardware, so a belief
//! that has never been tested must not be able to look like a fact. That is
//! also why a measured *rejection* is written down (`rejects`) rather than left
//! as an absence: silence cannot tell "we measured this and it failed" apart
//! from "we never tried it".

pub mod evaluate;

pub use evaluate::{evaluate, Conformance, Cost, Field, Finding};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The envelopes that ship with plexify, by name.
pub const BUILTIN_TARGETS: &[(&str, &str)] = &[
    (
        "chromecast-gen2-3",
        include_str!("../../targets/chromecast-gen2-3.toml"),
    ),
    (
        "lg-cx-webos",
        include_str!("../../targets/lg-cx-webos.toml"),
    ),
];

/// How a claim was arrived at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    /// Confirmed against a live playback decision on the device.
    Observed,
    /// Taken from a specification, and never tested.
    Assumed,
}

impl Provenance {
    /// The weaker of two provenances. A conclusion drawn from an observation
    /// and an assumption together is only as good as the assumption.
    pub fn weakest(self, other: Self) -> Self {
        self.max(other)
    }

    pub fn is_assumed(self) -> bool {
        self == Provenance::Assumed
    }
}

/// Values a client is believed to handle, each carrying its own provenance.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimSet {
    #[serde(default)]
    pub observed: BTreeSet<String>,
    #[serde(default)]
    pub assumed: BTreeSet<String>,
}

impl ClaimSet {
    /// How the belief about this value was arrived at, or `None` if the set
    /// says nothing about it.
    pub fn provenance_of(&self, value: &str) -> Option<Provenance> {
        if self.observed.contains(value) {
            Some(Provenance::Observed)
        } else if self.assumed.contains(value) {
            Some(Provenance::Assumed)
        } else {
            None
        }
    }
}

/// What a client does with a set of values.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Support {
    #[serde(default)]
    pub accepts: ClaimSet,
    /// Values measured or believed to fail. Recorded separately so a rejection
    /// that was actually observed keeps saying so.
    #[serde(default)]
    pub rejects: ClaimSet,
}

impl Support {
    /// Whether the value plays, and how good the belief behind that answer is.
    ///
    /// A value nobody has said anything about is refused, on an assumption:
    /// this envelope is an allow-list, and an unlisted codec is unknown rather
    /// than known-bad.
    pub fn verdict(&self, value: &str) -> (bool, Provenance) {
        if let Some(provenance) = self.accepts.provenance_of(value) {
            (true, provenance)
        } else if let Some(provenance) = self.rejects.provenance_of(value) {
            (false, provenance)
        } else {
            (false, Provenance::Assumed)
        }
    }
}

/// A ceiling the client is believed to accept up to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limit {
    pub value: u32,
    pub source: Provenance,
    /// The highest value seen to play. A cap can be a guess while the range
    /// below some point in it is measured - the Chromecast's reference-frame
    /// cap is the spec's 8, but only 5 has been watched working.
    #[serde(default)]
    pub observed_up_to: Option<u32>,
    #[serde(default)]
    pub note: Option<String>,
}

impl Limit {
    /// Whether the value fits, and how good the belief behind that answer is.
    pub fn verdict(&self, value: u32) -> (bool, Provenance) {
        let measured = self.observed_up_to.unwrap_or(match self.source {
            Provenance::Observed => self.value,
            Provenance::Assumed => 0,
        });

        if value <= measured {
            (true, Provenance::Observed)
        } else if value <= self.value {
            (true, Provenance::Assumed)
        } else {
            (false, self.source)
        }
    }
}

/// One client's playback envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaybackTarget {
    pub name: String,
    /// The hardware this was measured on, in enough detail to tell it from a
    /// near relative that behaves differently.
    #[serde(default)]
    pub device: Option<String>,
    pub video: VideoEnvelope,
    pub audio: AudioEnvelope,
    #[serde(default)]
    pub subtitles: SubtitleEnvelope,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VideoEnvelope {
    pub codecs: Support,
    /// Accepted profiles per codec. A codec with no entry is unconstrained,
    /// which is not the same as a codec with an empty one.
    #[serde(default)]
    pub profiles: BTreeMap<String, ClaimSet>,
    /// Level ceiling per codec, in FFprobe's integer form: 41 is 4.1.
    #[serde(default)]
    pub max_level: BTreeMap<String, Limit>,
    pub max_bit_depth: Limit,
    pub max_height: Limit,
    pub max_ref_frames: Limit,
    pub pixel_formats: Support,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioEnvelope {
    pub codecs: Support,
    pub max_channels: Limit,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubtitleEnvelope {
    /// Formats the client renders by burning them into the picture. The client
    /// then re-encodes the video to do it, so the track has to go.
    #[serde(default)]
    pub burns_in: ClaimSet,
}

impl PlaybackTarget {
    /// Load a target by built-in name, or from a TOML file at that path.
    pub fn load(spec: &str) -> Result<Self> {
        if let Some(target) = Self::builtin(spec) {
            return target;
        }

        let path = Path::new(spec);
        if path.is_file() {
            return Self::from_file(path);
        }

        Err(anyhow::anyhow!(
            "no target '{spec}': give a TOML file, or one of {}",
            Self::builtin_names().join(", ")
        ))
    }

    /// One of the envelopes that ship with plexify.
    pub fn builtin(name: &str) -> Option<Result<Self>> {
        BUILTIN_TARGETS
            .iter()
            .find(|(builtin, _)| *builtin == name)
            .map(|(name, toml)| {
                Self::from_toml(toml).with_context(|| format!("built-in target '{name}'"))
            })
    }

    pub fn builtin_names() -> Vec<&'static str> {
        BUILTIN_TARGETS.iter().map(|(name, _)| *name).collect()
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("could not read target {}", path.display()))?;

        Self::from_toml(&text).with_context(|| format!("target {}", path.display()))
    }

    pub fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).context("could not read the target envelope")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_target_parses_and_owns_its_name() {
        for (name, _) in BUILTIN_TARGETS {
            let target = PlaybackTarget::builtin(name).unwrap().unwrap();
            assert_eq!(&target.name, name, "the file's name must match its key");
        }
    }

    /// The two facts that cost the most to learn: what each device refuses.
    #[test]
    fn the_measured_rejections_survive_a_round_trip_through_toml() {
        let chromecast = PlaybackTarget::builtin("chromecast-gen2-3")
            .unwrap()
            .unwrap();
        assert_eq!(
            chromecast.audio.codecs.verdict("ac3"),
            (false, Provenance::Observed)
        );

        let lg = PlaybackTarget::builtin("lg-cx-webos").unwrap().unwrap();
        assert_eq!(
            lg.audio.codecs.verdict("opus"),
            (false, Provenance::Observed)
        );
        assert_eq!(
            lg.subtitles.burns_in.provenance_of("mov_text"),
            Some(Provenance::Observed)
        );
    }

    /// VP9 and yuvj420p on the Chromecast, and AC3 on the LG, are unverified.
    /// If any of them ever reads as observed, someone has promoted a spec sheet.
    #[test]
    fn what_is_unverified_says_so() {
        let chromecast = PlaybackTarget::builtin("chromecast-gen2-3")
            .unwrap()
            .unwrap();
        assert_eq!(
            chromecast.video.codecs.verdict("vp9"),
            (true, Provenance::Assumed)
        );
        assert_eq!(
            chromecast.video.pixel_formats.verdict("yuvj420p"),
            (true, Provenance::Assumed)
        );

        let lg = PlaybackTarget::builtin("lg-cx-webos").unwrap().unwrap();
        assert_eq!(lg.audio.codecs.verdict("ac3"), (true, Provenance::Assumed));
    }

    #[test]
    fn an_unlisted_value_is_refused_and_the_refusal_is_a_guess() {
        let target = PlaybackTarget::builtin("chromecast-gen2-3")
            .unwrap()
            .unwrap();

        assert_eq!(
            target.video.codecs.verdict("av1"),
            (false, Provenance::Assumed)
        );
    }

    /// A cap can be assumed while part of its range is measured.
    #[test]
    fn a_limit_reports_where_the_measurement_stops() {
        let refs = Limit {
            value: 8,
            source: Provenance::Assumed,
            observed_up_to: Some(5),
            note: None,
        };

        assert_eq!(refs.verdict(5), (true, Provenance::Observed));
        assert_eq!(refs.verdict(6), (true, Provenance::Assumed));
        assert_eq!(refs.verdict(9), (false, Provenance::Assumed));
    }

    #[test]
    fn an_observed_cap_covers_everything_below_it() {
        let level = Limit {
            value: 41,
            source: Provenance::Observed,
            observed_up_to: None,
            note: None,
        };

        assert_eq!(level.verdict(30), (true, Provenance::Observed));
        assert_eq!(level.verdict(41), (true, Provenance::Observed));
        assert_eq!(level.verdict(50), (false, Provenance::Observed));
    }

    #[test]
    fn a_misspelled_field_is_an_error_rather_than_a_default() {
        let toml = r#"
            name = "typo"
            [video]
            max_bit_dept = { value = 8, source = "observed" }
        "#;

        assert!(PlaybackTarget::from_toml(toml).is_err());
    }

    #[test]
    fn naming_a_target_that_does_not_exist_lists_the_ones_that_do() {
        let error = PlaybackTarget::load("chromecast-ultra")
            .unwrap_err()
            .to_string();

        assert!(error.contains("chromecast-gen2-3"), "got: {error}");
    }
}
