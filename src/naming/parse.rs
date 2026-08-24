//! Decomposing a library path into the fields it is made of.
//!
//! Everything here is recovery from names that were written by other tools, so
//! the rules are heuristics with one hard constraint: a heuristic may drop
//! something it is confident is noise, and it may leave a field empty, but it
//! must never invent a value. When a field cannot be recovered the parse fails
//! with a reason instead of guessing.

use std::sync::OnceLock;

use regex::Regex;

use super::{Episode, LibraryRoot, MediaName, Movie, SeasonDirectory, Unresolvable};

/// Tokens that can only describe how a file was produced.
///
/// A source, codec or container name is never a word an episode is titled with,
/// so finding one is what identifies a name as a scene release at all.
///
/// Matching is case-insensitive and by whole token, which cuts both ways: `web`
/// does not touch "Cobweb", but it would take the whole word "Web" - which is
/// why words that double as English live in [`AMBIGUOUS_RELEASE_TOKENS`] instead.
const RELEASE_TOKENS: &[&str] = &[
    "dvdrip", "dvdscr", "bdrip", "brrip", "bluray", "webrip", "webdl", "hdtv", "pdtv", "hdrip",
    "xvid", "divx", "x264", "x265", "h264", "h265", "hevc", "avc", "aac", "eac3", "ac3", "dts",
    "truehd", "flac", "mp3", "10bit", "8bit", "hi10p", "remux", "webdl", "web-dl",
];

/// Release tokens that are also ordinary words.
///
/// "The Web", "Extended Family" and "Proper Villains" are all plausible episode
/// titles, so these are only dropped from a name that already carried one of the
/// unmistakable tokens above. In isolation they are treated as part of the title.
const AMBIGUOUS_RELEASE_TOKENS: &[&str] = &[
    "web",
    "opus",
    "atmos",
    "retail",
    "proper",
    "repack",
    "internal",
    "limited",
    "remastered",
    "uncut",
    "extended",
    "dubbed",
    "subbed",
];

/// `S01E02`, `s1e2`, `S01.E02` - the marker that makes a file an episode.
fn episode_marker() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // The trailing group catches `S01E13.5`, a half episode. It is captured
    // rather than ignored so the parser can refuse it instead of reading `E13`
    // and quietly putting it on top of the real one.
    RE.get_or_init(|| Regex::new(r"(?i)\bs(\d{1,3})[\s._-]*e(\d{1,3})(\.\d{1,2})?\b").unwrap())
}

/// `Season 6`, `season 06 - The Arc Name`.
fn season_directory() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^season[\s._-]*(\d{1,3})(.*)$").unwrap())
}

/// Resolution, optionally with a frame rate: `1080p`, `720p60`, `2160p`, `4K`.
fn quality_marker() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b(\d{3,4}p(?:\d{2,3})?|4k)\b").unwrap())
}

/// A square-bracketed group: `[1080p]`, `[HorribleSubs]`, `[73241537]`.
///
/// Square brackets only. Parentheses carry parts of a name - `(Part 1)`,
/// `(Directors Cut)` - and dropping those took an edition off a film, which then
/// collided with the plain cut of the same film and left both unfixable.
fn bracketed_group() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[[^\]]*\]").unwrap())
}

/// A four digit year in parentheses, which is how a film's year is written.
///
/// Preferred over a bare number anywhere in the name, so that `Blade Runner 2049
/// (2017)` is a 2017 film called Blade Runner 2049 rather than a 2049 film
/// called Blade Runner.
fn parenthesised_year() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\((19\d{2}|20\d{2})\)").unwrap())
}

/// A four digit year, as a whole token.
fn year_marker() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b(19\d{2}|20\d{2})\b").unwrap())
}

/// Trailing `(2001)` and `{tvdb-81189}` on a series directory name, in any
/// combination - `Breaking Bad (2008) {tvdb-81189}` carries both.
fn directory_annotations() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:\s*(?:\(\d{4}\)|\{[a-z]+-\d+\}))+\s*$").unwrap())
}

