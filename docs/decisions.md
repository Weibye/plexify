# Decisions

Each entry is a choice that is hard to reverse, surprising, and had real alternatives. Entries
are numbered and appended. A merged entry is never edited: a later entry that replaces it says
**Supersedes N**. Entries 1–26 were recorded on 2026-11-09 from the earlier `CLAUDE.md`, and
27–28 on the same day. Later entries carry their own date.

Template:

```markdown
## N. Title

**Decision.** What we do.
**Why.** What forced it, with the measurement if there was one.
**Consequences.** What must stay true, and the test that guards it.
```

---

## 1. The filesystem is the queue

**Decision.** A job is a JSON file. The queue is four directories under the work root:
`_queue` → `_in_progress` → `_completed` or `_failed`. There is no server, broker or lock
daemon.

**Why.** Any number of `work` processes, on any number of machines, can share one directory
with no infrastructure.

**Consequences.**
- Every transition is an atomic rename.
- The work root (`-w`, which defaults to the current directory) is not the media root. Two
  shells in different directories silently produce two queues.
- Any command that reports on a queue prints the work root it resolved.

## 2. A job is named after its input

**Decision.** The job id is a v5 UUID of the resolved input path (`Job::id_for_input`).

**Why.** The same file always maps to the same job name. Re-scanning is idempotent, and
racing processes have a name to collide on.

**Consequences.**
- Never make the id random.
- `job_exists` checks all four directories, plus `.taken`, so a scan can't re-queue a
  parked job.
- Anything that changes over time, such as the attempt count, lives inside the job file,
  never in its name.

## 3. Enqueue and claim are renames

**Decision.** Enqueue writes a staging file and renames it onto `{id}.job`. Claim renames
the job from `_queue` into `_in_progress`. Rewriting a job (`write_job_atomically`) also
stages, then renames.

**Why.** Rename is atomic, so exactly one claimant wins and nobody ever sees a torn file.
The alternative, a marker file that claims the name, was rejected: a scanner that died
after taking the name would leave a file in no queue at all.

**Consequences.**
- Enqueue publishes but does not reserve. Two scanners can both write, and the second
  rename wins.
- A claim that loses sees the file absent and moves on.

## 4. A claim is kept alive by a heartbeat

**Decision.**
- `try_claim_job_file` writes `{id}.job.heartbeat` before it returns. The `ClaimedJob` it
  returns refreshes the heartbeat every `HEARTBEAT_INTERVAL` and stops on drop.
- `reclaim_stranded_jobs` runs at worker startup and returns jobs that have been quiet for
  `STALE_AFTER` to `_queue`.

**Why.** Rename keeps the mtime, so a job that waited in a backlog is already "stale" the
moment it is claimed. A heartbeat left to the caller would open a window in which two workers
hold one job. Touching the job file itself instead could leave it half-written.

**Consequences.**
- Staleness comes from two timestamps:
  - the job file's mtime, read **before** the take, as a fallback for a worker that died
    before its first heartbeat;
  - the heartbeat, read **after** the take, which catches a claim made while the sweep was
    reading.
- `status` and `clean` share one definition, `queue::is_stale` and `last_activity`. The
  sweep must not go through `last_activity`, because that reads both timestamps at once.
- Guarded by `a_job_claimed_without_a_heartbeat_is_still_reclaimed`.

## 5. A job leaves in-progress only by being taken

**Decision.** `take_for_move` is the only way out of `_in_progress`:
1. It marks the job's mtime through a handle opened for write, which cannot create the file.
2. It then renames the job to `{id}.job.taken`, and the mover writes only there.

**Why.** Writing content to a path creates the file if it is absent. Writing to a claim path
can therefore bring back a job that somebody else now holds. Mark before take: a `.taken`
file that carried the old quiet mtime would be judged abandoned by a second sweep, while the
first mover was still writing to it.

**Consequences.**
- Nothing writes to an `_in_progress` path it has not taken.
- `.taken` counts as queued.
- A sweep recovers a stale `.taken` file, judged by its own mtime alone.

## 6. Every way a job stops is an attempt

