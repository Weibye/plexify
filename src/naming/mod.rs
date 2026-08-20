//! What a media path in this library is supposed to look like.
//!
//! The library has one canonical shape, and this module is the only description
//! of it. Everything here follows from a single decision: instead of asking
//! *"does this path match an accepted form?"*, we **parse** a path into the
//! fields it is made of and **render** those fields back into the one canonical
//! form. A path is correct exactly when rendering its own parse reproduces it.
//!
//! That inversion is what makes renaming possible. A yes/no matcher can only
//! reject a path; it cannot say what the path should have been, which is why
//! a destination must never be produced by patching the source string.
//!
//! ```text
//! Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv
//!   parse  -> series "Elementary", season 6, episode 8, title "Sand Trap"
//!   render -> Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv
//! ```
//!
//! ## What is deliberately left alone
//!
//! Rules apply per path component, and a component with no rule is preserved as
//! it is. The series directory is never renamed here, so a file whose name
//! disagrees with its directory keeps its own name rather than being pulled
//! toward a directory that may itself be wrong. A file that sits directly in a
//! series directory with no season directory stays there. Both are conservative
//! on purpose: this runs against a library that cannot be reconstructed, and
//! moving a file between directories is a bigger claim than fixing its name.
//!
//! ## When we refuse
//!
//! A path we cannot decompose produces [`Assessment::Unresolvable`] with a
//! reason, never a guess. A duplicated library root is the clearest case: in
//! `Series/Veronica Mars/Series/...` the correct destination is genuinely
//! ambiguous, so it is reported for a human to resolve.

mod parse;
mod render;

use std::path::Path;

use crate::paths::to_forward_slashes;

pub use parse::parse;
pub use render::render;

/// The top-level directories a library is organised into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryRoot {
    Series,
    Anime,
    Movies,
}

impl LibraryRoot {
    /// The directory name this root appears as.
    pub fn as_str(&self) -> &'static str {
        match self {
            LibraryRoot::Series => "Series",
            LibraryRoot::Anime => "Anime",
            LibraryRoot::Movies => "Movies",
        }
    }

    /// Recognise a top-level directory name.
    pub fn from_component(component: &str) -> Option<Self> {
        match component {
            "Series" => Some(LibraryRoot::Series),
            "Anime" => Some(LibraryRoot::Anime),
            "Movies" => Some(LibraryRoot::Movies),
            _ => None,
        }
    }

    /// Every root, for callers that need to describe the library layout.
    pub fn all() -> [LibraryRoot; 3] {
        [
            LibraryRoot::Series,
            LibraryRoot::Anime,
            LibraryRoot::Movies,
        ]
    }

    /// Whether this root holds episodic content.
    pub fn is_episodic(&self) -> bool {
        matches!(self, LibraryRoot::Series | LibraryRoot::Anime)
    }
}

/// A season directory, or its absence.
///
/// `Season 6` and `Season 06` parse to the same number and render the same way,
/// which is what makes zero-padding a fix rather than a separate rule. `Specials`
/// is season zero by Plex convention but keeps its own name when rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeasonDirectory {
    /// A numbered season directory.
    Numbered(u32),
    /// A `Specials` directory.
    Specials,
    /// The episode sits directly in the series directory.
    Absent,
}

/// An episode file, decomposed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Episode {
    pub root: LibraryRoot,
    /// The series directory exactly as it exists, never rewritten.
    pub series_directory: String,
    /// The series name as the *file* gives it, cleaned of separators.
    pub series: String,
    pub season_directory: SeasonDirectory,
    pub season: u32,
    pub number: u32,
    /// The episode title, or `None` when the name carried nothing recoverable.
    pub title: Option<String>,
    /// Resolution and frame rate, if the name carried them.
    pub quality: Option<String>,
    pub extension: String,
}

/// A movie file, decomposed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Movie {
    pub root: LibraryRoot,
    /// The containing directory as it exists - a film directory or a collection.
    pub directory: String,
    pub title: String,
    pub year: u32,
    pub extension: String,
}

