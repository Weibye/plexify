#!/usr/bin/env bash
# Build a disposable media-library tree for exercising the plexify CLI by hand.
# The destination is deleted and recreated. Never point it at a real library.
set -euo pipefail

ROOT="${1:-}"
if [ -z "$ROOT" ]; then
    echo "usage: make-fixture.sh <destination-root>" >&2
    exit 2
fi

case "$ROOT" in
    / | "" | "$HOME") echo "refusing to build the fixture at '$ROOT'" >&2; exit 2 ;;
esac

rm -rf "$ROOT"

touch_file() {
    mkdir -p "$(dirname "$ROOT/$1")"
    : > "$ROOT/$1"
}

# --- Canonical: these must validate clean -------------------------------------
touch_file "Series/Charmed/Season 06/Charmed - S06E17 - Hyde School Reunion.avi"
touch_file "Anime/Cowboy Bebop/Season 01/Cowboy Bebop - S01E01 - Asteroid Blues.mkv"
touch_file "Movies/The Dark Knight (2008)/The Dark Knight (2008).mkv"
touch_file "Movies/Marvel Cinematic Universe Collection/Iron Man (2008).mkv"

# --- Lowercase episode marker -------------------------------------------------
touch_file "Series/Breaking Bad {tvdb-81189}/Season 01/Breaking Bad - s01e01 - Pilot.mkv"

# --- Unpadded season, missing dash before the episode title -------------------
touch_file "Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv"
touch_file "Series/Elementary/Season 6/Elementary - S06E09 Nobody Lives Forever.mkv"

# --- Scene-release naming: dots for spaces, release-group cruft ---------------
touch_file "Series/Scrubs/Season 9/Scrubs.S09E02.RETAIL.DVDRip.XviD-REWARD.avi"

# --- Dotted name, unpadded season, apostrophe in the episode title ------------
touch_file "Series/Samurai Jack (2001)/Season 3/Samurai.Jack.S03E10.XXXVI.Jack.The.Monks.and.the.Ancient.Master's.Son.avi"

# --- Quality metadata in parentheses instead of brackets ----------------------
touch_file "Series/Super Best Friends Play - FFX/Super Best Friends Play - Final Fantasy X - S01E13 (1080p60).webm"
touch_file "Series/Super Best Friends Play - FFX/Super Best Friends Play - Final Fantasy X - S01E13 (1080p60).vtt"

# --- No season directory at all: must be moved into one -----------------------
touch_file "Series/Loose Show/Loose Show - S02E03 - Wandering.mkv"

# --- Filed under the wrong season: the marker decides, not the directory ------
touch_file "Series/Misfiled/Season 01/Misfiled - S04E02 - Wrong Home.mkv"

# --- Duplicated tree root: report, never auto-fix -----------------------------
touch_file "Series/Veronica Mars/Series/Veronica Mars S02E04/Season 01/Veronica Mars S02E04.mp4"

# --- Lowercase episode marker, must normalise to uppercase --------------------
touch_file "Series/Firefly/Season 1/Firefly - s01e02 - The Train Job.mkv"

# --- Must be skipped via .plexifyignore ---------------------------------------
touch_file "Downloads/some.release.group.S01E01.mkv"
touch_file "Series/Charmed/Season 06/artwork.tmp"

cat > "$ROOT/.plexifyignore" <<'IGNORE'
# Fixture ignore rules - exercised on every validate/scan run
Downloads/
*.tmp
IGNORE

echo "Fixture built at: $ROOT"
echo
find "$ROOT" -type f | sed "s|^$ROOT/|  |" | sort
echo
echo "Try:"
echo "  cargo run -- validate $ROOT"
echo "  cargo run -- scan $ROOT --work-dir ${ROOT}-queue"
echo
echo "Clean up with:  rm -rf $ROOT ${ROOT}-queue"
