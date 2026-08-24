//! Rendering parsed fields back into the one canonical form.
//!
//! This is the only place that decides what a path in this library looks like:
//!
//! ```text
//! {Root}/{Series Directory}/Season {NN}/{Series} - S{NN}E{NN} - {Title} [{quality}].{ext}
//! {Root}/{Film Directory}/{Title} ({Year}) [{quality}].{ext}
//! ```
//!
//! Optional fields disappear cleanly rather than leaving their separators
//! behind: an episode with no title renders `Scrubs - S09E02.avi`, not
//! `Scrubs - S09E02 - .avi`. That matters because the output of this function is
//! parsed again to decide whether a file is already correct, so anything it
//! emits has to be something the parser reads back unchanged.

use super::{Episode, MediaName, Movie, SeasonDirectory};

/// Render a parsed name into its canonical path, relative to the media root.
pub fn render(name: &MediaName) -> String {
    match name {
        MediaName::Episode(episode) => render_episode(episode),
        MediaName::Movie(movie) => render_movie(movie),
    }
}

fn render_episode(episode: &Episode) -> String {
    let mut components = vec![episode.root.as_str().to_string()];
    components.extend(episode.directories.iter().cloned());

    // The season *number* comes from the episode's own marker, not from the
    // directory it happened to be in. That is what moves a loose file into a
    // season directory and a misfiled one across.
    //
    // An arc name on that directory is kept, though - `Season 02 - The Mighty
    // Nein`. Plex takes the season from the marker in the filename, so the arc
    // name costs it nothing, and it is information somebody curated: One Piece
    // arcs here carry their episode ranges. It survives only where the directory
    // already agrees with the marker; a file being moved to another season
    // cannot bring the old season's arc name with it.
    components.push(match &episode.season_directory {
        // Season zero is `Specials` and nothing else. Letting an arc name through
        // here would give it a second canonical home, and a file could then sit
        // correctly in either.
        SeasonDirectory::Numbered { number, suffix }
            if episode.season > 0 && *number == episode.season && !suffix.is_empty() =>
        {
            format!("Season {:02}{}", episode.season, suffix)
        }
        _ => season_directory_for(episode.season),
    });
    components.extend(episode.nested_directories.iter().cloned());

    let mut filename = format!(
        "{} - S{:02}E{:02}",
        episode.series, episode.season, episode.number
    );
    if let Some(title) = &episode.title {
        filename.push_str(" - ");
        filename.push_str(title);
    }
    if let Some(quality) = &episode.quality {
        filename.push_str(&format!(" [{quality}]"));
    }
    filename.push('.');
    filename.push_str(&episode.extension);

    components.push(filename);
    components.join("/")
}

/// The directory an episode of this season belongs in.
///
/// Season zero is `Specials` rather than `Season 00`. Plex reads both, and this
/// is the one a person reads at a glance.
fn season_directory_for(season: u32) -> String {
    if season == 0 {
        "Specials".to_string()
    } else {
        format!("Season {season:02}")
    }
}

/// A film: `{Title} ({Year}) [{quality}].{ext}`.
///
/// The quality is optional and disappears with its brackets, the same way an
/// episode's does - a film keeps it for the same reason, that it is information
/// the library holds.
fn render_movie(movie: &Movie) -> String {
    let mut components = vec![movie.root.as_str().to_string()];
    components.extend(movie.directories.iter().cloned());

    let mut filename = format!("{} ({})", movie.title, movie.year);
    if let Some(quality) = &movie.quality {
        filename.push_str(&format!(" [{quality}]"));
    }
    filename.push('.');
    filename.push_str(&movie.extension);

    components.push(filename);
    components.join("/")
}

#[cfg(test)]
mod tests {
    use super::super::{parse, LibraryRoot, SeasonDirectory};
    use super::*;

    fn episode() -> Episode {
        Episode {
            root: LibraryRoot::Series,
            directories: vec!["Elementary".to_string()],
            season_directory: SeasonDirectory::Numbered {
                number: 6,
                suffix: String::new(),
            },
            nested_directories: Vec::new(),
            series: "Elementary".to_string(),
            season: 6,
            number: 8,
            title: Some("Sand Trap".to_string()),
            quality: None,
            extension: "mkv".to_string(),
        }
    }

    #[test]
    fn pads_the_season_in_both_the_directory_and_the_marker() {
        assert_eq!(
            render(&MediaName::Episode(episode())),
            "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv"
        );
    }

    #[test]
    fn omits_a_missing_title_along_with_its_separator() {
        let without_title = Episode {
            title: None,
            ..episode()
        };

        assert_eq!(
            render(&MediaName::Episode(without_title)),
            "Series/Elementary/Season 06/Elementary - S06E08.mkv"
        );
    }