/// A parsed media path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaName {
    Episode(Episode),
    Movie(Movie),
}

/// Why a path could not be decomposed.
///
/// Each variant is something a person has to decide, not something the parser
/// could work harder at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unresolvable {
    /// A library root name appears again below the root.
    DuplicatedRoot { root: LibraryRoot },
    /// The path does not start at a known library root.
    OutsideLibrary,
    /// An episode file with no recognisable season/episode marker.
    NoEpisodeMarker,
    /// A movie file with no release year, which cannot be invented.
    NoReleaseYear,
    /// The path has no filename, or a filename with no extension.
    NotAMediaFile,
}

impl Unresolvable {
    /// A one-line explanation, for the validation report.
    pub fn reason(&self) -> String {
        match self {
            Unresolvable::DuplicatedRoot { root } => format!(
                "'{}' appears twice in this path; the correct location is ambiguous",
                root.as_str()
            ),
            Unresolvable::OutsideLibrary => format!(
                "not under a known library root ({})",
                LibraryRoot::all()
                    .iter()
                    .map(|root| root.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Unresolvable::NoEpisodeMarker => {
                "no season and episode marker could be found in the name".to_string()
            }
            Unresolvable::NoReleaseYear => {
                "no release year in the name; the year cannot be guessed".to_string()
            }
            Unresolvable::NotAMediaFile => "not a media file path".to_string(),
        }
    }
}

/// What should happen to a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessment {
    /// Already in canonical form.
    Canonical,
    /// Canonical form differs; this is where the file belongs.
    Rename { destination: String },
    /// Needs a person.
    Unresolvable(Unresolvable),
}

/// Assess a path, given relative to the media root.
///
/// This is the whole contract of the module: parse, render, compare. Callers get
/// a destination they can act on without ever inspecting the original string.
pub fn assess(relative_path: &Path) -> Assessment {
    let path = to_forward_slashes(relative_path);

    match parse(&path) {
        Ok(name) => {
            let destination = render(&name);
            if destination == path {
                Assessment::Canonical
            } else {
                Assessment::Rename { destination }
            }
        }
        Err(reason) => Assessment::Unresolvable(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The acceptance cases from issue #51, asserted end to end.
    ///
    /// Each is a real path from the library, with the destination the issue
    /// specifies for it.
    #[test]
    fn pads_the_season_directory_and_separates_the_episode_title() {
        assert_eq!(
            assess(Path::new(
                "Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv"
            )),
            Assessment::Rename {
                destination: "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv"
                    .to_string()
            }
        );
    }

    #[test]
    fn leaves_a_canonical_path_alone() {
        assert_eq!(
            assess(Path::new(
                "Series/Charmed/Season 06/Charmed - S06E17 - Hyde School Reunion.avi"
            )),
            Assessment::Canonical
        );
    }

    #[test]
    fn discards_scene_release_cruft_it_cannot_turn_into_a_title() {
        assert_eq!(
            assess(Path::new(
                "Series/Scrubs/Season 9/Scrubs.S09E02.RETAIL.DVDRip.XviD-REWARD.avi"
            )),
            Assessment::Rename {
                destination: "Series/Scrubs/Season 09/Scrubs - S09E02.avi".to_string()
            }
        );
    }

    #[test]
    fn recovers_a_dotted_series_name_and_episode_title() {
        assert_eq!(
            assess(Path::new(
                "Series/Samurai Jack (2001)/Season 3/Samurai.Jack.S03E10.XXXVI.Jack.The.Monks.and.the.Ancient.Master's.Son.avi"
            )),
            Assessment::Rename {
                destination:
                    "Series/Samurai Jack (2001)/Season 03/Samurai Jack - S03E10 - Jack The Monks and the Ancient Master's Son.avi"
                        .to_string()
            }
        );
    }

    #[test]
    fn moves_quality_metadata_into_brackets() {
        assert_eq!(
            assess(Path::new(
                "Series/Super Best Friends Play - FFX/Super Best Friends Play - Final Fantasy X - S01E13 (1080p60).webm"
            )),
            Assessment::Rename {
                destination:
                    "Series/Super Best Friends Play - FFX/Super Best Friends Play - Final Fantasy X - S01E13 [1080p60].webm"
                        .to_string()
            }
        );
    }

    #[test]
    fn refuses_to_rewrite_a_duplicated_library_root() {
        assert_eq!(
            assess(Path::new(
                "Series/Veronica Mars/Series/Veronica Mars S02E04/Season 01/Veronica Mars S02E04.mp4"
            )),
            Assessment::Unresolvable(Unresolvable::DuplicatedRoot {
                root: LibraryRoot::Series
            })
        );
    }

    /// Cases the library already contains, beyond the issue's list.
    #[test]
    fn uppercases_a_lowercase_episode_marker() {
        assert_eq!(
            assess(Path::new(
                "Series/Breaking Bad {tvdb-81189}/Season 01/Breaking Bad - s01e01 - Pilot.mkv"
            )),
            Assessment::Rename {
                destination:
                    "Series/Breaking Bad {tvdb-81189}/Season 01/Breaking Bad - S01E01 - Pilot.mkv"
                        .to_string()
            }
        );
    }

    #[test]
    fn treats_anime_like_series() {
        assert_eq!(
            assess(Path::new(
                "Anime/Cowboy Bebop/Season 01/Cowboy Bebop - S01E01 - Asteroid Blues.mkv"
            )),
            Assessment::Canonical
        );
    }

    #[test]
    fn keeps_a_canonical_movie_and_its_collection_directory() {
        assert_eq!(
            assess(Path::new(
                "Movies/The Dark Knight (2008)/The Dark Knight (2008).mkv"
            )),
            Assessment::Canonical
        );
        assert_eq!(
            assess(Path::new(
                "Movies/Marvel Cinematic Universe Collection/Iron Man (2008).mkv"
            )),
            Assessment::Canonical
        );
    }

    #[test]
    fn reports_a_movie_without_a_year_rather_than_inventing_one() {
        assert_eq!(
            assess(Path::new("Movies/Some Film/Some Film.mkv")),
            Assessment::Unresolvable(Unresolvable::NoReleaseYear)
        );
    }

    #[test]
    fn reports_a_file_outside_the_library_roots() {
        assert_eq!(
            assess(Path::new("Downloads/whatever.mkv")),
            Assessment::Unresolvable(Unresolvable::OutsideLibrary)
        );
    }

    #[test]
    fn reports_an_episode_file_with_no_marker() {
        assert_eq!(
            assess(Path::new("Series/Firefly/Season 01/Serenity.mkv")),
            Assessment::Unresolvable(Unresolvable::NoEpisodeMarker)
        );
    }

    /// The property the whole module rests on: whatever we propose must itself
    /// be canonical, or `--fix` would move a file twice.
    #[test]
    fn every_proposed_destination_is_itself_canonical() {
        let messy = [
            "Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv",
            "Series/Scrubs/Season 9/Scrubs.S09E02.RETAIL.DVDRip.XviD-REWARD.avi",
            "Series/Samurai Jack (2001)/Season 3/Samurai.Jack.S03E10.XXXVI.Jack.The.Monks.avi",
            "Series/Super Best Friends Play - FFX/Super Best Friends Play - S01E13 (1080p60).webm",
            "Series/Breaking Bad {tvdb-81189}/Season 01/Breaking Bad - s01e01 - Pilot.mkv",
            "Anime/Cowboy Bebop/Season 1/Cowboy Bebop.S01E05.Ballad.of.Fallen.Angels.mkv",
            "Movies/The Dark Knight (2008)/The Dark Knight (2008).mkv",
        ];

        for path in messy {
            let destination = match assess(Path::new(path)) {
                Assessment::Canonical => continue,
                Assessment::Rename { destination } => destination,
                Assessment::Unresolvable(reason) => {
                    panic!("{path} should be resolvable, got: {}", reason.reason())
                }
            };

            assert_eq!(
                assess(Path::new(&destination)),
                Assessment::Canonical,
                "proposed destination is not canonical: {path} -> {destination}"
            );
        }
    }
}
