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
    /// A scope whose root the **user stated**, rather than one inferred from the
    /// shape of the tree.
    ///
    /// [`scope_for`] has to guess, and one shape is genuinely undecidable from
    /// disk: a media root named after a library root that holds exactly one -
    /// `/srv/Movies` containing only `Movies/`. Nothing below it separates that
    /// from a duplication, and descending further only moves the guess, since a
    /// tree rsynced into itself contains season directories too. So the probe
    /// refuses it, and refusing is the right way to be wrong about a *guess*.
    ///
    /// It is the wrong way to be wrong about a fact the user knows. This turns
    /// the inference off rather than making it cleverer: the root is taken as
    /// given, no directory is listed, and the only judgement left is whether the
    /// two paths describe one tree.
    ///
    /// The two paths are kept **as the user spelled them**, made absolute and no
    /// more. Every path validation walks comes out of `scan_path`, so the root
    /// has to be a prefix of that same spelling for `strip_prefix` to be sound -
    /// and canonicalising to guarantee it would put a Windows `\\?\` prefix into
    /// the report and into the fix record, where a person then cannot paste it
    /// back into the command that produced it.
    ///
    /// The price is that two spellings of one directory - a `..` in the middle,
    /// a differing drive-letter case - are refused rather than reconciled. That
    /// is the same trade the rest of this area makes: a refusal names the two
    /// paths and is answered by retyping one of them, which is cheaper than
    /// being wrong about which tree was meant.
    ///
    /// Refuses rather than guesses on both counts: a root that is not a readable
    /// directory, and a scan path that is not inside it.
    pub fn stated(library_root: &Path, scan_path: &Path) -> anyhow::Result<Self> {
        let library_root = absolute(library_root);
        let scan_path = absolute(scan_path);

        if !library_root.is_dir() {
            return Err(anyhow::anyhow!(
                "library root {} is not a readable directory",
                library_root.display()
            ));
        }

        if !scan_path.starts_with(&library_root) {
            return Err(anyhow::anyhow!(
                "{} is not inside the library root {}; both are read as spelled, so a '..' or a \
                 differing drive letter has to be retyped rather than resolved",
                scan_path.display(),
                library_root.display()
            ));
        }

        Ok(Scope {
            library_root,
            scan_path,
        })
    }

    /// Whether this run covers the whole library.
    pub fn is_whole_library(&self) -> bool {
        self.library_root == self.scan_path
    }
}

/// Resolve a path against the current directory without otherwise changing it.
///
/// A relative path that *starts* at a root - plain `Series/Elementary` - has
/// nothing before that component to be the library root, so it has to be made
/// absolute before anything can be measured from it.
fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }

    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
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
/// the directory decides, so **this function reads that one directory from
/// disk** - see [`holds_library_roots`] for what it counts and why. Where no
/// candidate survives, the whole path is the library root, which is also what
/// happens for a path that names no root at all, so a whole-library run works
/// exactly as before.
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

