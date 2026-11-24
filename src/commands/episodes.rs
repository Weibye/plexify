//! Report, per show and season, the episodes the library leaves out and the
//! files that will not play whole. Read-only, and judged from the files alone:
//! see D37.

use anyhow::{anyhow, Result};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::info;

use crate::ignore::{is_media_file, IgnoreFilter};
use crate::naming::{
    download_marker, parse, scope_for, sort_key, LibraryRoot, MediaName, Scope, Unresolvable,
};
use crate::paths::to_forward_slashes;
use crate::probe::{probe_all, MediaProbe};

/// Why a file is not a whole episode.
#[derive(Debug, Clone, PartialEq)]
pub enum Problem {
    /// The name carries a downloader's marker for a file it never finished.
    DownloadFragment {
        marker: String,
    },
    /// FFprobe could not read the file.
    Unreadable {
        reason: String,
    },
    NoPicture,
    NoSound,
    /// The container never recorded its length, as a mux that was cut off
    /// leaves it.
    NoDuration,
}

impl Problem {
    pub fn explain(&self) -> String {
        match self {
            Problem::DownloadFragment { marker } => {
                format!("'{marker}' marks a download that never finished")
            }
            Problem::Unreadable { reason } => format!("FFprobe cannot read it: {reason}"),
            Problem::NoPicture => "no picture".to_string(),
            Problem::NoSound => "no sound".to_string(),
            Problem::NoDuration => "no duration".to_string(),
        }
    }
}

/// What FFprobe's reading says is wrong with a file.
pub fn probe_problems(probed: Result<MediaProbe>) -> Vec<Problem> {
    let probe = match probed {
        Ok(probe) => probe,
        Err(error) => {
            // The report already names the file, and every layer of the chain
            // repeats its path; FFprobe's own words are the last layer.
            let chain = format!("{error:#}");
            let reason = chain
                .rsplit_once(": ")
                .map_or(chain.as_str(), |(_, last)| last);
            return vec![Problem::Unreadable {
                reason: reason.to_string(),
            }];
        }
    };

    let mut problems = Vec::new();
    if probe.video.is_none() {
        problems.push(Problem::NoPicture);
    }
    if probe.audio.is_empty() {
        problems.push(Problem::NoSound);
    }
    if probe.duration_seconds.is_none() {
        problems.push(Problem::NoDuration);
    }
    problems
}

/// One file a season holds.
#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeFile {
    /// Relative to the show's directory, with `/` separators.
    pub path: String,
    /// The first and last episode its name covers. The same for all but a
    /// double episode.
    pub first: u32,
    pub last: u32,
    /// Whether Plex lists it: a media container, as opposed to a `.part` or
    /// `.ytdl` a downloader left beside one.
    pub listed: bool,
    pub problems: Vec<Problem>,
}

impl EpisodeFile {
    fn covers(&self, episode: u32) -> bool {
        (self.first..=self.last).contains(&episode)
    }
}

/// The files of one season, ordered by episode.
#[derive(Debug, Clone, PartialEq)]
pub struct Season {
    /// The season the files' markers name, which is what Plex files them by.
    pub number: u32,
    pub files: Vec<EpisodeFile>,
}

impl Season {
    /// The lowest episode any file is named for.
    pub fn first(&self) -> u32 {
        self.files.iter().map(|file| file.first).min().unwrap_or(0)
    }

    /// The highest episode any file is named for.
    pub fn last(&self) -> u32 {
        self.files.iter().map(|file| file.last).max().unwrap_or(0)
    }

    /// Episodes between the first and the last that Plex lists no file for.
    ///
    /// Only between them: a season that starts at E62 is how a show numbered
    /// across seasons looks, and nothing in the files says where a season ends.
    pub fn missing(&self) -> Vec<u32> {
        (self.first()..=self.last())
            .filter(|&episode| !self.listed(episode).any(|_| true))
            .collect()
    }

    /// Episodes Plex lists more than one file for, with the files.
    pub fn shared(&self) -> Vec<(u32, Vec<&EpisodeFile>)> {
        (self.first()..=self.last())
            .map(|episode| (episode, self.listed(episode).collect::<Vec<_>>()))
            .filter(|(_, files)| files.len() > 1)
            .collect()
    }

    /// Files that will not play whole.
    pub fn broken(&self) -> impl Iterator<Item = &EpisodeFile> {
        self.files.iter().filter(|file| !file.problems.is_empty())
    }