    #[test]
    fn puts_quality_in_brackets_after_the_title() {
        let with_quality = Episode {
            quality: Some("1080p60".to_string()),
            ..episode()
        };

        assert_eq!(
            render(&MediaName::Episode(with_quality)),
            "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap [1080p60].mkv"
        );
    }

    #[test]
    fn puts_a_loose_episode_into_the_season_its_marker_names() {
        let no_season_directory = Episode {
            season_directory: SeasonDirectory::Absent,
            ..episode()
        };

        assert_eq!(
            render(&MediaName::Episode(no_season_directory)),
            "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv",
            "a file with no season directory belongs in the one its marker names"
        );
    }

    #[test]
    fn moves_a_misfiled_episode_to_the_season_its_marker_names() {
        let misfiled = Episode {
            season_directory: SeasonDirectory::Numbered {
                number: 2,
                suffix: String::new(),
            },
            ..episode()
        };

        assert_eq!(
            render(&MediaName::Episode(misfiled)),
            "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv",
            "the marker says season six; the directory it sat in does not decide"
        );
    }

    #[test]
    fn season_zero_is_the_specials_directory() {
        let special = Episode {
            season_directory: SeasonDirectory::Absent,
            season: 0,
            number: 1,
            ..episode()
        };

        assert_eq!(
            render(&MediaName::Episode(special)),
            "Series/Elementary/Specials/Elementary - S00E01 - Sand Trap.mkv"
        );
    }

    #[test]
    fn keeps_a_directory_nested_under_the_season_directory() {
        let nested = Episode {
            nested_directories: vec!["Extras".to_string()],
            ..episode()
        };

        assert_eq!(
            render(&MediaName::Episode(nested)),
            "Series/Elementary/Season 06/Extras/Elementary - S06E08 - Sand Trap.mkv",
            "the season directory is corrected in place, not added underneath"
        );
    }

    #[test]
    fn keeps_an_arc_name_the_season_directory_already_carries() {
        let with_arc = Episode {
            season_directory: SeasonDirectory::Numbered {
                number: 6,
                suffix: " - The Long Night".to_string(),
            },
            ..episode()
        };

        assert_eq!(
            render(&MediaName::Episode(with_arc)),
            "Series/Elementary/Season 06 - The Long Night/Elementary - S06E08 - Sand Trap.mkv",
            "Plex reads the season from the marker, so the arc name costs nothing and is information somebody curated"
        );
    }

    #[test]
    fn pads_a_season_directory_while_keeping_its_arc_name() {
        let with_arc = Episode {
            season_directory: SeasonDirectory::Numbered {
                number: 6,
                suffix: " - The Long Night".to_string(),
            },
            season: 6,
            ..episode()
        };

        assert!(render(&MediaName::Episode(with_arc)).contains("/Season 06 - The Long Night/"));
    }

    #[test]
    fn does_not_carry_an_arc_name_into_another_season() {
        let misfiled = Episode {
            season_directory: SeasonDirectory::Numbered {
                number: 2,
                suffix: " - The Long Night".to_string(),
            },
            season: 6,
            ..episode()
        };

        assert_eq!(
            render(&MediaName::Episode(misfiled)),
            "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv",
            "season two's arc name does not describe season six"
        );
    }

    #[test]
    fn renders_a_movie_with_its_year() {
        let movie = Movie {
            root: LibraryRoot::Movies,
            directories: vec!["Marvel Cinematic Universe Collection".to_string()],
            title: "Iron Man".to_string(),
            year: 2008,
            quality: None,
            extension: "mkv".to_string(),
        };

        assert_eq!(
            render(&MediaName::Movie(movie)),
            "Movies/Marvel Cinematic Universe Collection/Iron Man (2008).mkv"
        );
    }

    /// Rendering has to produce something the parser reads back identically, or
    /// a file would be renamed a second time on the next run.
    #[test]
    fn what_is_rendered_parses_back_to_the_same_fields() {
        let cases = [
            MediaName::Episode(episode()),
            MediaName::Episode(Episode {
                title: None,
                quality: Some("720p".to_string()),
                ..episode()
            }),
            MediaName::Episode(Episode {
                season_directory: SeasonDirectory::Specials,
                season: 0,
                ..episode()
            }),
            MediaName::Episode(Episode {
                season_directory: SeasonDirectory::Absent,
                ..episode()
            }),
            MediaName::Episode(Episode {
                nested_directories: vec!["Extras".to_string()],
                ..episode()
            }),
        ];

        for name in cases {
            let rendered = render(&name);
            let reparsed = parse(&rendered).expect("rendered output must parse");

            assert_eq!(render(&reparsed), rendered, "for {rendered}");
        }
    }
}