/// Parse a library-relative path, given with `/` separators.
pub fn parse(path: &str) -> Result<MediaName, Unresolvable> {
    let components: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();

    let (root_name, below_root) = components
        .split_first()
        .ok_or(Unresolvable::NotAMediaFile)?;
    let root = LibraryRoot::from_component(root_name).ok_or(Unresolvable::OutsideLibrary)?;

    // A root name appearing again below the root means the tree was nested into
    // itself. Which copy is the real one is not ours to decide.
    if let Some(duplicate) = below_root
        .iter()
        .find_map(|component| LibraryRoot::from_component(component))
    {
        return Err(Unresolvable::DuplicatedRoot { root: duplicate });
    }

    let (filename, directories) = below_root.split_last().ok_or(Unresolvable::NotAMediaFile)?;
    let (stem, extension) = split_extension(filename)?;

    if root.is_episodic() {
        parse_episode(root, directories, stem, extension)
    } else {
        parse_movie(root, directories, stem, extension)
    }
}

/// Split `Pilot.mkv` into `("Pilot", "mkv")`.
fn split_extension(filename: &str) -> Result<(&str, &str), Unresolvable> {
    match filename.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            Ok((stem, extension))
        }
        _ => Err(Unresolvable::NotAMediaFile),
    }
}

fn parse_episode(
    root: LibraryRoot,
    directories: &[&str],
    stem: &str,
    extension: &str,
) -> Result<MediaName, Unresolvable> {
    let marker = episode_marker()
        .captures(stem)
        .ok_or(Unresolvable::NoEpisodeMarker)?;
    let whole_marker = marker.get(0).expect("group 0 always exists");

    // `S01E13.5` is a half episode - a recap or a special that sits between two
    // numbered ones. There is no canonical form for it, and rounding it to E13
    // would put it on top of the real E13.
    if marker.get(3).is_some() {
        return Err(Unresolvable::FractionalEpisode);
    }

    let season = marker[1]
        .parse()
        .map_err(|_| Unresolvable::NoEpisodeMarker)?;
    let number = marker[2]
        .parse()
        .map_err(|_| Unresolvable::NoEpisodeMarker)?;

    // Find the season directory anywhere in the chain rather than only at the
    // end: a file in `Season 01/Extras` is still in season one, and treating
    // `Extras` as "no season directory" would nest a second one inside it.
    let season_position = directories
        .iter()
        .rposition(|component| parse_season_directory(component).is_some());

    let (season_directory, directories, nested_directories) = match season_position {
        Some(position) => (
            parse_season_directory(directories[position]).expect("just matched"),
            &directories[..position],
            &directories[position + 1..],
        ),
        None => (SeasonDirectory::Absent, directories, &directories[..0]),
    };

    // The name in the file wins over the name of the directory holding it: a
    // directory may be an abbreviation of the series, or plain wrong, and
    // rewriting a file to match a directory would spread that.
    let mut series = clean_name(&stem[..whole_marker.start()]);
    if series.is_empty() {
        series = directories
            .last()
            .map(|directory| clean_name(&directory_annotations().replace(directory, "")))
            .unwrap_or_default();
    }
    if series.is_empty() {
        return Err(Unresolvable::NoSeriesName);
    }

    let (title, quality) = parse_title_and_quality(&stem[whole_marker.end()..]);

    Ok(MediaName::Episode(Episode {
        root,
        directories: directories.iter().map(|part| part.to_string()).collect(),
        season_directory,
        nested_directories: nested_directories
            .iter()
            .map(|part| part.to_string())
            .collect(),
        series,
        season,
        number,
        title,
        quality,
        extension: extension.to_string(),
    }))
}

fn parse_season_directory(component: &str) -> Option<SeasonDirectory> {
    if component.eq_ignore_ascii_case("specials") || component.eq_ignore_ascii_case("special") {
        return Some(SeasonDirectory::Specials);
    }

    let captures = season_directory().captures(component)?;
    Some(SeasonDirectory::Numbered {
        number: captures[1].parse().ok()?,
        suffix: captures[2].to_string(),
    })
}