    pub fn has_problems(&self) -> bool {
        !self.missing().is_empty() || !self.shared().is_empty() || self.broken().any(|_| true)
    }

    fn listed(&self, episode: u32) -> impl Iterator<Item = &EpisodeFile> {
        self.files
            .iter()
            .filter(move |file| file.listed && file.covers(episode))
    }
}

/// A show's seasons, as one series directory holds them.
#[derive(Debug, Clone, PartialEq)]
pub struct Show {
    /// The series directory, relative to the library root and root included:
    /// `Series/Charmed`.
    pub directory: String,
    pub seasons: Vec<Season>,
}

#[derive(Debug, Clone)]
pub struct EpisodesReport {
    pub scan_path: PathBuf,
    pub shows: Vec<Show>,
    /// Files under `Series/` or `Anime/` whose names say no whole episode,
    /// relative to the library root. `validate` names each one and why.
    pub unplaced: Vec<String>,
    pub elapsed: Duration,
}

impl EpisodesReport {
    pub fn files(&self) -> usize {
        self.seasons().map(|season| season.files.len()).sum()
    }

    pub fn seasons(&self) -> impl Iterator<Item = &Season> {
        self.shows.iter().flat_map(|show| &show.seasons)
    }
}

/// Where a file's name puts it.
#[derive(Debug, PartialEq)]
enum Placement {
    Episode {
        show: String,
        season: u32,
        first: u32,
        last: u32,
        fragment: Option<String>,
    },
    /// Under `Series/` or `Anime/`, but the name says no whole episode.
    Unplaced,
    /// A film, or outside the library roots.
    NotAnEpisode,
}

/// Read a library-relative path the way the rest of plexify does.
///
/// A name `parse` reads is placed where it says. A download fragment is placed
/// under the episode its marker names, because Plex lists it there. Any other
/// name `parse` refuses - a half episode `S01E13.5`, a tree nested into itself -
/// is not read as an episode here either, so `S01E13.5` never shows up as a
/// second E13.
fn place(relative: &str) -> Placement {
    let episodic = relative
        .split('/')
        .next()
        .and_then(LibraryRoot::from_component)
        .is_some_and(|root| root.is_episodic());
    if !episodic {
        return Placement::NotAnEpisode;
    }

    let Some(key) = sort_key(Path::new(relative)) else {
        return Placement::Unplaced;
    };
    let show = key.series_directory.join("/");

    match parse(relative) {
        Ok(MediaName::Episode(episode)) => Placement::Episode {
            show,
            season: episode.season,
            first: episode.number,
            last: episode.through.unwrap_or(episode.number),
            fragment: None,
        },
        Err(Unresolvable::DownloadFragment { marker }) => Placement::Episode {
            show,
            season: key.season,
            first: key.episode,
            last: key.episode,
            fragment: Some(marker),
        },
        _ => Placement::Unplaced,
    }
}

/// Group files into shows and seasons.
///
/// Each file is a library-relative path with what FFprobe found wrong with it,
/// or `None` when it was not probed because Plex does not list it.
fn assemble(files: Vec<(String, Option<Vec<Problem>>)>) -> (Vec<Show>, Vec<String>) {
    let mut shows: BTreeMap<String, BTreeMap<u32, Vec<EpisodeFile>>> = BTreeMap::new();
    let mut unplaced = Vec::new();

    for (relative, probed) in files {
        let (show, season, first, last, fragment) = match place(&relative) {
            Placement::Episode {
                show,
                season,
                first,
                last,
                fragment,
            } => (show, season, first, last, fragment),
            Placement::Unplaced => {
                unplaced.push(relative);
                continue;
            }
            Placement::NotAnEpisode => continue,
        };

        let path = relative
            .strip_prefix(&format!("{show}/"))
            .unwrap_or(&relative)
            .to_string();
        let listed = probed.is_some();
        let problems = fragment
            .map(|marker| Problem::DownloadFragment { marker })
            .into_iter()
            .chain(probed.into_iter().flatten())
            .collect();

        shows
            .entry(show)
            .or_default()
            .entry(season)
            .or_default()
            .push(EpisodeFile {
                path,
                first,
                last,
                listed,
                problems,
            });
    }

    let shows = shows
        .into_iter()
        .map(|(directory, seasons)| Show {
            directory,
            seasons: seasons
                .into_iter()
                .map(|(number, mut files)| {
                    files.sort_by(|a, b| (a.first, &a.path).cmp(&(b.first, &b.path)));
                    Season { number, files }
                })
                .collect(),
        })
        .collect();
    unplaced.sort();

    (shows, unplaced)
}