**Decision.**
- `ClaimedJob::fail` counts an attempt, and so does the sweep.
- Past `MAX_ATTEMPTS`, both send the job to `_failed`.
- A job file in `_in_progress` that won't parse goes to `_failed`, with its error saved
  beside it as `{job}.error`.

**Why.** A job that kills its worker (out of memory, a panic) never reaches `fail`. Without
counting in the sweep it would cycle forever, costing a worker each time round.

**Consequences.**
- A read that *fails*, unlike one that won't parse, is left for the next sweep: shares
  drop out.
- A retry is a person moving the file out of `_failed`.

## 7. Two workers on one input is possible

**Decision.** Nothing downstream may assume one worker per input.

**Why.** A scan's enqueue can land after a worker has claimed the same file. A worker swept
off a job can take the claim of the worker that was given it next.

**Consequences.**
- When `fail` or `complete` finds its job gone, it returns `Lost`. That is a normal
  outcome, not an error: `work` doesn't stop or wait, and leaves the heartbeat to whoever
  holds the claim now.
- The staging copy of an output is named after the worker that makes it (see 10).

## 8. Absence is judged by is_absent, not NotFound

**Decision.** `queue::is_absent` is the one definition of "not there".

**Why.** Windows reports an unreachable share (`ERROR_BAD_NETPATH`) as `NotFound`, so
checking the kind alone mistakes a work root that has dropped out for a job somebody else
took.

**Consequences.**
- A caller may conclude that a path is absent, but not *which* path was.
- `complete` doesn't create `_completed` first. If it did, a work root being cleared would
  look healthy.
- `status` reads an absent directory as empty and an unreachable one as an error. It
  skips a job file that vanished mid-read.

## 9. Jobs snapshot their settings

**Decision.** `Job::new` stores the absolute input path and the fully resolved
`QualitySettings` in the job file at scan time.

**Why.** Workers need no shared configuration.

**Consequences.** Changing `FFMPEG_CRF` doesn't affect jobs that are already queued.

## 10. The library only ever sees whole files

**Decision.** Encode into the work folder. Copy the result to `{output}.{worker}.partial`
beside the destination, then rename it into place.

**Why.** The work root and the media root are often on different volumes, so the move has
to be a copy. `work` and `scan` treat an existing output as done, so an interrupted copy
straight onto the destination would leave a truncated file nobody comes back for. A media
server would also index it.

**Consequences.**
- The staging name carries the worker id (see 7), not a per-copy id, so a worker that
  retries its move overwrites its own last `.partial`.
- A `.partial` left by a dead worker stays until a person removes it.
- `kill_on_drop(true)` makes sure a cancelled encode never runs on.

## 11. FFmpeg arguments are assembled in buckets

**Decision.** `FFmpegCommandBuilder` keeps global, input and output arguments apart and
joins them in FFmpeg's order. An input option attaches to the next input declared.

**Why.** FFmpeg's command line is positional, and options after the output are silently
dropped. Calling `with_output` before the mappings once threw away every `-map`.

**Consequences.** `.with_concat_list(l).with_input(s)` puts `-f concat` on the list alone.

## 12. Subtitle streams are mapped by name

**Decision.**
- `process_job` probes the source's subtitle streams once. Both encode paths map only the
  streams MP4 can hold, by index (`0:s:1`), never as `0:s?`.
- `BITMAP_SUBTITLE_CODECS` lists what is provably impossible. An unknown codec is mapped
  and left to FFmpeg.

**Why.** Mapping one bitmap stream into `mov_text` fails the whole job. Failing on an
unknown codec costs a rerun; dropping it would lose a track.

**Consequences.**
- A dropped stream is named in a warning, and the source is renamed rather than deleted.
- `dvb_teletext` is on the list because its decoder outputs a bitmap by default.
- A dropped `forced` track is reported separately. What should happen to it is an open
  question.
- The probe's CSV is split at most into `codec,forced[,language]`.
- Never use `-fix_sub_duration`: it never flushes the last event of a stream.

## 13. Long encodes are resumable chunks

**Decision.**
- A source longer than `MIN_CHUNKED_SECONDS` is encoded in `CHUNK_SECONDS` pieces into
  `{id}.chunks/`. Each piece is written as `{n}.ts.part` and renamed `{n}.ts` on success.