fn parse_movie(
    root: LibraryRoot,
    directories: &[&str],
    stem: &str,
    extension: &str,
) -> Result<MediaName, Unresolvable> {
    // Quality is read before anything is cut away, since it usually lives in one
    // of the bracketed groups that are about to go.
    let quality = quality_marker()
        .find(stem)
        .map(|found| found.as_str().to_lowercase());

    // A film's own name carries its year; the directory usually repeats it. When
    // both say so and they disagree, one of them is wrong about which film this
    // is, and nothing in the path says which.
    let from_file = parenthesised_year()
        .find(stem)
        .or_else(|| year_marker().find(stem))
        .and_then(|found| {
            found
                .as_str()
                .trim_matches(|c| c == '(' || c == ')')
                .parse::<u32>()
                .ok()
                .map(|year| (year, found))
        });

    // Only a year the directory states the way a film directory states one. A
    // collection directory such as `Marvel 2012 Collection` is naming something
    // else, and reading 2012 out of it would put every film in it in conflict.
    let from_directory = directories
        .last()
        .and_then(|directory| parenthesised_year().find(directory))
        .and_then(|found| {
            found
                .as_str()
                .trim_matches(|c| c == '(' || c == ')')
                .parse::<u32>()
                .ok()
        });

    if let (Some((file_year, _)), Some(directory_year)) = (&from_file, from_directory) {
        if *file_year != directory_year {
            return Err(Unresolvable::ConflictingYear {
                directory: directory_year,
                file: *file_year,
            });
        }
    }

    let (year, before_year) = match (from_file, from_directory) {
        (Some((file_year, found)), _) => (file_year, &stem[..found.start()]),
        (None, Some(directory_year)) => (directory_year, stem),
        (None, None) => return Err(Unresolvable::NoReleaseYear),
    };

    // Square-bracketed groups describe the file, not the film - `[1080p]`,
    // `[Multiple Subtitle]`, a fansub tag. Dropping them whole is what keeps a
    // name carrying several of them from having their fragments read as a title.
    let without_groups = bracketed_group().replace_all(before_year, " ");

    // And the quality has to come out of the title as well as being recorded,
    // or a film named `Show 2160p (2001)` keeps it in both places.
    let without_quality = quality_marker().replace_all(&without_groups, " ");
    let title = clean_name(&strip_release_tokens(
        &dots_to_spaces(&without_quality),
        is_dot_separated(stem),
    ));
    if title.is_empty() {
        return Err(Unresolvable::NoMovieTitle);
    }

    Ok(MediaName::Movie(Movie {
        root,
        directories: directories.iter().map(|part| part.to_string()).collect(),
        title,
        year,
        quality,
        extension: extension.to_string(),
    }))
}

/// Whether a name uses dots where another would use spaces.
///
/// Being written this way is itself evidence: a person types `The Foretelling`,
/// a release tool writes `The.Foretelling.1080p.BluRay`. It is half of what
/// licenses cutting a name short at a technical token.
fn is_dot_separated(text: &str) -> bool {
    let trimmed = text.trim_matches(|c: char| c.is_whitespace() || "-_.".contains(c));

    trimmed.contains('.') && !trimmed.contains(' ')
}

/// Pull the episode title and any quality metadata out of what follows the marker.
fn parse_title_and_quality(remainder: &str) -> (Option<String>, Option<String>) {
    let quality = quality_marker()
        .find(remainder)
        .map(|found| found.as_str().to_lowercase());

    // Whatever a bracketed group holds - `[1080p]`, `[HorribleSubs]`, a CRC32
    // stamped on by a fansub tool - describes the file rather than names the
    // episode. Quality has just been read out of them; the groups go.
    let without_groups = bracketed_group().replace_all(remainder, " ");

    // Cut the quality out rather than blanking it: a space here would make a
    // dotted name look spaced, and `dots_to_spaces` would then leave its dots
    // alone - `Show.S01E01.Pilot.1080p` would keep the dots it needs converted.
    let without_quality = quality_marker().replace_all(&without_groups, "");
    let title = clean_name(&strip_release_tokens(
        &dots_to_spaces(&without_quality),
        is_dot_separated(remainder),
    ));

    (if title.is_empty() { None } else { Some(title) }, quality)
}

