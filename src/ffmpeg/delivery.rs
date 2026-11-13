//! What becomes of a job's output once it is encoded: moved into the library
//! with its sidecars and its source disabled, or, for a parked job, discarded.

use anyhow::{anyhow, Result};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::{chunk_dir_for, FFmpegProcessor};
use crate::job::Job;
use crate::subtitles;

/// Where a finished output is copied to before it is renamed onto its
/// destination.
///
/// It sits in the destination directory, because a rename is only atomic within
/// one filesystem, and carries a suffix no part of the pipeline treats as media
/// so that a copy which never finished cannot be taken for an encode that did.
///
/// The name carries `worker_id` because a staging name shared between workers is
/// a second way to arrive at the corrupt output the staging name exists to
/// prevent: two workers holding one input would copy into one file and each
/// rename the splice onto the destination, which every later run then accepts as
/// a finished encode. The id belongs to the worker rather than to the call so
/// that a worker retrying a move writes over its own last attempt instead of
/// leaving another part-copy beside the destination.
pub fn staging_path_for(final_output_path: &Path, worker_id: &Uuid) -> PathBuf {
    let mut staging = final_output_path.as_os_str().to_os_string();
    staging.push(format!(".{worker_id}.partial"));
    PathBuf::from(staging)
}

/// `film.mkv` becomes `film.mkv.disabled`: the whole name is kept, byte for
/// byte, so a person can see what the file was and rename it back.
fn disabled_path(path: &Path) -> PathBuf {
    let mut disabled = path.as_os_str().to_os_string();
    disabled.push(".disabled");
    PathBuf::from(disabled)
}

/// Remove what a failed move left at `path`.
///
/// Nothing there is what a copy that never started leaves, so only another
/// failure is worth a word.
async fn remove_leftover(path: &Path) {
    match tokio::fs::remove_file(path).await {
        Err(e) if e.kind() != ErrorKind::NotFound => {
            warn!("Could not remove {path:?} after a failed move: {e}")
        }
        _ => {}
    }
}

impl FFmpegProcessor {
    /// Throw away everything a job left in the work folder.
    ///
    /// Called when a job is parked in `_failed`, because nothing will ever come
    /// back for it: the job id is derived from the input path, so `job_exists`
    /// sees the parked job and no re-scan will queue that file again. Its
    /// chunks would otherwise sit in `_in_progress` at roughly the size of the
    /// finished output, with nothing pointing at them.
    pub async fn discard_work(&self, job: &Job, work_folder: &Path) {
        self.discard_work_for_id(&job.id, work_folder).await;
    }

