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
//! toward a directory that may itself be wrong.
//!
//! The season directory is the exception, and it is not preserved but derived:
//! every episode belongs in the season its own marker names, so a file with no
//! season directory is moved into one and a file in the wrong one is moved
//! across. The directory is evidence of where a file ended up; the marker is
//! evidence of what it is, and the marker wins.
//!
//! ## When we refuse
//!
//! A path we cannot decompose produces [`Assessment::Unresolvable`] with a
//! reason, never a guess. A duplicated library root is the clearest case: in
//! `Series/Veronica Mars/Series/...` the correct destination is genuinely
//! ambiguous, so it is reported for a human to resolve.

mod parse;
mod render;

use std::path::{Component, Path, PathBuf};

use crate::paths::to_forward_slashes;

pub use parse::parse;
pub use render::render;

/// Which part of the library to look at, and what to measure it against.
///
/// Every judgement here is made on a path relative to the library root, so a
/// caller who points at one series still has to be measured from the root - a
/// path starting `Season 06/` names no series and belongs to no root, and would
/// be refused as unresolvable.
///
/// Separating the two lets a run be narrowed to a corner of the library without
/// changing what canonical means: walk `scan_path`, judge against `library_root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// The directory holding `Series`, `Anime`, and `Movies`.
    pub library_root: PathBuf,
    /// The subtree to actually walk. Equal to the root for a whole-library run.
    pub scan_path: PathBuf,
}

impl Scope {
    /// Whether this run covers the whole library.
    pub fn is_whole_library(&self) -> bool {
        self.library_root == self.scan_path
    }
}

/// Work out which library a path belongs to, and how much of it to walk.
///
/// The library root is the parent of the **outermost** component named after a
/// root *that is actually one*. Outermost rather than nearest matters for a tree
/// that was nested into itself: pointing into `Series/Veronica Mars/Series/...`
/// finds the outer `Series`, so the duplication is still reported rather than
/// being read as a library in its own right.
///
/// The qualification is the whole difficulty, because a name alone cannot settle
/// it. `/srv/Anime` may *hold* `Series/`, `Anime/` and `Movies/`, or it may
/// itself *be* the Anime root with series directories directly inside; both are
/// ordinary, and reading the first as the second refuses the entire library at
/// once, since every file below then looks like a tree nested into itself. Only
/// the directory decides, so [`holds_library_roots`] asks it: a candidate that
/// has roots beneath it is the media root, and the search moves further in.
/// Where nothing is left, the whole path is the library root - which is also
/// what happens for a path that names no root at all, so a whole-library run
/// works exactly as before.
///
/// Give this an absolute path. A relative one that *starts* at a root - plain
/// `Series/Elementary` - has nothing before that component to be the library
/// root, and the current directory stands in. Callers that take a path from a
/// user should resolve it first, so that what the report prints is unambiguous.
pub fn scope_for(path: &Path) -> Scope {
    let components: Vec<Component> = path.components().collect();

    let root_position = components
        .iter()
        .enumerate()
        .find_map(|(position, component)| match component {
            Component::Normal(name)
                if LibraryRoot::from_component(&name.to_string_lossy()).is_some() =>
            {
                let candidate: PathBuf = components[..=position].iter().collect();
                (!holds_library_roots(&candidate)).then_some(position)
            }
            _ => None,
        });

    match root_position {
        Some(position) => {
            let library_root: PathBuf = components[..position].iter().collect();

            Scope {
                library_root: if library_root.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    library_root
                },
                scan_path: path.to_path_buf(),
            }
        }
        None => Scope {
            library_root: path.to_path_buf(),
            scan_path: path.to_path_buf(),
        },
    }
}