/// Replace dots with spaces, but only in a name that uses them as separators.
///
/// Two names that must not be treated alike:
///
/// - `Samurai.Jack` - dots stand in for spaces, and the words are readable once
///   they are put back.
/// - `S.W.A.T` - the dots belong to an acronym, and replacing them would spell
///   the title out letter by letter.
///
/// A name that already contains spaces is using its dots for something other
/// than separation, and a name whose dot-separated parts are all single letters
/// is an acronym. Everything else is treated as separated by dots.
fn dots_to_spaces(text: &str) -> String {
    let trimmed = text.trim_matches(|c: char| c.is_whitespace() || "-_.".contains(c));

    if trimmed.contains(' ') {
        return trimmed.to_string();
    }

    let is_acronym = trimmed.contains('.')
        && trimmed
            .split('.')
            .all(|segment| segment.chars().count() <= 1);
    if is_acronym {
        return trimmed.to_string();
    }

    trimmed.replace('.', " ")
}

/// Drop tokens that describe the release rather than the content.
///
/// Three kinds of token go, and each is gated differently:
///
/// - An unmistakable one ([`RELEASE_TOKENS`]) always goes.
/// - One that doubles as a word ([`AMBIGUOUS_RELEASE_TOKENS`]) goes only in a
///   name that carried an unmistakable one, so "The Web" keeps its title.
/// - The release group - the shouted token a scene release signs off with - goes
///   under the same condition, so a genuine title like `TKO` survives.
fn strip_release_tokens(text: &str, dotted: bool) -> String {
    // Split on whitespace only. A hyphen belongs to the title far more often
    // than to the release - `Dungeons & Dragons - Honour Among Thieves` - and
    // the one place it separates metadata, the `x265-iVy` sign-off, sits inside
    // the tail that gets cut below.
    let subtokens: Vec<&str> = text
        .split_whitespace()
        .filter(|token| !token.is_empty())
        .collect();

    let matches_any = |token: &str, known: &[&str]| {
        known.iter().any(|candidate| {
            token.eq_ignore_ascii_case(candidate)
                // `x265-iVy`: the token is the codec with its group attached.
                || token
                    .split('-')
                    .next()
                    .is_some_and(|head| head.eq_ignore_ascii_case(candidate))
        })
    };

    // Whether this name was written by a release tool rather than by a person.
    //
    // The evidence has to come from somewhere other than the word being judged,
    // or the test is circular: one technical word in a spaced name is a title
    // that contains a word - `The DTS Report` - and letting `DTS` vouch for
    // itself would leave `The Report`. So it takes either a second such word, or
    // dots standing in for spaces alongside the first.
    //
    // Dots alone are not enough either. Plenty of ordinary rips are dotted, and
    // in one of those an all-capital token is a word: `Tale of X9` names a
    // character.
    let strong_tokens = subtokens
        .iter()
        .filter(|token| matches_any(token, RELEASE_TOKENS))
        .count();
    let looks_like_a_scene_release = strong_tokens > 1 || (dotted && strong_tokens > 0);

    // A scene name is a title followed by a tail of technical tokens, so the
    // first unmistakable one marks where the title ended. Cutting there removes
    // the parts no list could enumerate: the `2.0` channel layout of `EAC3.2.0`,
    // and a release group whose capitalisation - `iVy` - defeats reading it as
    // shouted.
    let subtokens = if looks_like_a_scene_release {
        match subtokens
            .iter()
            .position(|token| matches_any(token, RELEASE_TOKENS))
        {
            Some(tail) => &subtokens[..tail],
            None => &subtokens[..],
        }
    } else {
        &subtokens[..]
    };

    let mut kept: Vec<&str> = Vec::new();
    for token in subtokens.iter().copied() {
        if looks_like_a_scene_release && matches_any(token, AMBIGUOUS_RELEASE_TOKENS) {
            continue;
        }
        // The release group is the shouted token a scene release signs off with.
        // Only a name that already showed release metadata is read this way, so
        // an ordinary title in capitals survives elsewhere.
        if looks_like_a_scene_release
            && token.chars().any(|c| c.is_alphabetic())
            && !token.chars().any(|c| c.is_lowercase())
        {
            continue;
        }
        kept.push(token);
    }

    // Episodes of shows that number themselves in roman numerals carry the
    // numeral separately from the title. Single letters are left alone, since
    // `I` and `C` are words and initials far more often than numerals.
    if kept.len() > 1 && kept[0].len() > 1 && kept[0].chars().all(|c| "IVXLCDM".contains(c)) {
        kept.remove(0);
    }

    kept.join(" ")
}