- Chunks hold only the picture. The join takes audio and subtitles from the source.

**Why.** An interrupted long encode continues instead of restarting. Encoding audio once
avoids an AAC priming frame at every boundary, which made the sound jump 21 ms back and
forth. A subtitle event that crosses a boundary would be cut in two.

**Consequences.**
- A chunk file existing is the record of progress.
- The chunk directory records its `QualitySettings` and is discarded when they don't match.
- Parking a job deletes its chunks. The sweep leaves them alone.
- A WebM's sidecar subtitle is mapped as `2:s`, without `?`.
- Guarded by `the_joined_audio_lands_where_the_source_has_it`. Its fixture runs at 25 fps,
  because at 10 fps the priming frame hides inside the reorder delay.
- Also guarded by `a_long_webm_is_chunked_and_still_carries_its_sidecar`. Its fixture is VP8
  and Opus, because chunks are cut by seeking and where a seek lands depends on the codec.

## 14. Chunk boundaries are declared, not inferred

**Decision.**
- The concat list gives every chunk a `duration` (`concat_list_entry`).
- A chunk in which FFprobe finds no video stream marks the end of the file.

**Why.**
- Chunks never end exactly where `-t` asked, and the error adds up at every boundary.
  Measured: 350 ms of drift over three hours, and nothing visible over fifteen minutes.
- The container's duration includes the audio padding, so the last planned chunk can begin
  after the final picture.

**Consequences.**
- Short tests pass whether or not this is right.
- `the_joined_output_ends_where_the_source_ends` compares against the source's *picture*,
  not its container.

## 15. Nothing shifts the output timeline

**Decision.**
- No `-avoid_negative_ts` on any output.
- Every MP4 gets `-movflags +negative_cts_offsets`.

**Why.**
- `make_zero` moves every stream forward to answer for the AAC priming and the x264
  reorder delay, which the edit list already carries. Measured: the picture started 80 ms
  late at 25 fps and 200 ms late at 10 fps.
- Players that ignore edit lists play the picture `has_b_frames` frames late. Signed
  composition offsets fix those players and leave the others unchanged.

**Consequences.**
- Guarded by `the_picture_is_in_the_timeline_and_not_only_in_the_edit_list`. Its fixture
  needs `veryfast`, because `ultrafast` has no B-frames.
- FFmpeg 6.1 (CI) delays subtitles by 23 ms when the source starts before zero.
  `the_one_pass_encode_starts_the_output_where_the_source_starts` therefore uses FLAC audio,
  so its source starts at zero.

## 16. Background priority is per platform

**Decision.** `nice -n 19` on POSIX and `IDLE_PRIORITY_CLASS` on Windows, both behind `cfg`
in `FFmpegProcessor::ffmpeg_command`.

**Why.** `nice` isn't on a Windows PATH. Wrapping the command in it there fails every job.

**Consequences.** Both branches are kept, because both kinds of worker exist.

## 17. status only reads

**Decision.** `status` moves, rewrites and creates nothing. It doesn't call `init`.
`execute` returns `QueueStatus`, and rendering it is a separate step.

**Why.** A work root that has never held a job must read as empty. That is the symptom of
the `-w` mistake, and the report says so.

**Consequences.** Staleness comes from the same `last_activity` and `STALE_AFTER` the sweep
uses.

## 18. clean asks, and a live claim needs --force

**Decision.**
- `clean` reports what each directory holds and deletes nothing until the user says yes.
- With nobody there to answer, it refuses instead of waiting.
- A claim whose worker is still checking in is refused, even with `--yes`.
- That refusal is re-planned against the disk just before deleting.

**Why.** `_completed` and `_failed` can't be rebuilt. Deleting a live claim leaves a worker
writing an output that nothing will reconcile.

**Consequences.**
- Unreadable job files are reported by their filename, not skipped. Skipping them once let a
  live claim past the refusal.
- The report and the prompt use `eprintln!`, so `RUST_LOG` can't filter them out.

## 19. A name is correct when it survives parse and render

**Decision.** `naming` parses a library-relative path into fields and renders them in one
canonical form. A path is correct when `render(parse(p)) == p`. There is no table of
accepted shapes.