/// Whether a directory has library roots directly beneath it, and so is the
/// media root rather than a root itself.
///
/// This is the one place the naming module looks at a disk, and it looks at
/// exactly one directory. A path that cannot be read - it does not exist, or the
/// caller cannot list it - yields no evidence, and no evidence leaves the name
/// standing: the candidate is taken to be a real root, which is what a library
/// root normally is.
fn holds_library_roots(directory: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return false;
    };

    entries.flatten().any(|entry| {
        LibraryRoot::from_component(&entry.file_name().to_string_lossy()).is_some()
            && entry.path().is_dir()
    })
}

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
        [LibraryRoot::Series, LibraryRoot::Anime, LibraryRoot::Movies]
    }

    /// Whether this root holds episodic content.
    pub fn is_episodic(&self) -> bool {
        matches!(self, LibraryRoot::Series | LibraryRoot::Anime)
    }
}

/// A season directory, or its absence.
///
/// `Season 6` and `Season 06` parse to the same number and render the same way,
/// which is what makes zero-padding a fix rather than a separate rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeasonDirectory {
    /// A numbered season directory, with anything that followed the number.
    ///
    /// The suffix holds an arc name such as `Season 01 - Vox Machina`, and it is
    /// kept. Plex takes the season from the marker in the filename, so the arc
    /// name costs it nothing: a library observed with three arc-named seasons
    /// renders all three correctly, while the one season that failed to appear
    /// was the one whose *files* carried no marker.
    ///
    /// It is dropped in exactly one case, in `render`: a file moving to a
    /// different season cannot bring the old season's arc name with it.
    Numbered { number: u32, suffix: String },
    /// A `Specials` directory, kept under its own name.
    Specials,
    /// The episode sits directly in the series directory.
    Absent,
}

/// An episode file, decomposed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Episode {
    pub root: LibraryRoot,
    /// Directories between the root and the season directory, preserved as they
    /// are. Usually just the series directory. Nothing here renames them.
    pub directories: Vec<String>,
    /// The season directory as it was found, which is evidence rather than
    /// instruction: what gets rendered comes from the marker in the filename.
    /// `Absent` means the file was not in one, and it will be moved into the
    /// season directory its own marker names.
    pub season_directory: SeasonDirectory,
    /// Directories between the season directory and the file. Rare - an
    /// `Extras` folder inside a season - and preserved as they are. Keeping
    /// them separate is what stops a second season directory being created
    /// underneath the first.
    pub nested_directories: Vec<String>,
    /// The series name as the *file* gives it, cleaned of separators. Falls back
    /// to the series directory only when the filename carries no name at all.
    pub series: String,
    /// The season the *file* claims, which is not necessarily its directory's.
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
    /// Directories between the root and the file - a film directory, or a
    /// collection - preserved as they are.
    pub directories: Vec<String>,
    pub title: String,
    pub year: u32,
    /// Resolution and frame rate, if the name carried them. Kept for the same
    /// reason an episode keeps it: it is information the library holds, and
    /// dropping it would be a silent loss.
    pub quality: Option<String>,
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
    /// A library root sits below another one - the same name twice, or one root
    /// nested inside a different one.
    ///
    /// Both names are carried because the reason has to be true of the path it
    /// is printed against: `Movies/Series/...` duplicates a root without
    /// repeating a name, and reporting only the inner one sends the reader
    /// looking for a nesting that is not there.
    DuplicatedRoot {
        /// The root the path starts at.
        outer: LibraryRoot,
        /// The root found below it.
        inner: LibraryRoot,
    },
    /// The path does not start at a known library root.
    OutsideLibrary,
    /// An episode file with no recognisable season/episode marker.
    NoEpisodeMarker,
    /// A marker naming a fraction of an episode, such as `S01E13.5`.
    FractionalEpisode,
    /// Neither the filename nor the directory it sits in names the series.
    NoSeriesName,
    /// A movie file whose name is nothing but a year and release metadata.
    NoMovieTitle,
    /// A movie file with no release year, which cannot be invented.
    NoReleaseYear,
    /// The file and the directory holding it name different years.
    ConflictingYear { directory: u32, file: u32 },
    /// The path has no filename, or a filename with no extension.
    NotAMediaFile,
}