/// Whether a directory holds **more than one** library root directly beneath it,
/// and so is the media root rather than a root itself.
///
/// **This is the one place the naming module reads a disk**, and it reads
/// exactly one directory. Nothing else here touches a filesystem; keep it that
/// way, and keep this out of the parse/render core.
///
/// More than one is the whole rule, and the count is doing real work. A single
/// root-named child is character-for-character the observation
/// [`Unresolvable::DuplicatedRoot`] exists to report, and the two readings of it
/// cannot be told apart from the disk:
///
/// ```text
/// lib/Movies/Series/…   a media root holding one library, or a film called Series
/// lib/Series/Series/…   a media root holding one library, or a tree rsynced into itself
/// ```
///
/// Reading either as a media root moves the library root a level in, and the
/// consequence is not a worse message but a worse *action*: the film directory
/// then reads as a `Series` library, so an episode inside it earns a
/// well-formed destination and `validate --fix` builds a season directory inside
/// a film folder. `fix.rs` cannot catch that - the destination came out of
/// `render` and is canonical; it is the root underneath it that is wrong. So one
/// root-named child belongs to `parse`, which refuses it and says why.
///
/// Two or more distinct roots is not ambiguous in the same way. No single
/// library contains two others, so a directory holding `Series/` and `Movies/`
/// is a media root and nothing else, whatever it is called.
///
/// The cost of that is one shape left unfixed: a media root named after a
/// library root that holds exactly *one* root - `/srv/Movies` containing only
/// `Movies/`. It is refused rather than descended into, which is what happened
/// before this probe existed. Refusing a library is recoverable by hand; a file
/// moved to the wrong place is not.
///
/// Directories are counted, not names: a stray *file* called `Series` is not a
/// library root. A path that cannot be read - it does not exist, or the caller
/// cannot list it - yields no evidence, and no evidence leaves the name
/// standing, which again lands on refusal rather than on a bad destination.
fn holds_library_roots(directory: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return false;
    };

    let mut found: Vec<LibraryRoot> = Vec::new();

    for entry in entries.flatten() {
        let Some(root) = LibraryRoot::from_component(&entry.file_name().to_string_lossy()) else {
            continue;
        };

        if entry.path().is_dir() && !found.contains(&root) {
            found.push(root);

            if found.len() > 1 {
                return true;
            }
        }
    }

    false
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
    /// The first episode the file covers.
    pub number: u32,
    /// The last episode it covers, when the marker names two - a double episode
    /// held in one file, `S04E01-E02`.
    ///
    /// It is a field rather than something the title absorbs because it is a
    /// *value*: folded into the title, `E02` becomes four characters that no
    /// longer say the file contains an episode, and the library then claims an
    /// episode nothing appears to hold.
    pub through: Option<u32>,
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
    /// A marker whose two episodes are in different seasons, `S01E12-S02E01`.
    EpisodeRangeAcrossSeasons { from: u32, to: u32 },
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
            Unresolvable::EpisodeRangeAcrossSeasons { from, to } => format!(
                "the marker covers seasons {from} and {to}; no one name states that, and the season directory it belongs in is not decidable either"
            ),
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

/// A file whose series directory names a different series than the file does.
///
/// The filename is what this module parses and what Plex reads, so a directory
/// disagreeing with it is worth saying out loud. Which of the two is *right* is
/// not decidable from the path - a directory is often a curated abbreviation,
/// and a filename can simply be wrong - so this is a note on a file and never a
/// proposal. Nothing here renames a series directory: that would move every
/// file in it on evidence that does not support the move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesDirectoryDisagreement {
    /// The series directory as it is on disk.
    pub directory: String,
    /// The series name the file gives.
    pub series: String,
}

/// Note whether the series directory holding a file disagrees with the file.
///
/// Both names are already in hand once the path is parsed, so this is a
/// comparison rather than a second reading of the path. A file can be canonical
/// and still sit in a disagreeing directory, and a file can need a rename *and*
/// be in one, which is why this is separate from [`assess`] rather than a
/// variant of it.
pub fn series_directory_disagreement(relative_path: &Path) -> Option<SeriesDirectoryDisagreement> {
    let path = to_forward_slashes(relative_path);

    let episode = match parse(&path) {
        Ok(MediaName::Episode(episode)) => episode,
        _ => return None,
    };

    // Read the directory the same way the parser does when it falls back to it,
    // so an annotation such as `(2008) {tvdb-81189}` is not mistaken for a
    // different name.
    let directory = episode.directories.last()?;
    let stated = parse::series_name_from_directory(directory);

    // A directory that states nothing cannot disagree, and a difference of case
    // is not a disagreement about which series this is.
    if stated.is_empty() || stated.eq_ignore_ascii_case(&episode.series) {
        return None;
    }

    Some(SeriesDirectoryDisagreement {
        directory: directory.clone(),
        series: episode.series,
    })
}