/// The files a check of episodes looks at: media containers, and the debris a
/// downloader leaves beside them.
fn media_or_download_fragment(path: &Path) -> bool {
    is_media_file(path)
        || path
            .file_name()
            .is_some_and(|name| download_marker(&name.to_string_lossy()).is_some())
}

pub struct EpisodesCommand {
    scope: Scope,
}

impl EpisodesCommand {
    pub fn new(path: PathBuf) -> Self {
        let absolute = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&path))
                .unwrap_or(path)
        };
        Self {
            scope: scope_for(&absolute),
        }
    }

    pub async fn execute(&self) -> Result<EpisodesReport> {
        let started = Instant::now();
        let scan_path = &self.scope.scan_path;
        if !scan_path.is_dir() {
            return Err(anyhow!(
                "Media directory does not exist: {}",
                to_forward_slashes(scan_path)
            ));
        }
        info!("🔍 Checking episodes in {}", to_forward_slashes(scan_path));

        let filter = IgnoreFilter::for_scope(self.scope.library_root.clone(), scan_path)?;
        let mut relative = Vec::new();
        let mut listed = Vec::new();
        for path in filter.files_where(scan_path, media_or_download_fragment) {
            let Ok(within) = path.strip_prefix(&self.scope.library_root) else {
                continue;
            };
            let within = to_forward_slashes(within);
            if place(&within) == Placement::NotAnEpisode {
                continue;
            }
            if is_media_file(&path) {
                listed.push(path);
            }
            relative.push(within);
        }

        info!(
            "🔍 Found {} files under Series/ and Anime/, probing {}...",
            relative.len(),
            listed.len()
        );
        let probed = probe_all(listed.clone(), |_, probed| probe_problems(probed)).await?;
        let mut problems: BTreeMap<String, Vec<Problem>> = listed
            .iter()
            .map(|path| {
                let within = path.strip_prefix(&self.scope.library_root).unwrap_or(path);
                to_forward_slashes(within)
            })
            .zip(probed)
            .collect();

        let files = relative
            .into_iter()
            .map(|path| {
                let probed = problems.remove(&path);
                (path, probed)
            })
            .collect();
        let (shows, unplaced) = assemble(files);

        Ok(EpisodesReport {
            scan_path: scan_path.clone(),
            shows,
            unplaced,
            elapsed: started.elapsed(),
        })
    }
}

fn files(count: usize) -> String {
    match count {
        1 => "1 file".to_string(),
        count => format!("{count} files"),
    }
}