impl Unresolvable {
    /// A one-line explanation, for the validation report.
    pub fn reason(&self) -> String {
        match self {
            Unresolvable::DuplicatedRoot { outer, inner } if outer == inner => format!(
                "'{}' appears twice in this path; the correct location is ambiguous",
                outer.as_str()
            ),
            Unresolvable::DuplicatedRoot { outer, inner } => format!(
                "'{}' sits inside the '{}' root; the correct location is ambiguous",
                inner.as_str(),
                outer.as_str()
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
            Unresolvable::FractionalEpisode => {
                "the marker names a fraction of an episode, which has no canonical form; calling it a whole episode would collide with the real one"
                    .to_string()
            }
            Unresolvable::NoSeriesName => {
                "neither the file nor its directory names the series".to_string()
            }
            Unresolvable::NoMovieTitle => {
                "no film title left once the year and release metadata are removed".to_string()
            }
            Unresolvable::ConflictingYear { directory, file } => format!(
                "the directory says {directory} and the file says {file}; which is right is not recoverable from the path"
            ),
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
    use std::fs;
    use tempfile::TempDir;

    /// A media root that happens to be *called* after a library root, holding
    /// the three real roots beneath it. An ordinary layout for someone whose
    /// media partition is named `Movies`.
    fn media_root_named(name: &str) -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path().join("home").join("bob").join(name);

        for root in LibraryRoot::all() {
            fs::create_dir_all(media_root.join(root.as_str())).unwrap();
        }

        (temp_dir, media_root)
    }

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

    /// The quality metadata moves into brackets, and the file moves into a
    /// season directory.
    ///
    /// Issue #51 wrote this case out with the file staying loose in the series
    /// directory, because at the time nothing created season directories.
    /// Issue #86 supersedes that: an episode belongs in the season its marker
    /// names, and `Season 01` here does not exist yet.
    #[test]
    fn moves_quality_metadata_into_brackets_and_the_file_into_its_season() {
        assert_eq!(
            assess(Path::new(
                "Series/Super Best Friends Play - FFX/Super Best Friends Play - Final Fantasy X - S01E13 (1080p60).webm"
            )),
            Assessment::Rename {
                destination:
                    "Series/Super Best Friends Play - FFX/Season 01/Super Best Friends Play - Final Fantasy X - S01E13 [1080p60].webm"
                        .to_string()
            }
        );
    }

    #[test]
    fn moves_an_episode_out_of_the_wrong_season_directory() {
        assert_eq!(
            assess(Path::new(
                "Series/Elementary/Season 05/Elementary - S06E08 - Sand Trap.mkv"
            )),
            Assessment::Rename {
                destination: "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv"
                    .to_string()
            }
        );
    }

    #[test]
    fn corrects_the_season_directory_without_nesting_a_second_one() {
        assert_eq!(
            assess(Path::new(
                "Series/Elementary/Season 6/Extras/Elementary - S06E08 - Sand Trap.mkv"
            )),
            Assessment::Rename {
                destination:
                    "Series/Elementary/Season 06/Extras/Elementary - S06E08 - Sand Trap.mkv"
                        .to_string()
            }
        );
    }

    #[test]
    fn files_a_season_zero_episode_under_specials() {
        assert_eq!(
            assess(Path::new(
                "Series/Firefly/Season 00/Firefly - S00E01 - Making Of.mkv"
            )),
            Assessment::Rename {
                destination: "Series/Firefly/Specials/Firefly - S00E01 - Making Of.mkv".to_string()
            }
        );
        assert_eq!(
            assess(Path::new(
                "Series/Firefly/Specials/Firefly - S00E01 - Making Of.mkv"
            )),
            Assessment::Canonical
        );
    }

    #[test]
    fn refuses_to_rewrite_a_duplicated_library_root() {
        assert_eq!(
            assess(Path::new(
                "Series/Veronica Mars/Series/Veronica Mars S02E04/Season 01/Veronica Mars S02E04.mp4"
            )),
            Assessment::Unresolvable(Unresolvable::DuplicatedRoot {
                outer: LibraryRoot::Series,
                inner: LibraryRoot::Series
            })
        );
    }

    /// Issue #137: the reason has to be true of the path it is printed against.
    /// A `Series` directory inside a `Movies` root is a duplicated root, but
    /// neither name appears twice, so saying so sends the reader hunting for a
    /// nesting that is not there.
    #[test]
    fn a_root_inside_a_different_root_names_both_in_its_reason() {
        let assessment = assess(Path::new(
            "Movies/Series/Elementary/Season 01/Elementary - S01E01 - Pilot.mkv",
        ));

        let Assessment::Unresolvable(unresolvable) = assessment else {
            panic!("a root inside another root has no unambiguous destination");
        };

        let reason = unresolvable.reason();
        assert!(
            reason.contains("Series") && reason.contains("Movies"),
            "the reason names one root and claims it appears twice: {reason}"
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
            "Series/Some Show/Some Show - S02E03 - Loose Episode.mkv",
            "Series/Some Show/Season 01/Extras/Some Show - S01E02 - Behind It.mkv",
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

    #[test]
    fn a_path_with_no_library_root_is_the_library_root() {
        let scope = scope_for(Path::new("/media/library"));

        assert_eq!(scope.library_root, PathBuf::from("/media/library"));
        assert_eq!(scope.scan_path, PathBuf::from("/media/library"));
        assert!(scope.is_whole_library());
    }

    #[test]
    fn pointing_at_a_series_still_measures_from_the_library_root() {
        let scope = scope_for(Path::new("/media/library/Series/Elementary"));

        assert_eq!(scope.library_root, PathBuf::from("/media/library"));
        assert_eq!(
            scope.scan_path,
            PathBuf::from("/media/library/Series/Elementary")
        );
        assert!(!scope.is_whole_library());
    }

    #[test]
    fn pointing_at_a_season_works_the_same_way() {
        let scope = scope_for(Path::new("/media/library/Anime/Cowboy Bebop/Season 01"));

        assert_eq!(scope.library_root, PathBuf::from("/media/library"));
    }

    #[test]
    fn pointing_at_a_root_directory_scopes_to_it() {
        let scope = scope_for(Path::new("/media/library/Movies"));

        assert_eq!(scope.library_root, PathBuf::from("/media/library"));
        assert_eq!(scope.scan_path, PathBuf::from("/media/library/Movies"));
    }

    #[test]
    fn a_tree_nested_into_itself_resolves_to_the_outer_root() {
        let scope = scope_for(Path::new(
            "/media/library/Series/Veronica Mars/Series/Season 01",
        ));

        assert_eq!(
            scope.library_root,
            PathBuf::from("/media/library"),
            "the inner Series is the duplication we report, not a library of its own"
        );
    }

    /// Issue #137: a media root named after a library root made `validate`
    /// refuse every file in the library at once.
    #[test]
    fn a_media_root_named_after_a_library_root_is_still_the_library_root() {
        for name in ["Movies", "Anime", "Series"] {
            let (_temp_dir, media_root) = media_root_named(name);

            let scope = scope_for(&media_root);

            assert_eq!(
                scope.library_root, media_root,
                "a {name} directory holding the three roots is the media root, not one of them"
            );
            assert!(scope.is_whole_library());
        }
    }

    #[test]
    fn scoping_inside_a_media_root_named_after_a_library_root_measures_from_it() {
        let (_temp_dir, media_root) = media_root_named("Movies");
        let series = media_root.join("Series").join("Elementary");
        fs::create_dir_all(&series).unwrap();

        let scope = scope_for(&series);

        assert_eq!(
            scope.library_root, media_root,
            "the outer Movies is the media root; the real root is the Series below it"
        );
        assert_eq!(scope.scan_path, series);
    }

    /// The other reading of the same name, which must keep working: `/srv/Anime`
    /// holding series directories directly *is* the Anime root.
    #[test]
    fn a_root_directory_holding_series_directly_is_a_root() {
        let temp_dir = TempDir::new().unwrap();
        let anime = temp_dir.path().join("srv").join("Anime");
        fs::create_dir_all(anime.join("Cowboy Bebop").join("Season 01")).unwrap();

        let scope = scope_for(&anime);

        assert_eq!(scope.library_root, temp_dir.path().join("srv"));
        assert_eq!(scope.scan_path, anime);
    }

    /// A resolved Windows path carries a `Prefix` component and, canonicalised,
    /// the verbatim `\\?\C:\…` form. Neither may throw the root search off.
    #[test]
    fn a_windows_shaped_media_root_named_after_a_library_root_is_the_library_root() {
        let (_temp_dir, media_root) = media_root_named("Movies");
        let resolved = fs::canonicalize(&media_root).unwrap();

        let scope = scope_for(&resolved);

        assert_eq!(scope.library_root, resolved);
        assert!(scope.is_whole_library());
    }

    /// The tree that really is nested into itself, this time on disk, so the
    /// evidence below the path is real rather than absent.
    #[test]
    fn a_tree_nested_into_itself_on_disk_resolves_to_the_outer_root() {
        let temp_dir = TempDir::new().unwrap();
        let nested = temp_dir
            .path()
            .join("Series")
            .join("Veronica Mars")
            .join("Series")
            .join("Season 01");
        fs::create_dir_all(&nested).unwrap();

        let scope = scope_for(&nested);

        assert_eq!(
            scope.library_root,
            temp_dir.path(),
            "the inner Series is the duplication we report, not a library of its own"
        );
    }

    #[test]
    fn a_directory_merely_containing_a_root_name_is_not_one() {
        let scope = scope_for(Path::new("/media/library/Movies About Series"));

        assert_eq!(
            scope.library_root,
            PathBuf::from("/media/library/Movies About Series")
        );
    }

    #[test]
    fn scoping_does_not_change_what_canonical_means() {
        let scope = scope_for(Path::new("/media/library/Series/Elementary"));
        let file = Path::new("Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv");

        // The relative path is what gets judged, and it is the same either way.
        assert_eq!(
            assess(file),
            Assessment::Rename {
                destination: "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv"
                    .to_string()
            }
        );
        assert_eq!(scope.library_root, PathBuf::from("/media/library"));
    }

    #[test]
    fn a_relative_path_starting_at_a_root_falls_back_to_the_current_directory() {
        let scope = scope_for(Path::new("Series/Elementary"));

        assert_eq!(
            scope.library_root,
            PathBuf::from("."),
            "an empty root would give the ignore filter nothing to walk"
        );
        assert_eq!(scope.scan_path, PathBuf::from("Series/Elementary"));
    }

    /// Names taken verbatim from a real library, each one a shape the parser got
    /// wrong the first time it met it.
    #[test]
    fn strips_a_scene_release_tail_from_the_title() {
        assert_eq!(
            assess(Path::new(
                "Series/Blackadder (1982)/Season 01/Blackadder.S01E01.The.Foretelling.1080p.BluRay.EAC3.2.0.1080p.x265-iVy.mkv"
            )),
            Assessment::Rename {
                destination:
                    "Series/Blackadder (1982)/Season 01/Blackadder - S01E01 - The Foretelling [1080p].mkv"
                        .to_string()
            }
        );
    }

    #[test]
    fn keeps_a_dash_that_belongs_to_the_title() {
        assert_eq!(
            assess(Path::new(
                "Series/Show/Season 01/Show - S01E01 - Dungeons & Dragons - Honour Among Thieves.mkv"
            )),
            Assessment::Canonical
        );
    }

    #[test]
    fn treats_bracketed_groups_as_metadata_rather_than_a_title() {
        assert_eq!(
            assess(Path::new(
                "Anime/Kill la Kill/Season 01/Kill la Kill - S01E01 - [1080p][HorribleSubs].mkv"
            )),
            Assessment::Rename {
                destination: "Anime/Kill la Kill/Season 01/Kill la Kill - S01E01 [1080p].mkv"
                    .to_string()
            }
        );
    }

    #[test]
    fn discards_a_fansub_checksum() {
        assert_eq!(
            assess(Path::new(
                "Anime/Made In Abyss/Season 02/Made In Abyss - S02E01 - [1080p][HorribleSubs] [73241537].mkv"
            )),
            Assessment::Rename {
                destination: "Anime/Made In Abyss/Season 02/Made In Abyss - S02E01 [1080p].mkv"
                    .to_string()
            }
        );
    }

    #[test]
    fn refuses_a_fractional_episode_rather_than_rounding_it_into_another() {
        assert_eq!(
            assess(Path::new(
                "Anime/Shingeki no Kyojin/Season 01/Shingeki no Kyojin - S01E13.5 - [1080p][HorribleSubs].mkv"
            )),
            Assessment::Unresolvable(Unresolvable::FractionalEpisode),
            "S01E13.5 is a half episode; calling it E13 would collide with the real one"
        );
    }

    #[test]
    fn keeps_the_quality_on_a_movie() {
        assert_eq!(
            assess(Path::new(
                "Movies/Knives Out (2019)/Knives Out (2019) - [1080p].mp4"
            )),
            Assessment::Rename {
                destination: "Movies/Knives Out (2019)/Knives Out (2019) [1080p].mp4".to_string()
            }
        );
    }

    #[test]
    fn keeps_a_dash_in_a_movie_title() {
        assert_eq!(
            assess(Path::new(
                "Movies/Dungeons & Dragons - Honour Among Thieves (2023)/Dungeons & Dragons - Honour Among Thieves (2023)[720p].mkv"
            )),
            Assessment::Rename {
                destination:
                    "Movies/Dungeons & Dragons - Honour Among Thieves (2023)/Dungeons & Dragons - Honour Among Thieves (2023) [720p].mkv"
                        .to_string()
            }
        );
    }

    #[test]
    fn does_not_mangle_a_movie_carrying_several_bracketed_groups() {
        assert_eq!(
            assess(Path::new(
                "Movies/Made In Abyss - Fukaki Tamashii no Reimei (2020)/Made in Abyss - Fukaki Tamashii no Reimei - [1080p][Multiple Subtitle].mkv"
            )),
            Assessment::Rename {
                destination:
                    "Movies/Made In Abyss - Fukaki Tamashii no Reimei (2020)/Made in Abyss - Fukaki Tamashii no Reimei (2020) [1080p].mkv"
                        .to_string()
            }
        );
    }

    #[test]
    fn refuses_a_movie_whose_file_and_directory_disagree_about_the_year() {
        assert_eq!(
            assess(Path::new(
                "Movies/Kubo and the Two Strings (2016)/Kubo and the Two Strings (2018) - [1080p].mkv"
            )),
            Assessment::Unresolvable(Unresolvable::ConflictingYear {
                directory: 2016,
                file: 2018
            })
        );
    }

    /// Every one of these is a name the tail cut damaged, from review of the
    /// change that introduced it. A title is not release metadata just because
    /// it contains a word that also names a codec.
    #[test]
    fn a_technical_word_inside_a_title_does_not_end_it() {
        for path in [
            "Series/Show/Season 01/Show - S01E01 - The DTS Report.mkv",
            "Series/Show/Season 01/Show - S01E01 - Atmos of Fear.mkv",
            "Series/Show/Season 01/Show - S01E01 - Opus and Bill.mkv",
        ] {
            assert_eq!(
                assess(Path::new(path)),
                Assessment::Canonical,
                "the title of {path} is already correct and must survive"
            );
        }
    }

    #[test]
    fn a_film_named_after_a_codec_word_keeps_its_name() {
        assert_eq!(
            assess(Path::new(
                "Movies/Mr Hollands Opus (1995)/Mr Hollands Opus (1995).mkv"
            )),
            Assessment::Canonical
        );
    }

    #[test]
    fn one_technical_word_is_not_enough_to_read_a_name_as_a_release() {
        assert_eq!(
            assess(Path::new(
                "Series/Show/Season 01/Show - S01E01 - The Bluray Heist.mkv"
            )),
            Assessment::Canonical,
            "a spaced name carrying one such word is a title, not a scene release"
        );
    }

    #[test]
    fn a_movie_does_not_keep_its_quality_in_the_title_as_well() {
        assert_eq!(
            assess(Path::new("Movies/Show (2001)/Show 2160p (2001).mkv")),
            Assessment::Rename {
                destination: "Movies/Show (2001)/Show (2001) [2160p].mkv".to_string()
            }
        );
    }

    #[test]
    fn a_year_after_the_marker_is_not_a_fraction_of_an_episode() {
        let assessment = assess(Path::new(
            "Series/Show/Season 01/Show.S01E01.2020.1080p.WEB.mkv",
        ));

        assert!(
            !matches!(
                assessment,
                Assessment::Unresolvable(Unresolvable::FractionalEpisode)
            ),
            "`.2020` is a year, not a half episode: {assessment:?}"
        );
    }

    #[test]
    fn a_parenthesised_part_of_a_title_is_kept() {
        assert_eq!(
            assess(Path::new(
                "Series/Show/Season 01/Show - S01E01 - The Beginning (Part 1).mkv"
            )),
            Assessment::Canonical
        );
    }

    /// What matters is that the edition survives, so the two cuts of one film
    /// keep separate names. Dropping it made them collide, and `fix` then
    /// refused both for ever. Leaving the name alone satisfies that and touches
    /// nothing, which is the better of the two ways to satisfy it.
    #[test]
    fn an_edition_keeps_a_film_apart_from_the_plain_cut() {
        let edition = "Movies/Alien (1979)/Alien (Directors Cut) (1979).mkv";
        let plain = "Movies/Alien (1979)/Alien (1979).mkv";

        assert_eq!(assess(Path::new(edition)), Assessment::Canonical);
        assert_eq!(assess(Path::new(plain)), Assessment::Canonical);

        // Where each one belongs, said plainly: two names in, two names out.
        assert_ne!(
            render(&parse(edition).unwrap()),
            render(&parse(plain).unwrap()),
            "two cuts of one film must not resolve to one name"
        );
    }

    #[test]
    fn season_zero_is_specials_whatever_the_directory_is_called() {
        assert_eq!(
            assess(Path::new(
                "Series/Show/Season 00 - Extras/Show - S00E01 - Behind the Scenes.mkv"
            )),
            Assessment::Rename {
                destination: "Series/Show/Specials/Show - S00E01 - Behind the Scenes.mkv"
                    .to_string()
            },
            "an arc name must not give season zero a second canonical home"
        );
    }

    #[test]
    fn a_year_in_a_collection_directory_is_not_the_films_year() {
        assert_eq!(
            assess(Path::new(
                "Movies/Marvel 2012 Collection/Iron Man 3 (2013).mkv"
            )),
            Assessment::Canonical
        );
    }

    #[test]
    fn a_number_in_a_film_title_is_not_its_year() {
        assert_eq!(
            assess(Path::new(
                "Movies/Blade Runner 2049 (2017)/Blade Runner 2049 (2017).mkv"
            )),
            Assessment::Canonical,
            "the parenthesised year is the year; 2049 is part of the name"
        );
    }
}
