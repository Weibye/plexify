//! Path rendering helpers shared by the library-naming code.
//!
//! Naming patterns are written against forward-slash paths, so any path that is
//! matched against them - or shown to the user next to them - has to be rendered
//! the same way regardless of platform.

use std::path::Path;

/// Render a path as a string with `/` separators.
///
/// On Unix this is the path unchanged. On Windows it converts the native `\\`
/// separators, so `Series\\Charmed` becomes `Series/Charmed`.
pub fn to_forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
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
}