**Why.** A yes/no matcher can reject a path, but can't say what it should have been.

**Consequences.**
- Destinations come only from `render`.
- Recovery may drop noise or leave a field empty, but never invents a value. What it can't
  recover is `Unresolvable`, with a reason.
- Rules apply per component, and the series directory is never renamed.
- The season number comes from the episode marker, and season 0 renders as `Specials`. An
  arc name on the season directory is kept only when that directory already agrees with the
  marker.
- Guarded by `no_path_is_renamed_twice`.

## 20. A multi-episode marker is read whole or refused

**Decision.** `S04E01-E02` is a range: `Episode.through` holds the last episode. The second
number counts only when it is `E`-prefixed and glued on. Shapes with no canonical form
(`S04E01-S04E02`, `-E02-E03`, `-E02.5`) are matched in order to be refused.

**Why.** Reading only the first number left `-E02` to become the title. That round-tripped
perfectly, and said the file held one episode when it held two.

**Consequences.** A round trip alone can't catch this. It is guarded by
`a_marker_never_stops_in_the_middle_of_a_marker`, which checks where the marker ends.

## 21. Queue order is not a destination

**Decision.**
- `naming::sort_key` orders `work --priority episode` and reads the path itself, not
  through `parse`.
- It groups by the library-relative series directory chain, root included, as a
  `Vec<String>`. Within a group, the marker decides the order.

**Why.** An order only has to be useful, and a destination has to be right. Sending paths
`parse` refuses to the back of the queue cost them their order on every run. Comparing
joined strings breaks the tree order: `/` sorts above a space, `-` and `.`.

**Consequences.** Callers compare `EpisodeSortKey`s. Its field order is the ordering.

## 22. A library root is proven, not named

**Decision.**
- `scope_for` accepts a directory named `Series`, `Anime` or `Movies` as a library root
  only if it does not itself hold two or more distinct roots (`holds_library_roots`). A
  directory that does is a media root. When no candidate is left, it falls through to the
  whole path.
- `validate --library-root` takes a stated root as given (`Scope::stated`), spelled as the
  user typed it and not canonicalised.