/// Trim separator noise and collapse whitespace.
fn clean_name(text: &str) -> String {
    let spaced = dots_to_spaces(text);

    spaced
        .split_whitespace()
        // Only brackets left holding nothing go - the `()` a quality marker
        // leaves behind when it is lifted out of `(1080p60)`. Brackets around
        // something are part of the name: `The Beginning (Part 1)`.
        .filter(|token| !token.chars().all(|c| "()[]{}".contains(c)))
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c: char| c.is_whitespace() || "-_.".contains(c))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn episode(path: &str) -> Episode {
        match parse(path).expect("should parse") {
            MediaName::Episode(episode) => episode,
            other => panic!("expected an episode, got {other:?}"),
        }
    }

    #[test]
    fn reads_season_and_episode_from_the_marker() {
        let parsed = episode("Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv");

        assert_eq!(parsed.series, "Elementary");
        assert_eq!(parsed.season, 6);
        assert_eq!(parsed.number, 8);
        assert_eq!(parsed.title.as_deref(), Some("Sand Trap"));
        assert_eq!(parsed.quality, None);
        assert_eq!(parsed.extension, "mkv");
    }

    #[test]
    fn reads_a_lowercase_marker_the_same_way() {
        let parsed = episode("Series/Breaking Bad/Season 01/Breaking Bad - s01e01 - Pilot.mkv");

        assert_eq!(parsed.season, 1);
        assert_eq!(parsed.number, 1);
        assert_eq!(parsed.title.as_deref(), Some("Pilot"));
    }

    #[test]
    fn keeps_the_season_directory_it_found_and_the_arc_name_on_it() {
        let parsed = episode("Anime/Bebop/Season 2 - The Long Night/Bebop - S02E03 - Ballad.mkv");

        assert_eq!(
            parsed.season_directory,
            SeasonDirectory::Numbered {
                number: 2,
                suffix: " - The Long Night".to_string()
            }
        );
        assert_eq!(parsed.directories, vec!["Bebop".to_string()]);
    }

    #[test]
    fn recognises_a_specials_directory() {
        let parsed = episode("Series/Firefly/Specials/Firefly - S00E01 - Making Of.mkv");

        assert_eq!(parsed.season_directory, SeasonDirectory::Specials);
    }

    #[test]
    fn records_that_there_was_no_season_directory() {
        let parsed = episode("Series/Some Show/Some Show - S01E13.webm");

        assert_eq!(parsed.season_directory, SeasonDirectory::Absent);
        assert!(parsed.directories.is_empty() || parsed.directories == vec!["Some Show"]);
    }

    #[test]
    fn takes_the_series_name_from_the_file_not_the_directory() {
        let parsed = episode(
            "Series/Super Best Friends Play - FFX/Super Best Friends Play - Final Fantasy X - S01E13 (1080p60).webm",
        );

        assert_eq!(parsed.series, "Super Best Friends Play - Final Fantasy X");
        assert_eq!(parsed.quality.as_deref(), Some("1080p60"));
        assert_eq!(parsed.title, None);
    }

    #[test]
    fn falls_back_to_the_directory_when_the_file_omits_the_series() {
        let parsed =
            episode("Series/Breaking Bad (2008) {tvdb-81189}/Season 01/S01E01 - Pilot.mkv");

        assert_eq!(parsed.series, "Breaking Bad");
    }

    #[test]
    fn turns_a_dotted_name_into_words() {
        let parsed = episode(
            "Series/Samurai Jack (2001)/Season 3/Samurai.Jack.S03E10.XXXVI.Jack.The.Monks.avi",
        );

        assert_eq!(parsed.series, "Samurai Jack");
        assert_eq!(parsed.title.as_deref(), Some("Jack The Monks"));
    }

    #[test]
    fn leaves_dots_alone_in_a_name_that_uses_spaces() {
        let parsed = episode("Series/S.W.A.T/Season 01/S.W.A.T. - S01E01 - Pilot.mkv");

        assert_eq!(parsed.series, "S.W.A.T");
    }

    #[test]
    fn drops_release_metadata_leaving_no_title() {
        let parsed = episode("Series/Scrubs/Season 9/Scrubs.S09E02.RETAIL.DVDRip.XviD-REWARD.avi");

        assert_eq!(parsed.series, "Scrubs");
        assert_eq!(parsed.title, None);
    }

    #[test]
    fn keeps_a_shouted_title_when_nothing_else_says_scene_release() {
        let parsed = episode("Series/Some Show/Season 01/Some Show - S01E04 - TKO.mkv");

        assert_eq!(parsed.title.as_deref(), Some("TKO"));
    }

    #[test]
    fn keeps_a_single_letter_that_could_be_a_roman_numeral() {
        let parsed = episode("Series/Some Show/Season 01/Some Show - S01E04 - I Love Lucy.mkv");

        assert_eq!(parsed.title.as_deref(), Some("I Love Lucy"));
    }

    #[test]
    fn reads_quality_from_brackets_or_parentheses() {
        for name in [
            "Series/Show/Season 01/Show - S01E01 - Pilot [1080p].mkv",
            "Series/Show/Season 01/Show - S01E01 - Pilot (1080p).mkv",
            "Series/Show/Season 01/Show.S01E01.Pilot.1080p.mkv",
        ] {
            let parsed = episode(name);
            assert_eq!(parsed.quality.as_deref(), Some("1080p"), "for {name}");
            assert_eq!(parsed.title.as_deref(), Some("Pilot"), "for {name}");
        }
    }

    #[test]
    fn reads_a_movie_year_from_the_file_or_its_directory() {
        let from_file = match parse("Movies/The Dark Knight (2008)/The Dark Knight (2008).mkv") {
            Ok(MediaName::Movie(movie)) => movie,
            other => panic!("expected a movie, got {other:?}"),
        };
        assert_eq!(from_file.title, "The Dark Knight");
        assert_eq!(from_file.year, 2008);

        let from_directory = match parse("Movies/The Dark Knight (2008)/The Dark Knight.mkv") {
            Ok(MediaName::Movie(movie)) => movie,
            other => panic!("expected a movie, got {other:?}"),
        };
        assert_eq!(from_directory.year, 2008);
    }

    #[test]
    fn refuses_a_path_that_is_not_a_media_file() {
        assert_eq!(parse("Series"), Err(Unresolvable::NotAMediaFile));
        assert_eq!(
            parse("Series/Show/Season 01/no-extension"),
            Err(Unresolvable::NotAMediaFile)
        );
    }

    #[test]
    fn keeps_title_words_that_double_as_release_tokens() {
        for (name, expected) in [
            (
                "Series/Show/Season 01/Show - S01E01 - The Web.mkv",
                "The Web",
            ),
            (
                "Series/Show/Season 01/Show - S01E02 - Extended Family.mkv",
                "Extended Family",
            ),
            (
                "Series/Show/Season 01/Show - S01E03 - Proper Villains.mkv",
                "Proper Villains",
            ),
            (
                "Series/Show/Season 01/Show - S01E04 - The Limited.mkv",
                "The Limited",
            ),
        ] {
            assert_eq!(episode(name).title.as_deref(), Some(expected), "for {name}");
        }
    }

    #[test]
    fn drops_those_same_words_from_a_name_that_is_plainly_a_release() {
        let parsed =
            episode("Series/Show/Season 01/Show.S01E01.PROPER.EXTENDED.720p.BluRay.x264.mkv");

        assert_eq!(parsed.title, None);
        assert_eq!(parsed.quality.as_deref(), Some("720p"));
    }

    #[test]
    fn a_movie_with_no_title_left_says_so_in_movie_terms() {
        assert_eq!(
            parse("Movies/Unsorted/2008.1080p.BluRay.x264.mkv"),
            Err(Unresolvable::NoMovieTitle)
        );
        assert_eq!(
            Unresolvable::NoMovieTitle.reason(),
            "no film title left once the year and release metadata are removed"
        );
    }
}