/// Where an episode sits in the order work should be done in.
///
/// This is a **different question** from what the file should be called, and the
/// difference is the whole reason it is a separate type. A destination has to be
/// right: a wrong one moves a file somewhere nobody will look for it, and only
/// `undo` gets it back. An order only has to be useful, and being wrong about it
/// costs a worker some minutes spent on the wrong episode.
///
/// So [`parse`] refuses a path it cannot decompose, and [`sort_key`] answers for
/// paths it refuses. A file whose directory is named after an episode, a half
/// episode, a tree nested into itself - none of those has a canonical name, and
/// all of them have an obvious place in a queue.
///
/// The fields are ordered so the derived `Ord` is the ordering itself: group,
/// then season, then episode.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct EpisodeSortKey {
    /// The library-relative directory chain above the season directory, root
    /// included - `["Series", "Breaking Bad (2008)"]`.
    ///
    /// **Where the file is, not what it is called.** The rendered series name is
    /// the wrong key to group by in both directions. It drops the year, so
    /// `Breaking Bad (2008)` and `Breaking Bad (2020)` become one indistinguishable
    /// group; and it comes from the filename, so one directory holding both
    /// `Show - Long Name - S01E13.webm` and `S01E14.webm` becomes two groups with
    /// room for another series to sort between them. The directory is one string
    /// for the whole season in the second case and two different strings in the
    /// first, which is exactly the other way round from what the name gives.
    ///
    /// **The components are held apart because string order on a joined path is
    /// not tree order.** One series produces two groups when a subdirectory
    /// holding episodes has no season directory above it - `Doctor Who/Extras`
    /// beside `Doctor Who` - and the contiguity the prioritiser offers needs
    /// those two adjacent. Joined, they are not: `/` is `0x2F`, so a sibling
    /// differing by any character below it - a space, `-`, `.` - sorts between
    /// `Series/Doctor Who` and `Series/Doctor Who/Extras`. Compared element-wise
    /// the boundary between two components is not a character a name can sort
    /// under, so a difference deeper in the chain never outranks one higher up -
    /// which is what the two groups of a series need, whether the sibling's
    /// chain is shorter, longer, or the same length.
    pub series_directory: Vec<String>,
    /// The season the *file's* marker claims, not its directory's - the same
    /// choice `render` makes, so a misfiled episode is ordered where it belongs.
    pub season: u32,
    pub episode: u32,
}