**Why.** One root-named child is exactly what a tree nested into itself looks like. Taking it
as a root once built season directories inside a film folder. Canonicalising put `\\?\` on
every Windows path in reports.

**Consequences.**
- An ambiguous library is refused.
- `fix` resolves destinations from `library_root`, never from `scan_path`.
- `IgnoreFilter` is built from `library_root` and loads only the rules that can reach the
  scanned subtree.
- This is the only place `naming` reads the disk.

## 23. Only fix and undo move library files

**Decision.** `fix.rs` carries out `validate --fix`:
- It rechecks each proposal against the disk just before applying it.
- It never overwrites, and never lets two sources claim one destination.
- A media file and its sidecars move together.
- It writes the plan before the first rename and the outcome after.
- It reports emptied directories and never removes them.

`undo.rs` reverses only what was applied. It refuses when:
- the file is gone,
- the original path is taken, or
- the size changed.

**Why.** The library can't be rebuilt.

**Consequences.**
- The fix record carries the media root and each moved file's size.
- An undo writes its own record, so it can be undone in turn.

## 24. stderr is for diagnostics, stdout for output

**Decision.** `tracing` writes to stderr, which is set explicitly. Stdout carries only what a
command means to print. The exit point prints errors with `{:#}`.

**Why.** A report survives a pipe, and turning up `RUST_LOG` doesn't corrupt it. `{:#}`
prints the whole `anyhow` context chain.

**Consequences.** Commands can attach `.context(...)` and trust that the cause still reaches
the user.

## 25. Environment variables are global state

**Decision.** `Config::from_env` and `QualitySettings::from_env` read `FFMPEG_PRESET`,
`FFMPEG_CRF`, `FFMPEG_AUDIO_BITRATE` and `SLEEP_INTERVAL` deep in the stack.

**Why.** It was the simplest thing that worked. Snapshotting the settings into jobs (9)
keeps the reach of these variables small.

**Consequences.** A test that sets *or reads* one is marked `#[serial(env)]`. Without the
mark, the test passes alone and fails at random under `cargo test`.

## 26. Linux CI is the oracle

**Decision.** A change is finished when its PR's CI is green. The test job runs on
`ubuntu-latest` with apt's FFmpeg.

**Why.** A green local run hid an FFmpeg version difference through nine reviewed PRs.

**Consequences.**
- An assertion about FFmpeg behaviour records the version it was measured on.
- Never weaken an assertion to pass. Narrow it or document a minimum version.

## 27. A target is data with provenance

**Decision.**
- What a client plays is a TOML envelope in `targets/`, loaded by `target/`.
- Each value is marked `observed` or `assumed`.
- `probe/` only reports what a file contains. Deciding belongs to the target.

**Why.** Probing is slow and its answers don't change, while envelopes change whenever a
client is measured. A spec-derived value once produced false failures.

**Consequences.** A value moves from `assumed` to `observed` only with a measurement against
a live Plex playback decision.

## 28. The harness is principles, and decisions live here

**Decision.** `CLAUDE.md` holds the principles, the invariants as one-liners, and a map. The
reasoning behind each invariant lives in this log.

**Why.** At 8,000 words, `CLAUDE.md` was loaded into every session and read by nobody.
Detailed rationale belongs where it is looked up when an area changes.

**Consequences.**
- A new decision is appended here, not written into `CLAUDE.md`.
- `copilot-instructions.md` and `CONTRIBUTING.md` point at `CLAUDE.md` instead of
  repeating it.

## 29. A container claim covers its whole demuxer

**Decision.** A target's containers are keyed by FFprobe's whole `format_name`, one entry
per demuxer, and matched as one string. An entry is `observed` only if every format that
demuxer reads was measured on the device. Recorded 2026-11-09.

**Why.** FFprobe names the demuxer, not the format: an MKV and a WebM are both
`matroska,webm`, and an MP4 and a QuickTime file are both `mov,mp4,m4a,3gp,3g2,mj2`.
Accepting a file when any one of those names matched let a measured MKV vouch for every
WebM, and left the LG's assumed `webm` unreachable.

**Consequences.**
- No shipped container acceptance is `observed`: neither demuxer has had every format it
  reads measured.
- An entry that is not a whole `format_name`, such as `mp4`, matches nothing, and its files
  are refused on an assumption.
- Guarded by `a_webm_on_the_lg_is_unverified` and
  `the_mp4_demuxer_passes_the_chromecast_only_on_an_assumption`.

## 30. A finished job does not keep its input out of the queue

**Supersedes** the second consequence of 2, which said `job_exists` checks all four
directories.

**Decision.** `job_exists` consults `_queue`, `_in_progress` with its `.taken` name, and
`_failed`. It never consults `_completed`. Recorded 2026-11-09.

**Why.** A finished input is kept out of a scan by its output existing (see 10) or its
source being disabled. If the output is later deleted, the file should be queued again
rather than remembered as done forever. A parked job is the opposite case: a re-scan must
not walk it back in to fail again, so moving it out of `_failed` by hand is what asks for a
retry. A job under its `.taken` name is mid-move, and counting it keeps free the name the
sweep returns it to.

**Consequences.** Guarded by `test_job_exists_covers_in_progress_jobs`,
`a_failing_job_is_retried_and_then_parked` and `a_job_held_mid_move_still_counts_as_queued`.

## 31. Only fix and undo give a library file another media name

**Supersedes** the title of 23, which said only fix and undo move library files.

**Decision.** `fix.rs` and `undo.rs` are the only code that renames a library file to a
different media name. `work` also writes to the library, through `FFmpegProcessor` in
`ffmpeg/`, in two ways only:
- `move_to_destination` adds the new output and its subtitle sidecars (see 10).
- `disable_source_files` appends `.disabled` to the source and its `.vtt`.

Recorded 2026-11-09.

**Why.** Delivering an output and retiring its source are what `work` is for. Neither puts
an existing file under a name Plex reads as something else, which is the move that can go
unnoticed (principle 6) and the one 23's safeguards exist for.

**Consequences.**
- A change that gives a library file a new media name belongs in `fix.rs`, with 23's
  rechecks and record.
- No test guards where library writes happen. Review holds it.

## 32. A file with no picture is not judged

**Decision.** `evaluate` records a missing video stream as an unverified pass, not a
refusal. The audit lists it under unverified claims as `video codec: no video stream`.

Recorded 2026-11-11.

**Why.** No target says what a client does with a file that has no picture, so a refusal
claimed a measurement nobody made (see 27). Refusing also queued a `Reencode` job, and a
re-encode cannot add a picture.

**Consequences.**
- An audio-only file counts under "Direct Plays as it is", with the gap shown. A separate
  "could not assess" bucket in `audit` would say it more plainly.
- Guarded by `a_file_with_no_video_stream_is_unassessable_rather_than_refused`.

## 33. A source with no picture is refused, not converted

**Decision.** `work` refuses a job whose source FFprobe finds no video stream in, before
FFmpeg runs. `scan --target` queues no job for such a file, because `evaluate` judges
nothing about it, its audio included (see 32).

Recorded 2026-11-11.

**Why.** Every operation maps the picture, so such a job can only fail, with FFmpeg's
reason. Mapping it as optional (`-map 0:v?`) would deliver a file with no picture and
disable its source. A picture-less file in a video library is more likely misfiled or
broken than in need of conversion, and a refused file can be fixed by hand (principle 6).

**Consequences.**
- A plain `scan` still queues one, because it queues by extension. The job is retried up
  to `MAX_ATTEMPTS` before it is parked, since the queue cannot park on a first failure.
- A file FFprobe cannot read at all is refused the same way.
- Guarded by `a_source_with_no_video_stream_is_refused_for_that` and
  `a_file_with_no_video_stream_is_not_refused_for_its_audio_either`.

## 34. A file with no picture is its own outcome

**Supersedes** the first consequence of 32.

**Decision.** `evaluate` returns `Conformance::NoPicture` for a file FFprobe finds no
picture in: audio only, or only cover art. It has no cost, reasons or unverified claims.
`audit` counts it in its own row and names each file. `scan --target` names each one and
queues none (see 33).

Recorded 2026-11-13.

**Why.** Counting it under "Direct Plays as it is" claimed a playback nobody measured
(principle 6), and nothing named the file, so nobody could act on it (principle 5).

**Consequences.**
- `Conformance::cost()` returning `None` means no job, not Direct Play. A caller that
  wants Direct Play excludes `NoPicture`, as `AuditReport::bucket` does.
- Guarded by `a_file_with_no_video_stream_is_unassessable_rather_than_refused`,
  `a_file_with_no_picture_is_named_rather_than_counted_as_direct_play` and
  `a_file_with_no_picture_is_neither_queued_nor_said_to_play`.

## 35. An observation is a Direct Play read off the device

Recorded 2026-11-13.

**Decision.**
- An `observed` value is read off a live Plex playback decision on the device, with the
  client's quality setting at Original.
- An acceptance is a Direct Play.
- A rejection says what rules out a client cap: the video stream was copied, or playback
  stalled with no session in `/status/sessions`. It goes under `rejects`, not left out.
- A limit records the highest value measured as `observed_up_to`.

**Why.** Two LG values were promoted to `observed` from sessions under the app's
"3 Mbps 720p" cap, and both were wrong. A cap can only cause a transcode, so it fakes
rejections and never acceptances. A value left out cannot tell "measured failing" from
"never tried".

**Consequences.**
- The TOMLs hold each device's evidence and point here.
- Guarded by `the_lg_is_not_claimed_to_burn_any_subtitle_in` and
  `the_measured_rejections_survive_a_round_trip_through_toml`.

## 36. Direct Play is one predicate

Recorded 2026-11-13.

**Supersedes** the first consequence of 34.

**Decision.** `Conformance::direct_plays()` is true only for `Conforms`. `cost()` returning
`None` means no job and nothing more.

**Why.** "No cost" covered both Direct Play and `NoPicture`, so every caller wanting Direct
Play had to remember to exclude `NoPicture` by name.

**Consequences.** Guarded by `a_file_with_no_picture_is_named_rather_than_counted_as_direct_play`.