    /// The same, for a job known only by its id.
    ///
    /// The startup sweep parks jobs too - one that keeps taking its worker down
    /// ends up in `_failed` the same way one that keeps failing does - and it
    /// has only the job filename to go on. Everything a job leaves in the work
    /// folder is named after its id, so that is enough.
    pub async fn discard_work_for_id(&self, job_id: &str, work_folder: &Path) {
        let chunk_dir = chunk_dir_for(job_id, work_folder);
        if chunk_dir.exists() {
            match tokio::fs::remove_dir_all(&chunk_dir).await {
                Ok(_) => info!("🗑️ Discarded the chunks of a parked job: {chunk_dir:?}"),
                Err(e) => warn!("Could not remove {chunk_dir:?}: {e}"),
            }
        }

        // A part-written output from before this file was chunked, or from the
        // join that was interrupted. Named `{id}_{filename}`, so the filename
        // this job would have used is not needed to find it.
        let prefix = format!("{job_id}_");
        let unreadable = |e: std::io::Error| {
            warn!("Could not read {work_folder:?} to discard what job {job_id} left there: {e}")
        };
        let mut entries = match tokio::fs::read_dir(work_folder).await {
            Ok(entries) => entries,
            Err(e) => return unreadable(e),
        };
        loop {
            let entry = match entries.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(e) => return unreadable(e),
            };
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                if let Err(e) = tokio::fs::remove_file(entry.path()).await {
                    warn!("Could not remove {:?}: {e}", entry.path());
                }
            }
        }
    }

    /// Move completed file from work folder to media folder.
    ///
    /// The work root and the media root are routinely on different volumes, so
    /// the file has to be copied rather than renamed - and a copy is not atomic.
    /// It therefore goes to a staging name beside the destination and is renamed
    /// onto the destination from there, which within one directory is atomic. A
    /// media server sees the output either absent or whole, and a copy that dies
    /// part-way leaves a `.partial` that no later run mistakes for a finished
    /// encode: [`Job::output_exists`] does not accept it, so the job is tried
    /// again rather than recorded as done.
    ///
    /// The staging name is this worker's alone ([`staging_path_for`]), so two
    /// workers that both end up holding one input cannot splice their copies
    /// together into one file and land it at the destination.
    pub async fn move_to_destination(
        &self,
        job: &Job,
        media_root: Option<&Path>,
        work_folder: &Path,
    ) -> Result<()> {
        let work_output_path = job.work_folder_output_path(work_folder);
        let final_output_path = job.full_output_path(media_root);

        // Ensure the work folder output file exists
        if !work_output_path.exists() {
            return Err(anyhow!(
                "Work folder output file does not exist: {work_output_path:?}"
            ));
        }

        // Create final output directory if it doesn't exist
        if let Some(parent) = final_output_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Move the file from work folder to final location
        let staging_path = staging_path_for(&final_output_path, &self.worker_id);

        if let Err(e) = tokio::fs::copy(&work_output_path, &staging_path).await {
            remove_leftover(&staging_path).await;
            return Err(anyhow!("Failed to copy output to {staging_path:?}: {e}"));
        }

        // `fs::rename` replaces whatever is at the destination; this refuses it.
        if let Err(e) = crate::fix::rename(&staging_path, &final_output_path) {
            remove_leftover(&staging_path).await;
            return Err(anyhow!(
                "Failed to move output into place at {final_output_path:?}: {e:#}"
            ));
        }

        // A media file and the files named after it move as a group, and in
        // this order: a sidecar that arrives first is a subtitle for a file
        // that is not there yet, and one that never arrives is a track the
        // library will never show.
        self.move_sidecars(&work_output_path, &final_output_path)
            .await;

        // The output is where it belongs, so the move has succeeded. A work file
        // that will not delete is worth a word but not a failed job: reporting
        // failure here would send the job round again to find its own finished
        // output at the destination.
        if let Err(e) = tokio::fs::remove_file(&work_output_path).await {
            warn!("Could not remove {work_output_path:?} after moving it: {e}");
        }

        info!(
            "📁 Moved completed file: {:?} -> {:?}",
            work_output_path, final_output_path
        );

        Ok(())
    }
    /// Move the subtitle sidecars written beside a work-folder output to sit
    /// beside the finished file.
    ///
    /// Only the subtitle extensions this project writes are carried, rather
    /// than everything sharing the stem. The work folder also holds this job's
    /// chunk directory and its partial files, and a mover that took anything
    /// with the right prefix would eventually take one of those.
    ///
    /// A sidecar that will not move is a warning, not a failed job: the media
    /// file is already at its destination, and failing here would send the job
    /// round again to find its own finished output in place.
    async fn move_sidecars(&self, work_output: &Path, final_output: &Path) {
        let (Some(directory), Some(work_stem), Some(final_stem)) = (
            work_output.parent(),
            work_output.file_stem().map(|stem| stem.to_string_lossy()),
            final_output.file_stem(),
        ) else {
            return;
        };

        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(e) => {
                warn!(
                    "Could not read {directory:?} for the subtitles of {final_output:?}, so none \
                     were moved beside it: {e}"
                );
                return;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().to_string();
            let Some(suffix) = name
                .strip_prefix(work_stem.as_ref())
                .and_then(|rest| rest.strip_prefix('.'))
            else {
                continue;
            };

            let extension = suffix.rsplit('.').next().unwrap_or_default();
            if !subtitles::is_sidecar_extension(extension) {
                continue;
            }

            let mut name = final_stem.to_os_string();
            name.push(format!(".{suffix}"));
            let destination = final_output.with_file_name(name);
            // `copy` would replace a subtitle somebody already put there.
            if destination.exists() {
                warn!("Kept {path:?}: {destination:?} already exists; it will not be overwritten");
                continue;
            }
            if let Err(e) = tokio::fs::copy(&path, &destination).await {
                warn!("Could not put the subtitle {destination:?} beside its file: {e}");
                continue;
            }

            if let Err(e) = tokio::fs::remove_file(&path).await {
                warn!("Could not remove {path:?} after moving it: {e}");
            }

            info!("Moved subtitle: {:?} -> {:?}", path, destination);
        }
    }

    pub async fn disable_source_files(&self, job: &Job, media_root: Option<&Path>) -> Result<()> {
        let mut sources = vec![job.full_input_path(media_root)];
        // A WebM's subtitle lives beside it as a `.vtt`.
        sources.extend(
            job.full_subtitle_path(media_root)
                .filter(|vtt| vtt.exists()),
        );
        let moves: Vec<(PathBuf, PathBuf)> = sources
            .into_iter()
            .map(|path| {
                let disabled = disabled_path(&path);
                (path, disabled)
            })
            .collect();

        // Every name is checked before anything moves: a source disabled without
        // its subtitle leaves Plex a subtitle beside nothing. `fix::rename` checks
        // again, since `fs::rename` would replace a `.disabled` file already there.
        if let Some((_, taken)) = moves.iter().find(|(_, disabled)| disabled.exists()) {
            return Err(anyhow!(
                "{taken:?} already exists; it will not be overwritten"
            ));
        }
        for (path, disabled) in &moves {
            crate::fix::rename(path, disabled)?;
            debug!("Disabled {path:?} -> {disabled:?}");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{
        AudioAction, MediaFileType, Operation, PostProcessingSettings, QualitySettings,
        SubtitleAction,
    };
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_work_folder_output_path_generation() {
        let temp_dir = TempDir::new().unwrap();
        let work_folder = temp_dir.path();

        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings {
            disable_source_files: false,
        };
        let media_root = temp_dir.path();
        let job = Job::new(
            PathBuf::from("test.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
            quality,
            post_processing,
            media_root,
        );

        let work_output_path = job.work_folder_output_path(work_folder);

        // Verify the path structure
        assert!(work_output_path.starts_with(work_folder));
        assert!(work_output_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .contains(&job.id));
        assert!(work_output_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with("test.mp4"));
    }

    #[tokio::test]
    async fn test_move_to_destination() {
        let temp_dir = TempDir::new().unwrap();
        let work_folder = temp_dir.path().join("work");
        let media_folder = temp_dir.path().join("media");

        tokio::fs::create_dir_all(&work_folder).await.unwrap();
        tokio::fs::create_dir_all(&media_folder).await.unwrap();

        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings {
            disable_source_files: false,
        };
        let job = Job::new(
            PathBuf::from("test.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
            quality,
            post_processing,
            &media_folder,
        );

        // Create a dummy file in the work folder
        let work_output_path = job.work_folder_output_path(&work_folder);
        tokio::fs::write(&work_output_path, "test content")
            .await
            .unwrap();

        let processor = FFmpegProcessor::new(false);

        // The job holds absolute paths, so there is no media root to join.
        processor
            .move_to_destination(&job, None, &work_folder)
            .await
            .unwrap();

        // Verify the file was moved
        assert!(!work_output_path.exists());
        let final_path = job.full_output_path(None);
        assert!(final_path.exists());

        let content = tokio::fs::read_to_string(&final_path).await.unwrap();
        assert_eq!(content, "test content");
    }

    /// A job whose output goes to `media/`, with the finished encode already
    /// sitting in the work folder waiting to be moved.
    async fn job_ready_to_move(temp_dir: &TempDir) -> (Job, PathBuf) {
        let work_folder = temp_dir.path().join("work");
        let media_folder = temp_dir.path().join("media");
        tokio::fs::create_dir_all(&work_folder).await.unwrap();
        tokio::fs::create_dir_all(&media_folder).await.unwrap();

        let job = Job::new(
            PathBuf::from("film.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
            QualitySettings::default(),
            PostProcessingSettings {
                disable_source_files: false,
            },
            &media_folder,
        );

        tokio::fs::write(job.work_folder_output_path(&work_folder), "a whole encode")
            .await
            .unwrap();

        (job, work_folder)
    }

    #[tokio::test]
    async fn a_move_that_fails_leaves_nothing_the_pipeline_calls_finished() {
        let temp_dir = TempDir::new().unwrap();
        let (job, work_folder) = job_ready_to_move(&temp_dir).await;
        let final_path = job.full_output_path(None);

        let processor = FFmpegProcessor::new(false);

        // A directory in the way of the staging name is a copy that cannot
        // start, standing in for the disk filling up or the share dropping out
        // half way through one.
        let staging_path = staging_path_for(&final_path, &processor.worker_id);
        tokio::fs::create_dir(&staging_path).await.unwrap();

        assert!(processor
            .move_to_destination(&job, None, &work_folder)
            .await
            .is_err());

        assert!(
            !final_path.exists(),
            "the destination name is only ever taken by a finished file"
        );
        assert!(
            !job.output_exists(None),
            "a job whose move failed must be tried again, not recorded as done"
        );
        assert!(
            job.work_folder_output_path(&work_folder).exists(),
            "the encode that was not moved stays where the next attempt looks"
        );

        // And the next attempt, with the obstruction gone, completes the move.
        tokio::fs::remove_dir(&staging_path).await.unwrap();
        processor
            .move_to_destination(&job, None, &work_folder)
            .await
            .unwrap();

        assert_eq!(
            tokio::fs::read_to_string(&final_path).await.unwrap(),
            "a whole encode"
        );
        assert!(!staging_path.exists());
        assert!(!job.work_folder_output_path(&work_folder).exists());
    }

    // The three tests below guard against `fs::rename` and `fs::copy`, which
    // replace an existing file without a word, on Linux and Windows alike.

    #[tokio::test]
    async fn delivery_never_writes_over_a_file_already_at_the_output_name() {
        let temp_dir = TempDir::new().unwrap();
        let (job, work_folder) = job_ready_to_move(&temp_dir).await;
        let final_path = job.full_output_path(None);
        tokio::fs::write(&final_path, "somebody's file")
            .await
            .unwrap();
        let processor = FFmpegProcessor::new(false);

        let error = processor
            .move_to_destination(&job, None, &work_folder)
            .await
            .expect_err("a taken output name is refused");

        assert!(
            format!("{error:#}").contains(&format!("{final_path:?}")),
            "the refusal names the file it would have replaced: {error:#}"
        );
        assert_eq!(
            tokio::fs::read_to_string(&final_path).await.unwrap(),
            "somebody's file"
        );
        assert!(
            job.work_folder_output_path(&work_folder).exists(),
            "the encode is kept for a person to look at"
        );
        assert!(!staging_path_for(&final_path, &processor.worker_id).exists());
    }

    #[tokio::test]
    async fn delivery_never_writes_over_a_subtitle_already_beside_the_output() {
        let temp_dir = TempDir::new().unwrap();
        let (job, work_folder) = job_ready_to_move(&temp_dir).await;
        let work_output = job.work_folder_output_path(&work_folder);
        let work_subtitle = work_output.with_extension("eng.srt");
        tokio::fs::write(&work_subtitle, "extracted").await.unwrap();
        let existing = job.full_output_path(None).with_extension("eng.srt");
        tokio::fs::write(&existing, "somebody's subtitle")
            .await
            .unwrap();

        FFmpegProcessor::new(false)
            .move_to_destination(&job, None, &work_folder)
            .await
            .expect("a subtitle that cannot land does not hold back its file");

        assert_eq!(
            tokio::fs::read_to_string(&existing).await.unwrap(),
            "somebody's subtitle"
        );
        assert!(
            work_subtitle.exists(),
            "the extracted subtitle is kept for a person to look at"
        );
    }

    #[tokio::test]
    async fn disabling_a_source_never_writes_over_a_disabled_file() {
        let temp_dir = TempDir::new().unwrap();
        let media = temp_dir.path();
        let job = Job::new(
            PathBuf::from("show.webm"),
            MediaFileType::WebM,
            Operation::Reencode { channels: None },
            QualitySettings::default(),
            PostProcessingSettings::default(),
            media,
        );
        let processor = FFmpegProcessor::new(false);
        let source = media.join("show.webm");
        let subtitle = media.join("show.vtt");
        let disabled_source = media.join("show.webm.disabled");
        let disabled_subtitle = media.join("show.vtt.disabled");

        // The source's own `.disabled` name is taken.
        std::fs::write(&source, "source").unwrap();
        std::fs::write(&disabled_source, "an older source").unwrap();
        let error = processor
            .disable_source_files(&job, None)
            .await
            .expect_err("a taken .disabled name is refused");
        assert!(
            format!("{error:#}").contains(&format!("{disabled_source:?}")),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "source");
        assert_eq!(
            std::fs::read_to_string(&disabled_source).unwrap(),
            "an older source"
        );

        // The source's is free, the subtitle's is taken. Neither moves: a source
        // disabled without its subtitle leaves Plex a subtitle beside nothing.
        std::fs::remove_file(&disabled_source).unwrap();
        std::fs::write(&subtitle, "subtitle").unwrap();
        std::fs::write(&disabled_subtitle, "an older subtitle").unwrap();
        let error = processor
            .disable_source_files(&job, None)
            .await
            .expect_err("a taken .vtt.disabled name is refused");
        assert!(
            format!("{error:#}").contains(&format!("{disabled_subtitle:?}")),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "source");
        assert!(!disabled_source.exists());
        assert_eq!(std::fs::read_to_string(&subtitle).unwrap(), "subtitle");
        assert_eq!(
            std::fs::read_to_string(&disabled_subtitle).unwrap(),
            "an older subtitle"
        );
    }

    /// A name that is not UTF-8 is still a name somebody has to recognise and
    /// rename back. A lossy conversion would disable `film.m\u{FFFD}v` or, by
    /// dropping the extension, `film..disabled`.
    #[test]
    fn a_name_that_is_not_utf8_is_disabled_whole() {
        #[cfg(unix)]
        let name = {
            use std::os::unix::ffi::OsStrExt;
            std::ffi::OsStr::from_bytes(b"film.m\xffv").to_os_string()
        };
        #[cfg(windows)]
        let name = {
            use std::os::windows::ffi::OsStringExt;
            // `film.m`, an unpaired surrogate, `v`.
            std::ffi::OsString::from_wide(&[0x66, 0x69, 0x6C, 0x6D, 0x2E, 0x6D, 0xD800, 0x76])
        };
        assert!(name.to_str().is_none(), "the fixture must not be UTF-8");

        let mut expected = name.clone();
        expected.push(".disabled");
        assert_eq!(
            disabled_path(&Path::new("media").join(&name)),
            Path::new("media").join(&expected),
            "{name:?} should keep every byte of its name"
        );
    }

    #[tokio::test]
    async fn a_part_copied_file_left_by_a_killed_move_is_not_taken_for_the_output() {
        let temp_dir = TempDir::new().unwrap();
        let (job, work_folder) = job_ready_to_move(&temp_dir).await;
        let final_path = job.full_output_path(None);
        let processor = FFmpegProcessor::new(false);
        let staging_path = staging_path_for(&final_path, &processor.worker_id);

        // What a worker killed mid-copy leaves behind: some of the bytes, under
        // the staging name. This worker's own, because a worker retrying its
        // move is the case that has to write over it rather than around it.
        tokio::fs::write(&staging_path, "a whole").await.unwrap();

        assert!(
            !job.output_exists(None),
            "debris from an interrupted copy is not an output"
        );

        processor
            .move_to_destination(&job, None, &work_folder)
            .await
            .unwrap();

        assert_eq!(
            tokio::fs::read_to_string(&final_path).await.unwrap(),
            "a whole encode",
            "the retry writes over the debris rather than around it"
        );
        assert!(!staging_path.exists());
    }

    #[test]
    fn two_workers_stage_one_destination_under_names_of_their_own() {
        let final_path = PathBuf::from("/library/Series/Show/Show - S01E01.mp4");

        let one = FFmpegProcessor::new(false);
        let other = FFmpegProcessor::new(false);

        assert_ne!(
            staging_path_for(&final_path, &one.worker_id),
            staging_path_for(&final_path, &other.worker_id),
            "two workers holding one input must not copy into one file"
        );

        // A worker's own staging name does not move under it between the copy
        // and the rename, so a retry writes over its last part-copy.
        assert_eq!(
            staging_path_for(&final_path, &one.worker_id),
            staging_path_for(&final_path, &one.worker_id)
        );

        // And whatever the name, the pipeline still reads it as staging debris
        // rather than as media.
        assert_eq!(
            staging_path_for(&final_path, &one.worker_id)
                .extension()
                .unwrap(),
            "partial"
        );
    }

    #[tokio::test]
    async fn two_workers_moving_one_output_do_not_splice_their_copies() {
        let temp_dir = TempDir::new().unwrap();
        let media_folder = temp_dir.path().join("media");
        tokio::fs::create_dir_all(&media_folder).await.unwrap();

        let job = Job::new(
            PathBuf::from("film.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
            QualitySettings::default(),
            PostProcessingSettings {
                disable_source_files: false,
            },
            &media_folder,
        );
        let final_path = job.full_output_path(None);

        // Two workers that both ended up holding this input - the queue is
        // meant to prevent it, but the destination is a media library and the
        // cost of being wrong is a file nobody can reconstruct. Each has its
        // own encode of the same source, big enough that the two copies are in
        // flight at the same time.
        let encodes = [(b'a', "one"), (b'b', "other")].map(|(byte, name)| {
            let work_folder = temp_dir.path().join(name);
            std::fs::create_dir_all(&work_folder).unwrap();
            let content = vec![byte; 8 * 1024 * 1024];
            std::fs::write(job.work_folder_output_path(&work_folder), &content).unwrap();
            (FFmpegProcessor::new(false), work_folder, content)
        });
        let [(one, one_work, one_content), (other, other_work, other_content)] = &encodes;

        let (first, second) = tokio::join!(
            one.move_to_destination(&job, None, one_work),
            other.move_to_destination(&job, None, other_work)
        );
        // The first to land keeps the destination, and the other refuses to
        // write over it. Whichever it was, the destination holds one worker's
        // encode from end to end. A shared staging name would leave the two
        // interleaved, and `output_exists` would call that finished.
        let winner = match (&first, &second) {
            (Ok(()), Err(_)) => one_content,
            (Err(_), Ok(())) => other_content,
            _ => panic!("exactly one move lands: {first:?}, {second:?}"),
        };
        let landed = tokio::fs::read(&final_path).await.unwrap();
        assert!(
            &landed == winner,
            "the destination holds {} bytes that are not the encode that landed",
            landed.len()
        );

        let mut left_behind = Vec::new();
        let mut entries = std::fs::read_dir(&media_folder).unwrap();
        while let Some(Ok(entry)) = entries.next() {
            left_behind.push(entry.file_name().to_string_lossy().to_string());
        }
        assert_eq!(
            left_behind,
            vec![final_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()],
            "each worker takes its own staging file with it"
        );
    }

    /// A sidecar written into the work folder has to arrive beside the finished
    /// file, or the library never sees the track.
    #[tokio::test]
    async fn sidecars_move_out_of_the_work_folder_with_their_file() {
        let temp_dir = TempDir::new().unwrap();
        let work_folder = temp_dir.path().join("work");
        let media = temp_dir.path().join("media");
        std::fs::create_dir_all(&work_folder).unwrap();
        std::fs::create_dir_all(&media).unwrap();

        let job = Job::new(
            media.join("show.mkv"),
            MediaFileType::Mkv,
            Operation::Remux {
                audio: AudioAction::Copy,
                subtitles: SubtitleAction::Extract,
            },
            QualitySettings::default(),
            PostProcessingSettings::default(),
            &media,
        );

        let work_output = job.work_folder_output_path(&work_folder);
        let work_stem = work_output
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .to_string();
        std::fs::write(&work_output, "video").unwrap();
        std::fs::write(work_folder.join(format!("{work_stem}.eng.srt")), "subs").unwrap();
        // Not a sidecar, and it must not be dragged along.
        std::fs::write(work_folder.join(format!("{work_stem}.log")), "noise").unwrap();

        FFmpegProcessor::new(false)
            .move_to_destination(&job, None, &work_folder)
            .await
            .expect("the move should succeed");

        assert!(media.join("show.eng.srt").exists(), "the subtitle followed");
        assert!(!work_folder.join(format!("{work_stem}.eng.srt")).exists());
        assert!(
            work_folder.join(format!("{work_stem}.log")).exists(),
            "only subtitles move"
        );
    }
}