/// Group and order an episode file, for a caller that needs a queue order rather
/// than a name.
///
/// Takes a path relative to the library root. `None` means the path holds
/// nothing to order by: it is not under `Series` or `Anime`, or its filename
/// carries no episode marker. A film is `None`, which is what puts films behind
/// episodes.
///
/// See [`EpisodeSortKey`] for why this is allowed to answer where [`parse`] is
/// not.
pub fn sort_key(relative_path: &Path) -> Option<EpisodeSortKey> {
    parse::sort_key(&to_forward_slashes(relative_path))
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

    #[test]
    fn reports_a_directory_named_after_an_episode_instead_of_growing_the_name() {
        assert_eq!(
            assess(Path::new("Series/S01E01/Season 01/S01E01 - x.mkv")),
            Assessment::Unresolvable(Unresolvable::NoSeriesName),
            "neither the file nor its directory names the series, and a marker is not a name"
        );
    }

    /// The property the whole module rests on: whatever we propose must itself
    /// be canonical, or `--fix` would move a file twice.
    ///
    /// The corpus below is a list of tidy paths, all of which name their series
    /// in the filename. That is the shape the property is *least* likely to fail
    /// on, so [`no_path_is_renamed_twice`] widens it to paths whose series name
    /// has to come from somewhere else.
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

    /// The same property over a generated corpus rather than a hand-written one.
    ///
    /// Every combination of a series directory, a season directory and a
    /// filename below is assessed, and a proposal for any of them has to be
    /// canonical in its own right. Refusing a path is an acceptable answer here
    /// and a rename to a path that would be renamed again is not, which is what
    /// separates a heuristic that gives up from one that grows a filename.
    ///
    /// It is not, on its own, enough. Stability says nothing about whether the
    /// parse behind it threw something away - see
    /// [`parse::tests::a_marker_never_stops_in_the_middle_of_a_marker`], which is
    /// the half of the property this one cannot see.
    #[test]
    fn no_path_is_renamed_twice() {
        let series_directories = [
            "Elementary",
            "Elementary (2012)",
            "Breaking Bad (2008) {tvdb-81189}",
            "Super Best Friends Play - FFX",
            "S.W.A.T",
            "S01E01",
            "Veronica Mars S02E04",
            // Markers a directory can carry other than at the end, more than one
            // of them, and one alongside an annotation. Each leaves a different
            // remainder for the round trip to be wrong about.
            "S02E04 Veronica Mars",
            "Veronica Mars S02E04 (2004)",
            "Veronica Mars S02E04-S02E05",
            "Show.S01",
        ];
        let season_directories = [
            "",
            "Season 1/",
            "Season 01/",
            "Season 01 - The Arc/",
            "Season 00/",
            "Specials/",
            "Season 3/Extras/",
        ];
        let filenames = [
            "S01E01.mkv",
            "S01E01 - Pilot.mkv",
            "Elementary - S01E01 - Pilot.mkv",
            "Elementary.S01E01.Pilot.1080p.BluRay.x264.mkv",
            "s01e01 - pilot.mkv",
            "Elementary - S01E01 (1080p60).webm",
            "S00E01 - [1080p][HorribleSubs].mkv",
            // A file holding two episodes, in each form the marker recognises
            // and in the two that follow it - a bare second number, and a title
            // that opens with something marker-shaped.
            "S01E01-E02.mkv",
            "Elementary - S01E01-E02 - Pilot & Aftermath.mkv",
            "Elementary.S01E01-E02.Pilot.1080p.BluRay.x264.mkv",
            "elementary - s01e01-s01e02 - pilot.mkv",
            "Elementary - S01E01-02 - Pilot.mkv",
            "Elementary - S01E01 - E02 Is A Title.mkv",
        ];

        let mut proposals = 0;
        for series_directory in series_directories {
            for season_directory in season_directories {
                for filename in filenames {
                    let path = format!("Series/{series_directory}/{season_directory}{filename}");

                    let destination = match assess(Path::new(&path)) {
                        Assessment::Rename { destination } => destination,
                        // Canonical is the property holding trivially, and a
                        // refusal puts the path in front of a person, which is
                        // the honest answer when nothing names the series.
                        Assessment::Canonical | Assessment::Unresolvable(_) => continue,
                    };
                    proposals += 1;

                    assert_eq!(
                        assess(Path::new(&destination)),
                        Assessment::Canonical,
                        "a second run would move this again: {path} -> {destination}"
                    );
                }
            }
        }

        assert!(
            proposals > 0,
            "the corpus proposed nothing, so it proved nothing"
        );
    }

    #[test]
    fn notes_a_series_directory_that_names_something_else() {
        assert_eq!(
            series_directory_disagreement(Path::new(
                "Series/Super Best Friends Play - FFX/Season 01/Super Best Friends Play - Final Fantasy X - S01E13.webm"
            )),
            Some(SeriesDirectoryDisagreement {
                directory: "Super Best Friends Play - FFX".to_string(),
                series: "Super Best Friends Play - Final Fantasy X".to_string(),
            })
        );
    }

    #[test]
    fn a_canonical_file_can_still_sit_in_a_disagreeing_directory() {
        let path = Path::new("Series/FFX/Season 01/Final Fantasy X - S01E13 - Zanarkand.webm");

        assert_eq!(
            assess(path),
            Assessment::Canonical,
            "the note is not a verdict, and must not turn a correct file into a rename"
        );
        assert_eq!(
            series_directory_disagreement(path),
            Some(SeriesDirectoryDisagreement {
                directory: "FFX".to_string(),
                series: "Final Fantasy X".to_string(),
            })
        );
    }

    #[test]
    fn says_nothing_when_the_directory_and_the_file_agree() {
        for path in [
            // The names match outright.
            "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv",
            // An annotation on the directory describes the series rather than
            // being part of its name.
            "Series/Breaking Bad (2008) {tvdb-81189}/Season 01/Breaking Bad - S01E01 - Pilot.mkv",
            // A difference of case is not a disagreement about which series.
            "Series/breaking bad/Season 01/Breaking Bad - S01E01 - Pilot.mkv",
            // The filename names no series, so the directory is where the name
            // came from and cannot contradict it.
            "Series/Breaking Bad/Season 01/S01E01 - Pilot.mkv",
            // Same, with a marker glued onto the directory. Both readings of a
            // directory go through one function, so the name the parse took out
            // of it cannot then be reported as disagreeing with it.
            "Series/Veronica Mars S02E04/Season 02/S02E01.mp4",
            // A film has no series directory to disagree with.
            "Movies/The Dark Knight (2008)/The Dark Knight (2008).mkv",
            // A path that cannot be parsed says nothing about anything.
            "Series/S01E01/Season 01/S01E01 - x.mkv",
        ] {
            assert_eq!(
                series_directory_disagreement(Path::new(path)),
                None,
                "for {path}"
            );
        }
    }

    fn key(path: &str) -> EpisodeSortKey {
        sort_key(Path::new(path)).unwrap_or_else(|| panic!("no sort key for {path}"))
    }

    /// A group written the way a path reads, for an assertion to state.
    fn group(directories: &str) -> Vec<String> {
        directories.split('/').map(str::to_string).collect()
    }

    /// Issue #138: the year is what tells two shows of one name apart, and it
    /// lives on the directory precisely because the canonical *filename* does
    /// not carry it. Grouping by the rendered name throws it away and leaves the
    /// comparator returning `Equal` for every pair, so the two are interleaved.
    #[test]
    fn a_reboot_pair_is_two_groups_rather_than_one() {
        let original = "Series/Breaking Bad (2008)/Season 01/Breaking Bad - S01E01 - Pilot.mkv";
        let reboot = "Series/Breaking Bad (2020)/Season 01/Breaking Bad - S01E01 - Pilot.mkv";

        let series_of = |path: &str| match parse(path).unwrap() {
            MediaName::Episode(episode) => episode.series,
            other => panic!("expected an episode, got {other:?}"),
        };
        assert_eq!(
            series_of(original),
            series_of(reboot),
            "the rendered name is the same for both, which is what made them one group"
        );
        assert_ne!(
            key(original),
            key(reboot),
            "two shows of one name must not share a group"
        );
    }

    /// The other direction of the same mistake: the filename wins over the
    /// directory when a name is *rendered*, so one season directory whose files
    /// are inconsistently named becomes two groups, with room for another series
    /// to sort between them.
    #[test]
    fn one_series_directory_is_one_group_however_its_files_are_named() {
        let named = "Series/Super Best Friends Play - FFX/Season 01/Super Best Friends Play - Final Fantasy X - S01E13.webm";
        let bare = "Series/Super Best Friends Play - FFX/Season 01/S01E14.webm";

        assert_eq!(key(named).series_directory, key(bare).series_directory);
        assert!(key(named) < key(bare), "and ordered by their markers");
    }

    /// The group is where the file is, so a series of one name under two roots
    /// stays two series.
    #[test]
    fn the_root_is_part_of_the_group() {
        assert_ne!(
            key("Series/Trigun/Season 01/Trigun - S01E01 - The 60 Billion Double Dollar Man.mkv"),
            key("Anime/Trigun/Season 01/Trigun - S01E01 - The 60 Billion Double Dollar Man.mkv")
        );
    }

    /// A season directory is what the marker orders *within*, so it cannot be
    /// part of the group - an unpadded season, an arc name, `Specials` and a
    /// missing season directory all describe one series.
    #[test]
    fn the_season_directory_is_not_part_of_the_group() {
        let expected = group("Series/Critical Role");

        for path in [
            "Series/Critical Role/Season 1/Critical Role - S01E12 - Kraghammer.mkv",
            "Series/Critical Role/Season 01 - Vox Machina/Critical Role - S01E12 - Kraghammer.mkv",
            "Series/Critical Role/Specials/Critical Role - S00E01 - Talks Machina.mkv",
            "Series/Critical Role/Critical Role - S01E12 - Kraghammer.mkv",
            "Series/Critical Role/Season 01/Extras/Critical Role - S01E12 - Clip.mkv",
        ] {
            assert_eq!(key(path).series_directory, expected, "for {path}");
        }
    }

    /// Issue #191: the contiguity `--priority episode` offers rests on every
    /// file of one series producing the same group, and a subdirectory holding
    /// episodes with no season directory above them produces a different one.
    /// Two groups of one series still sort together when they are compared
    /// component-wise; compared as joined strings they do not, because `/` is
    /// `0x2F` and a sibling name differing by any character below it - a space,
    /// `-`, `.` - sorts between them.
    #[test]
    fn a_sibling_directory_cannot_split_a_series_in_two() {
        let proper = key("Series/Doctor Who/Season 01/Doctor Who - S01E01 - Rose.mkv");
        let extras = key("Series/Doctor Who/Extras/Doctor Who - S01E01 - Rose.mkv");
        let sibling = key(
            "Series/Doctor Who - Confidential/Season 01/Doctor Who Confidential - S01E01 - Bringing Back the Doctor.mkv",
        );

        // The whole order, stated as strict comparisons, because adjacency does
        // not discriminate and equality does not either: a key that dropped the
        // group would make all three equal, and equal keys stay in the order
        // they were given in - which a sorted vector then reads back as a pass.
        assert!(
            proper < extras,
            "the two groups of one series must order together: {proper:?} then {extras:?}"
        );
        assert!(
            extras < sibling,
            "another series must sort outside both, not between them: {extras:?} then {sibling:?}"
        );
    }

    /// Everything the queue orders, it orders by the marker in the filename -
    /// the same evidence `render` files the episode under, so a misfiled episode
    /// is worked in the season it belongs to rather than the one it sits in.
    #[test]
    fn the_marker_decides_the_season_not_the_directory() {
        let misfiled = key("Series/Elementary/Season 05/Elementary - S06E08 - Sand Trap.mkv");

        assert_eq!(misfiled.season, 6);
        assert_eq!(misfiled.episode, 8);
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

    /// A film directory that happens to be named `Series`, sitting in a real
    /// `Movies` root, and the run narrowed to that root.
    ///
    /// One root-named child is the duplication case and nothing else: the
    /// candidate is a real root, and the child below it is for `parse` to
    /// report. Reading it as evidence of a media root moves `library_root` a
    /// level in, after which the film directory reads as a `Series` library and
    /// the episode inside it earns a *destination* - `validate --fix` then
    /// builds a season directory inside a film folder.
    ///
    /// A rule of "roots the candidate is not named after" does not save this:
    /// the candidate is `Movies` and the child is `Series`, so the names differ
    /// and the search would descend anyway. The count is what separates them.
    #[test]
    fn a_film_directory_named_after_a_root_does_not_move_the_root_inwards() {
        let temp_dir = TempDir::new().unwrap();
        let movies = temp_dir.path().join("lib").join("Movies");
        fs::create_dir_all(movies.join("Series")).unwrap();
        fs::create_dir_all(movies.join("Batman Begins (2005)")).unwrap();
        fs::create_dir_all(temp_dir.path().join("lib").join("Series")).unwrap();

        let scope = scope_for(&movies);

        assert_eq!(
            scope.library_root,
            temp_dir.path().join("lib"),
            "one root-named child is a duplication to report, not a media root to descend into"
        );
        assert_eq!(scope.scan_path, movies);
    }

    /// A tree rsynced into itself *directly* under the root, and the run
    /// narrowed to that root.
    ///
    /// The nesting the earlier guard covers sits a level down, so the outer
    /// `Series` holds no root and the probe never speaks. Adjacent duplication
    /// is the shape that puts a root-named child right where the probe looks.
    #[test]
    fn a_tree_nested_directly_into_itself_resolves_to_the_outer_root() {
        let temp_dir = TempDir::new().unwrap();
        let series = temp_dir.path().join("lib").join("Series");
        fs::create_dir_all(series.join("Series").join("Veronica Mars")).unwrap();
        fs::create_dir_all(series.join("Elementary")).unwrap();

        let scope = scope_for(&series);

        assert_eq!(
            scope.library_root,
            temp_dir.path().join("lib"),
            "the inner Series is the duplication we report, not a library of its own"
        );
        assert_eq!(scope.scan_path, series);
    }

    /// The probe counts directories, because a stray *file* named `Series` is
    /// not a library root and must not be counted toward one.
    #[test]
    fn a_file_named_after_a_root_is_not_evidence_of_a_media_root() {
        let temp_dir = TempDir::new().unwrap();
        let anime = temp_dir.path().join("srv").join("Anime");
        fs::create_dir_all(anime.join("Series")).unwrap();
        fs::write(anime.join("Movies"), "").unwrap();

        let scope = scope_for(&anime);

        assert_eq!(
            scope.library_root,
            temp_dir.path().join("srv"),
            "one root directory and one root-named file is still one root directory"
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

    /// Issue #198, the four files it found, verbatim. All four were already
    /// named the way Plex reads a double episode, and were being proposed a name
    /// that says the file holds one.
    #[test]
    fn leaves_a_double_episode_that_is_already_named_correctly_alone() {
        for path in [
            "Series/Charmed/Season 04/Charmed - S04E01-E02 - Charmed Again.avi",
            "Series/Charmed/Season 05/Charmed - S05E01-E02 - A Witches Tale.avi",
            "Series/Charmed/Season 05/Charmed - S05E22-E23 - Oh My Goddess.avi",
            "Series/Elementary/Season 01/Elementary - S01E23-E24 - The Woman & Heroine.mkv",
        ] {
            assert_eq!(assess(Path::new(path)), Assessment::Canonical, "for {path}");
        }
    }

    #[test]
    fn normalises_a_double_episode_written_the_long_way() {
        assert_eq!(
            assess(Path::new(
                "Series/Charmed/Season 4/Charmed - s04e01-s04e02 - Charmed Again.avi"
            )),
            Assessment::Rename {
                destination: "Series/Charmed/Season 04/Charmed - S04E01-E02 - Charmed Again.avi"
                    .to_string()
            }
        );
    }

    #[test]
    fn refuses_a_double_episode_that_spans_two_seasons() {
        assert_eq!(
            assess(Path::new(
                "Series/Charmed/Season 04/Charmed - S04E22-S05E01 - Finale.avi"
            )),
            Assessment::Unresolvable(Unresolvable::EpisodeRangeAcrossSeasons { from: 4, to: 5 }),
            "neither season directory is the right one, so there is nothing to propose"
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
