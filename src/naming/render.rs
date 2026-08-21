//! Rendering parsed fields back into the one canonical form.
//!
//! This is the only place that decides what a path in this library looks like:
//!
//! ```text
//! {Root}/{Series Directory}/Season {NN}/{Series} - S{NN}E{NN} - {Title} [{quality}].{ext}
//! {Root}/{Film Directory}/{Title} ({Year}).{ext}
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

    match &episode.season_directory {
        SeasonDirectory::Numbered { number, suffix } => {
            components.push(format!("Season {number:02}{suffix}"));
        }
        SeasonDirectory::Specials => components.push("Specials".to_string()),
        SeasonDirectory::Absent => {}
    }

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

fn render_movie(movie: &Movie) -> String {
    let mut components = vec![movie.root.as_str().to_string()];
    components.extend(movie.directories.iter().cloned());
    components.push(format!(
        "{} ({}).{}",
        movie.title, movie.year, movie.extension
    ));

    components.join("/")
}

#[cfg(test)]
mod tests {
    use super::super::{parse, LibraryRoot};
    use super::*;

    fn episode() -> Episode {
        Episode {
            root: LibraryRoot::Series,
            directories: vec!["Elementary".to_string()],
            season_directory: SeasonDirectory::Numbered {
                number: 6,
                suffix: String::new(),
            },
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
    fn leaves_out_the_season_directory_when_there_was_none() {
        let no_season_directory = Episode {
            season_directory: SeasonDirectory::Absent,
            ..episode()
        };

        assert_eq!(
            render(&MediaName::Episode(no_season_directory)),
            "Series/Elementary/Elementary - S06E08 - Sand Trap.mkv"
        );
    }

    #[test]
    fn keeps_the_arc_name_on_a_season_directory() {
        let with_arc = Episode {
            season_directory: SeasonDirectory::Numbered {
                number: 6,
                suffix: " - The Long Night".to_string(),
            },
            ..episode()
        };

        assert_eq!(
            render(&MediaName::Episode(with_arc)),
            "Series/Elementary/Season 06 - The Long Night/Elementary - S06E08 - Sand Trap.mkv"
        );
    }

    #[test]
    fn renders_a_movie_with_its_year() {
        let movie = Movie {
            root: LibraryRoot::Movies,
            directories: vec!["Marvel Cinematic Universe Collection".to_string()],
            title: "Iron Man".to_string(),
            year: 2008,
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
        ];

        for name in cases {
            let rendered = render(&name);
            let reparsed = parse(&rendered).expect("rendered output must parse");

            assert_eq!(render(&reparsed), rendered, "for {rendered}");
        }
    }
}