/// `E30`, `E40–E43`: consecutive episodes collapsed into a range.
fn episode_ranges(episodes: &[u32]) -> String {
    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for &episode in episodes {
        match ranges.last_mut() {
            Some((_, end)) if *end + 1 == episode => *end = episode,
            _ => ranges.push((episode, episode)),
        }
    }
    ranges
        .iter()
        .map(|&(start, end)| {
            if start == end {
                format!("E{start:02}")
            } else {
                format!("E{start:02}–E{end:02}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Render the report. Every season is listed, a clean one included, because
/// the line saying which episodes cannot be seen is the only hint that a
/// season ends early.
pub fn render(report: &EpisodesReport) -> String {
    let mut out = String::new();
    let troubled = report.seasons().filter(|s| s.has_problems()).count();

    let _ = writeln!(out, "\n📺 Episode Report");
    let _ = writeln!(out, "═════════════════");
    let _ = writeln!(
        out,
        "📂 Scanned directory: {}",
        to_forward_slashes(&report.scan_path)
    );
    let _ = writeln!(
        out,
        "📁 Shows: {}, seasons: {}, episode files: {}",
        report.shows.len(),
        report.seasons().count(),
        report.files()
    );
    let _ = writeln!(out, "❌ Seasons with problems: {troubled}");
    if !report.unplaced.is_empty() {
        let _ = writeln!(
            out,
            "🤔 Not counted, because the name says no whole episode: {}; `plexify validate` names each one",
            files(report.unplaced.len())
        );
    }
    let _ = writeln!(out, "⏱️  Check time: {:.2}s", report.elapsed.as_secs_f64());

    if report.shows.is_empty() {
        let _ = writeln!(out, "\nNo episode files under Series/ or Anime/.");
        return out;
    }

    for show in &report.shows {
        let _ = writeln!(out, "\n{}", show.directory);
        for season in &show.seasons {
            render_season(&mut out, season);
        }
    }

    out
}

fn render_season(out: &mut String, season: &Season) {
    let name = match season.number {
        0 => "Specials".to_string(),
        number => format!("Season {number:02}"),
    };
    let (first, last) = (season.first(), season.last());
    let span = if first == last {
        format!("E{first:02}")
    } else {
        format!("E{first:02}–E{last:02}")
    };
    let mark = if season.has_problems() { "❌" } else { "✅" };
    let _ = writeln!(
        out,
        "  {mark} {name}: {span} in {}",
        files(season.files.len())
    );

    // What the files cannot say, on every season: a season that looks complete
    // up to its last file may run on past it.
    let unseen = if first <= 1 {
        format!("after E{last:02}")
    } else if first == last {
        format!("before or after E{first:02}")
    } else {
        format!("before E{first:02} or after E{last:02}")
    };
    let _ = writeln!(out, "     Episodes {unseen} cannot be seen from the files.");

    let missing = season.missing();
    if !missing.is_empty() {
        let _ = writeln!(out, "     Missing: {}", episode_ranges(&missing));
    }

    for (episode, files) in season.shared() {
        let _ = writeln!(
            out,
            "     E{episode:02} is in {} files, and Plex lists each one as E{episode:02}:",
            files.len()
        );
        for file in files {
            let _ = writeln!(out, "       {}", file.path);
        }
    }

    let broken: Vec<_> = season.broken().collect();
    if !broken.is_empty() {
        let _ = writeln!(out, "     Will not play whole:");
        for file in broken {
            let _ = writeln!(out, "       {}", file.path);
            let problems: Vec<_> = file.problems.iter().map(Problem::explain).collect();
            let _ = writeln!(out, "         {}", problems.join("; "));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{AudioStream, VideoStream};
    use crate::test_support::ffmpeg_present;
    use std::fs;
    use std::process::{Command, Stdio};
    use tempfile::TempDir;

    const FFX: &str = "Series/Super Best Friends Play - FFX";

    fn ffx(name: &str) -> String {
        format!("{FFX}/Super Best Friends Play - Final Fantasy X - {name}")
    }

    /// A file Plex lists, with nothing wrong inside it.
    fn whole(path: String) -> (String, Option<Vec<Problem>>) {
        (path, Some(Vec::new()))
    }

    fn only_season(files: Vec<(String, Option<Vec<Problem>>)>) -> Season {
        let (shows, _) = assemble(files);
        assert_eq!(shows.len(), 1, "{shows:#?}");
        assert_eq!(shows[0].seasons.len(), 1, "{shows:#?}");
        shows[0].seasons[0].clone()
    }

    #[test]
    fn gaps_between_the_first_and_last_episode_are_missing() {
        let season = only_season(
            (1..=44)
                .filter(|e| ![30, 40, 41, 42, 43].contains(e))
                .map(|e| whole(ffx(&format!("S01E{e:02} (720p30).webm"))))
                .collect(),
        );

        assert_eq!(season.missing(), vec![30, 40, 41, 42, 43]);
        assert_eq!(episode_ranges(&season.missing()), "E30, E40–E43");
    }

    /// One Piece numbers across seasons: its second season starts at E62.
    #[test]
    fn a_season_numbered_from_where_the_last_ended_is_not_missing_its_start() {
        let season = only_season(
            (62..=64)
                .map(|e| {
                    whole(format!(
                        "Anime/One Piece/Season 02/One Piece - S02E{e} - X.mkv"
                    ))
                })
                .collect(),
        );

        assert!(season.missing().is_empty());
        assert!(!season.has_problems());
    }

    #[test]
    fn a_double_episode_holds_both_of_its_numbers() {
        let season = only_season(vec![
            whole("Series/Charmed/Season 04/Charmed - S04E01-E02 - Charmed Again.avi".into()),
            whole("Series/Charmed/Season 04/Charmed - S04E03 - Hell Hath No Fury.avi".into()),
        ]);

        assert!(season.missing().is_empty());
        assert!(season.shared().is_empty());
    }

    /// E39 of the FFX show: a picture, a sound and an unfinished merge, all
    /// listed by Plex as E39.
    #[test]
    fn an_episode_in_three_pieces_names_each_piece_and_why() {
        let season = only_season(vec![
            whole(ffx("S01E38 (720p30).webm")),
            (
                ffx("S01E39 (720p30).f247.webm"),
                Some(vec![Problem::NoSound]),
            ),
            (
                ffx("S01E39 (720p30).f251.webm"),
                Some(vec![Problem::NoPicture]),
            ),
            (
                ffx("S01E39 (720p30).temp.webm"),
                Some(vec![Problem::NoDuration]),
            ),
        ]);

        let shared = season.shared();
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].0, 39);
        assert_eq!(shared[0].1.len(), 3);
        assert!(season.missing().is_empty());

        let broken: Vec<_> = season.broken().map(|f| f.problems.clone()).collect();
        assert_eq!(
            broken,
            vec![
                vec![
                    Problem::DownloadFragment {
                        marker: ".f247".into()
                    },
                    Problem::NoSound
                ],
                vec![
                    Problem::DownloadFragment {
                        marker: ".f251".into()
                    },
                    Problem::NoPicture
                ],
                vec![
                    Problem::DownloadFragment {
                        marker: ".temp".into()
                    },
                    Problem::NoDuration
                ],
            ]
        );
    }

    /// A `.part` is a fragment Plex does not list, so its episode is still
    /// missing - and the fragment is named beside it.
    #[test]
    fn an_episode_with_only_a_part_file_is_missing_and_the_part_is_named() {
        let season = only_season(vec![
            whole(ffx("S01E39 (720p30).webm")),
            (ffx("S01E40 (720p30).f247.webm.part"), None),
            whole(ffx("S01E41 (720p30).webm")),
        ]);

        assert_eq!(season.missing(), vec![40]);
        let broken: Vec<_> = season.broken().map(|f| f.path.clone()).collect();
        assert_eq!(
            broken,
            vec!["Super Best Friends Play - Final Fantasy X - S01E40 (720p30).f247.webm.part"]
        );
    }

    #[test]
    fn a_half_episode_is_not_a_second_copy_of_the_whole_one() {
        let (shows, unplaced) = assemble(vec![
            whole(
                "Anime/Shingeki no Kyojin/Season 01/Shingeki no Kyojin - S01E13 - [1080p].mkv"
                    .into(),
            ),
            whole(
                "Anime/Shingeki no Kyojin/Season 01/Shingeki no Kyojin - S01E13.5 - [1080p].mkv"
                    .into(),
            ),
        ]);

        assert!(shows[0].seasons[0].shared().is_empty());
        assert_eq!(
            unplaced,
            vec!["Anime/Shingeki no Kyojin/Season 01/Shingeki no Kyojin - S01E13.5 - [1080p].mkv"]
        );
    }

    #[test]
    fn films_are_not_episodes() {
        let (shows, unplaced) = assemble(vec![whole(
            "Movies/The Dark Knight (2008)/The Dark Knight (2008).mkv".into(),
        )]);
        assert!(shows.is_empty());
        assert!(unplaced.is_empty());
    }

    #[test]
    fn a_file_with_only_a_picture_has_no_sound() {
        let picture_only = MediaProbe {
            duration_seconds: Some(1.0),
            video: Some(VideoStream::default()),
            ..MediaProbe::default()
        };
        assert_eq!(probe_problems(Ok(picture_only)), vec![Problem::NoSound]);

        let whole = MediaProbe {
            duration_seconds: Some(1.0),
            video: Some(VideoStream::default()),
            audio: vec![AudioStream::default()],
            ..MediaProbe::default()
        };
        assert!(probe_problems(Ok(whole)).is_empty());
    }

    #[test]
    fn every_season_says_what_its_files_cannot_show() {
        let report = EpisodesReport {
            scan_path: PathBuf::from("/media"),
            shows: assemble(vec![
                whole("Series/Charmed/Season 06/Charmed - S06E01 - X.avi".into()),
                whole("Series/Charmed/Season 06/Charmed - S06E02 - Y.avi".into()),
                whole("Anime/One Piece/Season 02/One Piece - S02E62 - Z.mkv".into()),
            ])
            .0,
            unplaced: Vec::new(),
            elapsed: Duration::ZERO,
        };

        let rendered = render(&report);
        assert!(
            rendered.contains("✅ Season 06: E01–E02 in 2 files\n     Episodes after E02 cannot be seen from the files."),
            "{rendered}"
        );
        assert!(
            rendered.contains("Episodes before or after E62 cannot be seen from the files."),
            "{rendered}"
        );
    }

    /// Encode a second of picture and sound into `path`, written through a pipe
    /// when `unfinished`, so the muxer cannot go back and record the duration -
    /// what a merge cut off part way leaves behind.
    fn encode(path: &Path, maps: &[&str], unfinished: bool) {
        let mut command = Command::new("ffmpeg");
        command
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=160x120:rate=10",
            ])
            .args(["-f", "lavfi", "-i", "sine=duration=1"])
            .args(maps)
            .args(["-c:v", "libvpx", "-c:a", "libopus", "-f", "webm"])
            .stderr(Stdio::null());
        if unfinished {
            command.arg("-").stdout(fs::File::create(path).unwrap());
        } else {
            command.arg("-y").arg(path).stdout(Stdio::null());
        }
        assert!(
            command.status().unwrap().success(),
            "could not build {path:?}"
        );
    }

    /// The FFX show as it was found: E17 and E34 without sound, E39 in three
    /// pieces, E30 and E40-E43 missing. Probed for real.
    #[tokio::test]
    async fn the_ffx_show_is_reported_as_it_was_found() {
        if !ffmpeg_present() {
            return;
        }
        let library = TempDir::new().unwrap();
        let show = library.path().join(FFX);
        fs::create_dir_all(&show).unwrap();
        let file = |name: &str| {
            show.join(format!(
                "Super Best Friends Play - Final Fantasy X - {name}"
            ))
        };
        let both = ["-map", "0:v", "-map", "1:a"];

        for episode in [29, 31, 44] {
            encode(
                &file(&format!("S01E{episode:02} (720p30).webm")),
                &both,
                false,
            );
        }
        encode(&file("S01E17 (1080p60).f303.webm"), &["-map", "0:v"], false);
        encode(&file("S01E34 (720p30).f247.webm"), &["-map", "0:v"], false);
        encode(&file("S01E39 (720p30).f247.webm"), &["-map", "0:v"], false);
        encode(&file("S01E39 (720p30).f251.webm"), &["-map", "1:a"], false);
        encode(&file("S01E39 (720p30).temp.webm"), &both, true);
        // Plex does not list a `.part`, so it is never probed, but it is named.
        fs::write(file("S01E40 (720p30).f247.webm.part"), b"").unwrap();
        // A canonical name says nothing about what is inside: the probe has to.
        encode(&file("S01E33 (720p30).webm"), &["-map", "0:v"], false);

        let report = EpisodesCommand::new(library.path().join("Series"))
            .execute()
            .await
            .unwrap();
        let season = &report.shows[0].seasons[0];

        assert_eq!(
            episode_ranges(&season.missing()),
            "E18–E28, E30, E32, E35–E38, E40–E43"
        );
        let problems = |name: &str| {
            season
                .files
                .iter()
                .find(|f| f.path.ends_with(name))
                .map(|f| f.problems.clone())
                .unwrap()
        };
        assert_eq!(
            problems("S01E17 (1080p60).f303.webm"),
            vec![
                Problem::DownloadFragment {
                    marker: ".f303".into()
                },
                Problem::NoSound
            ]
        );
        assert_eq!(problems("S01E33 (720p30).webm"), vec![Problem::NoSound]);
        assert_eq!(
            problems("S01E39 (720p30).f251.webm"),
            vec![
                Problem::DownloadFragment {
                    marker: ".f251".into()
                },
                Problem::NoPicture
            ]
        );
        // Measured on FFmpeg 9.0: a WebM muxed to a pipe records no duration.
        assert_eq!(
            problems("S01E39 (720p30).temp.webm"),
            vec![
                Problem::DownloadFragment {
                    marker: ".temp".into()
                },
                Problem::NoDuration
            ]
        );
        assert!(problems("S01E44 (720p30).webm").is_empty());
        assert_eq!(
            problems("S01E40 (720p30).f247.webm.part"),
            vec![Problem::DownloadFragment {
                marker: ".part".into()
            }]
        );
        assert_eq!(season.shared()[0].0, 39);
    }
}
