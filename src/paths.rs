//! The one way a path becomes text.
//!
//! Naming patterns are written against forward-slash paths, so any path that is
//! matched against them has to be rendered the same way on every platform. A
//! path shown to the user goes through the same function, so one file reads the
//! same in every line that names it.

use std::path::Path;

/// Render a path as a string with `/` separators.
///
/// On Windows it converts the native `\\` separators, so `Series\\Charmed`
/// becomes `Series/Charmed`. On Unix the path is unchanged: there `\\` is an
/// ordinary file-name character, not a separator.
pub fn to_forward_slashes(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn converts_native_separators() {
        let path = PathBuf::from("Series").join("Charmed").join("Season 06");
        assert_eq!(to_forward_slashes(&path), "Series/Charmed/Season 06");
    }

    #[test]
    fn leaves_forward_slashes_alone() {
        assert_eq!(
            to_forward_slashes(Path::new("Series/Charmed/Season 06")),
            "Series/Charmed/Season 06"
        );
    }

    #[test]
    fn normalises_a_path_mixing_both_separators() {
        let mixed = Path::new("C:/media").join("Series").join("Charmed");
        assert!(!to_forward_slashes(&mixed).contains('\\'));
    }

    #[cfg(unix)]
    #[test]
    fn keeps_a_backslash_in_a_unix_file_name() {
        assert_eq!(
            to_forward_slashes(Path::new("Series/Show\\Name.mkv")),
            "Series/Show\\Name.mkv"
        );
    }
}
