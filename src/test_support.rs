//! Helpers the tests of more than one module share.
//!
//! What only the transcoding tests need lives in `ffmpeg/test_support.rs`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use crate::job::{Job, MediaFileType, Operation, PostProcessingSettings, QualitySettings};

/// Whether a real FFmpeg is available to exercise the code that runs it.
///
/// The tests that call this are the only ones that prove what FFmpeg does
/// with a real file, so a developer without FFmpeg installed skips them
/// rather than failing - but **CI must actually run them**. A skipped test
/// and a passing one are indistinguishable in the log, so if these quietly
/// stopped running nobody would find out until a library lost a track.
pub(crate) fn ffmpeg_present() -> bool {
    let available = Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);

    if !available {
        assert!(
            std::env::var("CI").is_err(),
            "FFmpeg must be installed in CI: these tests are the only check of what FFmpeg makes of a real file, and skipping them silently would leave that unverified"
        );
        eprintln!("skipping: ffmpeg is not on PATH");
    }

    available
}

/// A one-second test pattern and tone, encoded the way `args` say.
///
/// Input 0 is the picture and input 1 the tone, for an `args` that maps them.
pub(crate) fn build_test_file(path: &Path, args: &[&str]) {
    let built = Command::new("ffmpeg")
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=320x240:rate=24",
            "-f",
            "lavfi",
            "-i",
            "sine=duration=1",
        ])
        .args(args)
        .arg("-y")
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(built.success(), "could not build {path:?}");
}

/// Run `f` and return what it logged, as `main`'s subscriber prints it.
pub(crate) fn logs_of<T>(f: impl FnOnce() -> T) -> (T, String) {
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // While only one dispatcher is registered, tracing decides whether a
    // log line is wanted by asking the dispatcher of whichever thread
    // reaches it first. That is often another test's thread, which has
    // none, and the answer "never" is cached for every thread. A second
    // registered dispatcher makes it ask all of them.
    let _second = tracing::Dispatch::new(tracing_subscriber::registry());

    let sink = Sink::default();
    let writer = sink.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let result = tracing::subscriber::with_default(subscriber, f);
    let logs = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    (result, logs)
}

/// Backdate files, so a job looks as though nothing has touched it.
///
/// Unwrapped throughout: a helper that quietly did nothing would leave a
/// staleness test asserting that a *fresh* job is not stale, which it would
/// pass without testing anything.
pub(crate) fn age(paths: &[impl AsRef<Path>], by: Duration) {
    let when = std::fs::FileTimes::new().set_modified(SystemTime::now() - by);
    for path in paths {
        let file = std::fs::File::options()
            .write(true)
            .open(path.as_ref())
            .unwrap();
        file.set_times(when).unwrap();
    }
}

/// Write a file at a library-relative path, creating its directories.
pub(crate) fn touch(root: &Path, relative: &str, contents: &str) {
    let path = crate::fix::absolute(root, relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// A group written the way a path reads, for an assertion to state.
pub(crate) fn group(directories: &str) -> Vec<String> {
    directories.split('/').map(str::to_string).collect()
}

/// Clear the encoding environment variables so a test observes documented
/// defaults rather than whatever the developer's shell happens to export.
/// Only safe inside `#[serial(env)]` tests.
pub(crate) fn clear_encoding_env() {
    std::env::remove_var("FFMPEG_PRESET");
    std::env::remove_var("FFMPEG_CRF");
    std::env::remove_var("FFMPEG_AUDIO_BITRATE");
}

/// A re-encode of one MKV under `media_root`, with every setting at its
/// default: the job a test needs when what the job does is not the point.
pub(crate) fn mkv_reencode(input: impl Into<PathBuf>, media_root: &Path) -> Job {
    Job::new(
        input.into(),
        MediaFileType::Mkv,
        Operation::Reencode { channels: None },
        QualitySettings::default(),
        PostProcessingSettings::default(),
        media_root,
    )
}

/// Every job in the queue under `work_root`, ordered by the file it
/// transcodes.
pub(crate) fn queued_jobs(work_root: &Path) -> Vec<Job> {
    let mut jobs: Vec<Job> = std::fs::read_dir(work_root.join("_queue"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension() == Some("job".as_ref()))
        .map(|path| serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap())
        .collect();
    jobs.sort_by(|left, right| left.input_path.cmp(&right.input_path));
    jobs
}
