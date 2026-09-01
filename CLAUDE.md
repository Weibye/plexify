# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build                              # Debug build
cargo build --release                    # Release build (LTO, stripped, panic=abort)
cargo test                               # All tests
cargo test --lib                         # Unit tests only (in-module `mod tests`)
cargo test --test integration_tests      # Integration tests only
cargo test test_episode_prioritization   # Single test by name substring
cargo test -- --nocapture                # Show println!/stdout from tests
cargo bench                              # Criterion benchmarks (benches/validate_bench.rs)
```

### Quality gate

CI runs four jobs, and all must pass before a PR merges. Run all four locally before
declaring any task finished, and fix what fails:

```bash
cargo build
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets --all-features
```

`cargo fmt` (no args) auto-fixes formatting. Clippy warnings do not fail CI, but new ones
should not be introduced. The fourth CI job is a `rustsec/audit-check` dependency scan,
which has no local equivalent.

**Local green and CI green are not the same thing.** The test job runs on `ubuntu-latest`
with FFmpeg installed by `apt-get`, and the FFmpeg-dependent tests are sensitive to which
build they meet - `-avoid_negative_ts` does not default the same way across versions, so an
encode that starts its output where the source starts on one machine can carry a one-frame
head shift on another. The four commands above passing locally therefore says nothing about
the platform CI runs on. Check the run on the pull request before calling a change finished,
and when an assertion rests on what FFmpeg does, record which FFmpeg it was measured on.
Never weaken an assertion to turn CI green: a behaviour that cannot be made
version-independent should be narrowed, or given a documented minimum version, rather than
asserted more loosely.

### Running the CLI against real media

The binary mutates a media library in place — `work` deletes or disables source files, and
validation fixes rename them. Never point a manual test run at a real library. Build a
throwaway tree under a temp directory instead (see the `media-fixture` skill).

## Architecture

Two subsystems live in one binary. They share only the path and naming conventions; no code
path connects them.

### 1. Transcoding pipeline (`scan`/`add` → `work` → `clean`)

**The filesystem is the database.** There is no server, broker, or lockfile daemon. A job is
a JSON file, and the queue is three directories that jobs move between:

```
{work_root}/_queue/{uuid}.job  →  _in_progress/  →  _completed/
                                              ↘  _failed/
```

**A job is named after the file it transcodes.** The `{uuid}` is a v5 UUID derived from the
resolved input path (`Job::id_for_input`), not a random one, and that is what makes the queue
addressable: the same file always maps to the same job filename, so re-scanning a library is
idempotent and both primitives below have something stable to collide on. Never make the id
random again.

Any number of `work` processes on any number of machines can point at the same work root.
One job per input file comes from the name: every scanner that sees a file derives the same
v5 id for it, and `job_exists` consults all four queue directories before a scan writes
anything. What the two filesystem primitives in `src/queue/mod.rs` add is that each step
between those directories happens all at once, so no other process ever sees a half-done one:

- **Enqueue** publishes a job. It writes the file under a staging name and renames it onto
  `{uuid}.job`, which replaces whatever is there in one step. It is not a claim on the name —
  two scanners racing on one file both write, and the second rename wins — but neither can
  leave a torn file or any debris behind. A marker claiming the name instead is worse than
  nothing: a scanner that died between taking the name and writing the job would leave the
  name taken with nothing under it, and every later scan would skip a file that is then in no
  queue directory at all.
- **Claim** renames the job file from `_queue/` into `_in_progress/`. Rename is atomic, so
  exactly one worker wins; losers see `NotFound` and move on.

Preserve this property when touching the queue. Any change that reads-then-writes instead of
renaming reintroduces the race that this design exists to avoid. The same goes for rewriting
a job in place to record an attempt: `write_job_atomically` stages and renames, so a worker
killed mid-rewrite leaves a job file that still parses.

- **Take** is how a job leaves `_in_progress`, and `take_for_move` is the only way to do it.
  Recording an attempt means writing the job file, and a content write is not a rename: it
  *creates* the file when it is absent and replaces it when it is not, so writing to a claim
  path can resurrect a job somebody else now holds. A mover therefore moves the job to
  `{uuid}.job.taken` and writes only there. `reclaim_one` and `ClaimedJob::fail` both begin
  this way, and **nothing may write contents to a path in `_in_progress` it has not taken**.

`take_for_move` **marks the job before it takes it** — `mark_attended` sets the mtime through
a handle opened for write, which cannot create the file — and the order is the point, not an
implementation detail. Renaming preserves timestamps, and every job a mover takes is one that
already looked quiet, since that is what made it eligible. Take first and the `.taken` file
carries that quiet timestamp for the length of the move, so a second sweep enumerating
`_in_progress` judges the *move* abandoned, renames the file back, and leaves the first mover
writing a file it no longer owns — whose write then creates a second copy. Marking first means
a `.taken` file reads as attended from the moment it exists. A mover that dies between the two
leaves the job looking fresh for `STALE_AFTER` and the next sweep collects it: a delay, and
nothing worse.

Taking a job says only that it moved, never that its worker is gone: a claim taken a moment
ago renames just as willingly as one abandoned an hour ago, and the sweep reads the whole
directory before it moves anything. So `reclaim_one` decides staleness itself, from two
timestamps read at two different moments, and both moments matter:

- the **job file's own mtime, read before the take**, because marking overwrites it. It is
  the fallback for a job whose worker died before its first heartbeat landed, which has
  nothing else to be judged by. Read it after the mark and that job is never reclaimed —
  `a_job_claimed_without_a_heartbeat_is_still_reclaimed` covers this.
- the **heartbeat, read after the take**, because a worker writes one before it is handed its
  claim. A fresh heartbeat here is a worker that claimed the job while the sweep was still
  reading, and its job goes straight back.

The check at the top of the sweep loop only avoids disturbing obvious claims; this pair
decides. `quiet_for`, `later_of` and `modified_at` exist so this can be said in the same terms
`is_stale` uses; `is_stale` itself stays the shared definition `clean` and `status` rely on.

A job held under `.taken` counts as queued in `job_exists`, so no scan can occupy the name it
has to return to, and a sweep recovers a `.taken` file — judged by its own mtime alone — whose
mover died.

**What none of this rules out is two workers on one input**, so nothing downstream may assume
it cannot happen. A scan whose `job_exists` found nothing and whose enqueue lands after a
worker has claimed the job queues that file again while it is being encoded. `ClaimedJob::fail`
and `complete` can reach the same place from the other side: a worker swept off its job and
then re-given it to somebody else takes that worker's claim, because nothing in a job file or
beside it says which worker holds it. These windows are why the staging name a finished encode
is copied under carries the id of the worker that copied it.

**The work root is not the media root.** `JobQueue::new` takes both. `--work-dir`/`-w`
controls the queue location and **defaults to the current working directory**, not to the
media directory. This trips people up constantly: running `scan` from two different shells
with different CWDs silently produces two unrelated queues. `media_root` is retained on the
queue only for resolving legacy relative job paths. Any command that reports on a queue must
print the work root it resolved, as `status` does - otherwise the report and the mistake are
indistinguishable.

**Jobs are self-describing snapshots.** `Job::new` resolves the input path to absolute and
bakes the fully-resolved `QualitySettings` into the job file at scan time. A job queued last
week transcodes with the settings it was created with; changing `FFMPEG_CRF` afterwards has
no effect on it. Workers therefore need no shared configuration.

**FFmpeg's command line is positional, so `FFmpegCommandBuilder` assembles it in buckets.**
An option applies to the next file that follows it, and options after the output path are
silently discarded. The builder therefore keeps global, input, and output arguments apart and
joins them in FFmpeg's order at `build` time, so a caller cannot break the command by
chaining in a different order — which is exactly what happened when `with_output` was called
before the stream mappings and every `-map` was thrown away.

Input options are the exception to "order does not matter", and deliberately: they attach
to the **next input declared**, so `.with_concat_list(list).with_input(source)` puts
`-f concat` on the list alone and reads the source as an ordinary file. Holding them in one
bucket ahead of every input, as an earlier version did, silently put every option on input 0.

**A subtitle stream is carried by name, and only if MP4 can hold it.** `-c:s mov_text` is a
text encoder, and FFmpeg will not encode a picture into one, so mapping a bitmap subtitle
stream — `hdmv_pgs_subtitle` from a Blu-ray, `dvd_subtitle` from a DVD — does not lose that
stream, it fails the whole job and the video with it. `process_job` therefore probes the
source's subtitle streams once and hands the same `SubtitleSelection` to both encode paths,
which name the streams they can convert (`0:s:1`) rather than asking for the group (`0:s?`).
`BITMAP_SUBTITLE_CODECS` lists what is provably impossible, not what is known to work: an
unrecognised codec is mapped and left for FFmpeg to judge, because being wrong that way
costs a job that can be run again, and being wrong the other way loses a track from a
library. A dropped stream is named in a warning, and the source is renamed rather than
deleted, so the track can still be taken from it.

`dvb_teletext` is on that list for a different reason from the rest, and it is why the list
cannot be derived by reading `ffmpeg -codecs`: teletext is not inherently a picture, but its
only decoder emits one unless `-txt_format` says otherwise, and the default is `bitmap`.
Output format as a *decoder option* rather than a codec property is the shape to look for
before adding anything else. `arib_caption` and `eia_608` come off the same kind of broadcast
capture and both decode to text, so both are correctly absent.

**A dropped stream that is `forced` is reported differently, and that is all it is.** The
probe asks for `stream_disposition=forced` in the call it already makes, so a track holding
the translated signs a scene cannot be followed without costs nothing extra to tell apart
from a decorative transcript. Nothing acts on the difference — the track is dropped and the
job succeeds either way — because what *should* happen to a forced bitmap track is an open
decision. The detection exists so that decision has something to act on, and so a log can
distinguish a file that now plays a scene untranslated from one that lost a track nobody
asked for. Adding the disposition changed the probe's CSV to `codec,forced[,language]`;
`parse_probed_subtitle_line` stops splitting at three fields because FFprobe CSV-quotes a
language tag containing a comma, and splitting further would read its tail as a field.

Do not reach for `-fix_sub_duration` to resolve overlapping events. It holds each event back
until the next one arrives so it can bound it, so the final event of every stream is never
flushed and never reaches the output — silently, on every file. The MP4 muxer already ends a
text sample where the following one starts, which is the whole of what the flag was there
for.

**Transcoded output is written to the work folder, then moved.** `FFmpegProcessor::process_job`
encodes to `job.work_folder_output_path(work_folder)` and only then calls
`move_to_destination`. This keeps a media server from indexing a half-written `.mp4` sitting
next to the source. Preserve the write-then-move ordering.

The move itself has to be a copy, because the work root and the media root are routinely on
different volumes — so it copies to `{output}.{worker}.partial` beside the destination and
renames that onto the destination, which within one directory is atomic. The destination name
is therefore only ever taken by a whole file. That matters beyond the media server: `work` and
`scan` both treat a file at the output path as an encode that is already done, so a copy
interrupted straight onto the destination would be recorded as a finished job and leave a
truncated `.mp4` in the library that nothing ever comes back for.

The `{worker}` in that name is the id of the worker doing the copy, and it is what keeps the
guarantee from depending on the queue never handing one input to two workers — which, as
above, it cannot promise. Two workers sharing a staging name would copy into one file and each
rename the splice onto the destination, which is the same corrupt output by another route. The
id belongs to the worker rather than to the copy so that a worker retrying its own move writes
over its last part-copy. A `.partial` left by a worker that was killed is nobody's to remove —
no other worker can tell it from a copy still being made — so it sits in the library until a
person clears it out. No command reports one.

**A job that a worker stops running comes back, and a job that cannot succeed stops coming
back.** Both are properties of the same three moves, and both are easy to undo by accident:

- **A claim protects the job, and `ClaimedJob` is the claim.** `try_claim_job_file` writes a
  `{uuid}.job.heartbeat` before it returns, and the returned `ClaimedJob` owns the task that
  refreshes it every `HEARTBEAT_INTERVAL` and aborts it on drop. Both halves have to stay in
  the queue rather than in a caller. Claiming is a rename and rename keeps the mtime, so a job
  that waited out a backlog is *already* older than `STALE_AFTER` when it is claimed; if the
  first heartbeat were the caller's job, the gap before it landed would be a window in which
  two workers could hold one file. The heartbeat is a *separate* file because touching the job
  file itself risks a half-written job.
- **`JobQueue::reclaim_stranded_jobs` runs at worker startup** and renames back to `_queue/`
  any job quiet for `STALE_AFTER`. Nothing else ever looks in `_in_progress/`, so this is the
  only thing that recovers a job from a worker that was killed. Every move it makes must stay
  a rename, and the timestamps that decide staleness must be read at the moments described
  above — the job's before the take, the heartbeat's after it — or two workers can both take
  one job.
- **Every way a job can stop counts as an attempt.** `ClaimedJob::fail` counts one, and so
  does the sweep - a job that takes its worker down (an OOM on a large encode, a panic) never
  reaches `fail`, and without counting there it would cycle forever at the cost of a worker
  each time round. Past `MAX_ATTEMPTS` both paths rename into `_failed/` instead of `_queue/`.
  The count lives in the job file rather than the name, because the name is the v5 id and is
  what makes the queue addressable. `job_exists` consults `_failed/`, so re-scanning cannot
  walk a parked job back into the queue; moving the file out by hand is what asks for a retry,
  and `clean` empties it along with everything else - after saying so.
- **A file in `_in_progress` that is not a job goes to `_failed` too.** Contents that will not
  parse will not start parsing, and since `job_exists` goes by filename, a job file left there
  keeps its media file out of the queue for as long as it sits. The sweep renames it into
  `_failed` and writes the parse error beside it as `{job}.error`. A read that *fails* is left
  alone: a work root on a network share drops out now and then, and that is worth another
  sweep rather than a decision.

**`status` is the only way to read the queue, and it only reads.** `src/commands/status.rs`
walks the same four directories and reports counts, the jobs in `_in_progress` with how long
since their worker last checked in, and the jobs in `_failed` with their attempt count and
recorded error. It moves, rewrites and deletes nothing, and deliberately does not call
`init` - a work root that has never held a job must read as empty rather than be created by
the act of asking about it, because that reading is the symptom of the `-w` mistake below and
is what the report says out loud. Whether a job is stranded comes from `queue::last_activity`
and `STALE_AFTER`, the same two things the sweep uses; a report judging that on its own rule
would tell users about jobs the sweep will not reclaim. `execute` returns `QueueStatus` and
rendering is separate, so another consumer reads the state rather than parsing the text.

Reading a queue nothing is holding still means racing it, and two of the three absences
`status` can meet are not errors. A job file that has *vanished* between the listing and the
read is a worker's `complete()` doing its job, so it is skipped rather than reported as
unreadable - only a file still present and unparseable is worth a corruption signal. A queue
directory that is *absent* reads as empty, but one that cannot be *reached* is an error,
because answering an unreachable share with the `-w` advice sends a user to change the one
thing that is right. Windows makes that distinction invisible to `ErrorKind` - it reports
`ERROR_BAD_NETPATH` as `NotFound` - so `is_absent` checks the raw code there, and narrowing
it back to the kind alone silently restores the wrong answer on the platform where shared
work roots are most likely.

**`clean` still empties all four directories, and that is why it has to ask first.** The four
are not equally reconstructible: `_queue/` is rebuilt by re-running `scan`, but `_completed/`
is the only record that a file was transcoded, `_failed/` holds every parked job's attempt
count and error *and* is what keeps `job_exists` from re-queueing it, and `_in_progress/`
holds live claims. Narrowing the default set would silently change what the command has always
meant, so instead `src/commands/clean.rs` reports what is in each directory, itemising the two
whose contents nothing can rebuild, and removes nothing until the user says yes. `--only`
narrows a run, `--dry-run` prints the report and stops, `--yes` answers in advance.

Two of those refusals are not questions, and must not become questions:

- **A run with nobody to answer refuses rather than blocks.** `Confirmation::is_interactive`
  is checked before the prompt, so `plexify clean` in a script or a CI job exits with the
  reason instead of waiting on a `read_line` that will never return.
- **A live claim takes more than a `yes`.** A claim whose worker is still checking in - judged
  by `queue::is_stale` and `STALE_AFTER`, the same rule the sweep uses, so the two can never
  disagree about which claims are held - is refused outright, and `--yes` does not cover it.
  `--force` is separate for exactly this reason: deleting a claim mid-encode leaves a worker
  writing an output nothing will reconcile, which is a corruption rather than a cleanup.

**The live-claim refusal runs again, against the disk, immediately before the deletion**, for
the same reason `src/fix.rs` rechecks every proposal before applying it: the report was read
before the user was asked, and the gap is however long a person spends reading it, so a worker
claiming a job inside that window is routine rather than a race. The other counts in the report
are a floor - whatever arrives in the window is deleted too - and that is accepted, because
`_queue` is rebuilt by `scan` and the other two are what the user consented to losing. Only the
live claim is catastrophic, so only the live claim is re-read. The recheck must stay a re-plan
rather than a second walk of its own: two implementations of "live" that can disagree are not a
safeguard.

**A job file that will not parse is still a job file that is about to be deleted.** In
`_failed` that is a parked job somebody needs to see before emptying the directory - a
documented state once `quarantine_unreadable` puts one there deliberately - and in
`_in_progress` it is a claim, because liveness comes off the job file's and the heartbeat's
timestamps and needs no parse. Skipping unreadable jobs is how a live worker's claim walked
past the refusal: it never became a claim, so there was nothing to refuse. They are reported by
their own filename, which is the v5 id and so is stable, and nothing is invented for the fields
that could not be read.

The report and the prompt are `eprintln!` to stderr, not `tracing`. `clean` has no
machine-readable stdout to protect, the report exists to be read next to the question it is
asking, and a confirmation prompt that `RUST_LOG` can filter out is a prompt that can go
missing.

Whatever an interrupted encode left in the work folder is left alone by the sweep. Deciding
what of it is still usable belongs to the encoder, not the queue. What must *not* be left
behind is a live process: `kill_on_drop(true)` on the FFmpeg command is what stops a
cancelled encode from running on and writing the same work-folder path as the worker that
later reclaims the job.

**A long encode is resumable, and that is what the chunk directory in `_in_progress/` is.**
A source over `MIN_CHUNKED_SECONDS` is encoded in `CHUNK_SECONDS` pieces into
`{job-id}.chunks/`, each written as `{n}.ts.part` and renamed to `{n}.ts` only after FFmpeg
exits successfully. A chunk file existing is therefore the whole record of progress —
`encode_chunks` skips what is already there — so nothing may create one by any other route.
The pieces are transport streams because that is the container built to be concatenated;
separately encoded MP4s each carry their own encoder delay in an edit list the demuxer does
not read. `reclaim_stranded_jobs` leaves the chunk directory alone for exactly this reason.

**A chunk holds the picture and nothing else.** The audio and the subtitles are both taken
from the source at the join, and for nearly the same reason. A subtitle event straddling a
boundary would be cut in half by it; and one FFmpeg run per chunk is one AAC priming frame
per chunk, which MPEG-TS — having no edit list and no negative timestamp — can only carry by
spending real time on it. Whether that frame falls inside the length the concat list declares
for its chunk then varies from chunk to chunk, so the sound steps one AAC frame, 21ms at
48kHz, back and forth at every boundary while the picture stays where it belongs. Measured
on a 30s source in 5s pieces: alternating runs of +0.2ms and +21.2ms. Encoding the audio once
over the whole file is one priming frame in front of an MP4 that has the edit list to hold
it — which is exactly what the one-pass path already produces, and is why the two paths now
put the sound in the same place. `the_joined_audio_lands_where_the_source_has_it` is what
keeps that true; its fixture is 25fps rather than the 10fps the others use, because at 10fps
a frame interval is five times an AAC frame and the priming disappears inside the picture's
own reorder delay.

The audio channel cap therefore rides on the join rather than on the chunks. Do not put an
encoder back in the chunk command to save the join a pass over the source: the join already
reads that file for its subtitles, so the audio costs no extra I/O, and it is the only place
the output's sound can be written once.

**The concat list declares each chunk's length, and that is load-bearing.** A chunk never
ends exactly where `-t` asked it to, because video cuts on a frame boundary and audio on a
1024-sample AAC frame boundary. Without a `duration` line the demuxer starts each chunk
where the previous one actually ended, and since the error is *per boundary* it compounds:
measured on a three-hour source, the picture ended 350ms behind where it belonged, while a
fifteen-minute episode crossing two boundaries showed nothing at all. Any test short enough
to be comfortable will therefore pass whether or not this is right - `concat_list_entry` is
the only thing keeping it right.

**The plan divides up the container's duration, and a chunk has to produce video to be
joinable.** A container's duration is the longest of its streams, so it outlives the last
video frame by the audio encoder's padding - and `plan_chunks` rounds up, so the final chunk
can begin after the picture has ended. That chunk encodes happily and comes out a few hundred
bytes long with no video stream in it at all; joined in, it leaves the MP4's video track
running a whole frame interval past its own last picture. `encode_chunks` therefore treats a
chunk that **FFprobe can find no video stream in** as the end of the file.

A source whose sound outlives its image loses nothing to that. The audio is not in the chunks,
so the join reads it from the source in full however far the picture got - and the joined
output then ends where the *source* ends, which is what
`the_joined_output_ends_where_the_source_ends` measures. That test compares the joined picture
against the source's **picture**, not against its container: an MKV declares no per-stream
duration and its container outlives its last frame by the audio encoder's padding, so
comparing the two would leave a whole AAC frame of room for a fault to sit in.

**Nothing shifts the output timeline to keep it off zero.** Two different things put a negative
timestamp in front of an MP4 mux, and MP4's edit list is the field built to carry both. An AAC
encoder emits a priming frame before the content begins, so the one-pass encode's first audio
packet is one frame early. And x264 holds frames back to reorder them, so the concat demuxer
feeding the join hands over video whose first DTS is a reorder delay before its PTS - the
join's input is *not* already non-negative, and assuming it was is what hid this.

`-avoid_negative_ts make_zero` answers the same question a second time, by moving *every*
stream forward until nothing is negative. On the one-pass encode that is one AAC frame. On the
join it is the reorder delay, so the picture ends up starting at that delay rather than where
the chunks put it - 0.080s at 25fps, 0.200s at 10fps, 0.500s at 4fps, one measurement each.
Both cases drag the subtitles along too and leave a sliver of an event where the first one used
to be. So it belonged on neither, and is on neither.

Only on the chunks was it ever inert: MPEG-TS cannot carry a negative timestamp, so FFmpeg
shifts it there by default and the chunk files are byte-identical either way. The shift is
itself written as an edit rather than by moving samples, so a player that ignores edit lists
saw the same file with or without the flag - which is how it sat in the join unnoticed. That is
not a reason to put it back on a fourth output: a concat-fed MP4 mux is exactly where it does
the most damage.

**The edit list is where a delay is *expressed*, and it must not be the only place.** Left to
itself the MP4 muxer answers the reorder delay with the video track's `elst` `media_time` and
writes nothing about it into the media timeline, so the output is in sync only on a player
that reads `elst` - and edit-list handling is inconsistent across hardware decoders and
set-top clients. One that skips it plays the picture `has_b_frames` frame intervals behind the
sound: measured 80ms at 25fps and 200ms at 10fps, on all three of the MP4s this project
writes. `with_negative_composition_offsets` therefore puts `-movflags +negative_cts_offsets`
on every one of them, which says the same thing as a signed composition offset in the sample
table instead.

It is not a second answer competing with the first, which is what separates it from
`-avoid_negative_ts`: measured, the edit-list-honouring reading of an output is unchanged by
it - same first video packet, same first audio packet - while the edit-list-ignoring reading
comes back to zero. It corrects the players that were wrong and leaves the ones that were
right alone, and `the_picture_is_in_the_timeline_and_not_only_in_the_edit_list` asserts both
halves of that on both encode paths. Its fixture has to be built by a job asking for
`veryfast`: `chunking_job` asks for `ultrafast`, x264 turns B-frames off there, and a source
with no reorder delay passes this test whatever the muxer was told.

`-movflags` accumulates across repeats rather than replacing, so a remux still gets its
`+faststart` as well.

That claim is about what plexify's own command line asks the muxer for, and it stops there.
FFmpeg 6.1, which is what `apt` installs on the Ubuntu CI runs, offsets an entire input
forward when the container declares a `start_time` before zero - as an MKV with AAC audio
does, by the priming frame. Video and audio come back to zero through the filter graph; a
transcoded subtitle stream never enters one, so it keeps the offset and lands 23ms late with
`mov_text` filling the head. Nothing on the command line answers that - it is unchanged by
every `-avoid_negative_ts` value, by `-muxdelay`, `-max_interleave_delta`,
`+negative_cts_offsets`, and by dropping the audio track altogether - and FFmpeg 9.0 does not
do it at all. So a fixture measuring the muxer has to start at zero itself, which is why
`the_one_pass_encode_starts_the_output_where_the_source_starts` builds its source with FLAC
audio and asserts that it did.

Two things follow from the directory being named after the job id, which is the v5 UUID of
the input path and so stable forever. A parked job's chunks would be found again by any later
job for the same file, so the chunk directory records the `QualitySettings` it was filled
with and `prepare_chunk_dir` discards anything that does not match — half an output at one
CRF and half at another, with nothing recording it, is worse than redoing the work. And
because `job_exists` consults `_failed/`, nothing will ever come back for a parked job at
all, so `discard_work` removes its chunks when it is parked rather than leaving them to
occupy the size of the finished output indefinitely.

**De-prioritising a background worker has no portable spelling.** `nice` is a POSIX utility
and is not on a Windows PATH, so wrapping the command in it there does not lower FFmpeg's
priority — it fails to spawn, and the job fails on every retry. `FFmpegProcessor::ffmpeg_command`
keeps the two behind `cfg`: `nice -n 19` off Windows, `IDLE_PRIORITY_CLASS` as a creation flag
on it. Both worker nodes matter; do not collapse this back to one branch.

### 2. Library validation (`validate`)

`src/naming/` owns what a correct path looks like, and `validate` is a caller. The module
**parses** a library-relative path into fields and **renders** those fields back into one
canonical form; a path is correct exactly when rendering its own parse reproduces it. There
is no table of accepted shapes, and adding one would reintroduce the problem this design
exists to avoid: a yes/no matcher can reject a path but cannot say what it should have been,
so a destination ends up patched together from the source string.

Consequences to preserve when changing it:

- A destination must always come out of `render`, never out of string surgery on the input.
- Anything `render` emits must parse back to the same fields, or a fix would move a file
  twice. There is a test for this; keep it passing.
- Recovery heuristics may drop what they are confident is noise and may leave a field empty,
  but must never invent a value. Unrecoverable paths return `Unresolvable` with a reason.
- Rules apply per path component, and a component with no rule is preserved. The series
  directory is not renamed.
- The season directory is the exception: its **number** comes from the episode's own marker,
  not from the directory. A file with no season directory is moved into one, a misfiled file
  moves across, and season zero renders as `Specials`. An arc name already on that directory
  (`Season 02 - The Mighty Nein`) is kept, because Plex reads the season from the filename
  marker and the arc name is curated information - but only where the directory already
  agrees with the marker, since a file moving seasons cannot carry the old arc name along.

**A run can be narrowed without changing what canonical means.** `naming::scope_for` splits
the path a user gave into a `library_root` (the parent of the outermost `Series`/`Anime`/
`Movies` component *that is actually a root* — see below) and a `scan_path` (what to walk).
Validation walks the scan path and judges every file against the root, because a path
starting `Season 06/` names no series and would be unresolvable.

**A component's name does not establish that it is a library root, and only a directory
holding two of them settles it.** `/srv/Anime` can equally be a media root that *holds*
`Series/`, `Anime/`, `Movies/`, or the Anime root itself with series directories directly
inside it. Taking the name at face value reads the first as the second, and then every file
below looks like a tree nested into itself and the whole library becomes unresolvable at once
— `validate --fix` inert on all of it. `scope_for` therefore lists each candidate directory
and skips it when it holds **more than one distinct root** (`holds_library_roots`), falling
through to the whole path when no candidate survives. This is the only place `naming` reads a
disk; keep it out of parse/render, and keep the count where it is:

- **One root-named child is not evidence, because it is exactly what `DuplicatedRoot`
  reports.** `lib/Movies/Series/` is either a media root holding one library or a film
  directory called `Series`, and `lib/Series/Series/` is either that or a tree rsynced into
  itself. Reading either as a media root pushes `library_root` a level in, and the damage is
  an *action*, not a message: the film directory then reads as a `Series` library, an episode
  inside it earns a canonical destination, and `--fix` builds a season directory inside a film
  folder. `fix.rs` cannot catch it — the destination came out of `render`; the root beneath it
  is what is wrong. Leave that case to `parse`.
- **Two distinct roots is unambiguous**, because no one library contains another.
- **The residual cost is a refusal, and that is the right way to be wrong.** A media root
  named after a root that holds exactly one — `/srv/Movies` containing only `Movies/` — is
  still refused, as are an unreadable directory and a stray *file* named `Series`. Refusing a
  library is recoverable by hand; a file moved to a wrong root is recoverable only through
  `undo`, and only if someone notices.

Two further consequences that are easy to break:

- `fix` resolves destinations from `library_root`, never `scan_path`. A destination routinely
  falls outside the scanned subtree — correcting `Season 6` to `Season 06` moves a file into
  a sibling directory.
- `IgnoreFilter` is built from `library_root`, so scoping into a directory cannot override a
  `.plexifyignore` rule written at the root. `for_scope` loads only the rules that can reach
  the scanned subtree - those above it and inside it - so a narrow run stays narrow instead of
  walking the library to find rules that could never match.

Paths are matched **relative to the media root, with `/` separators**. On Windows they must
go through `crate::paths::to_forward_slashes` first, or every component comparison is wrong.

`validate` reports a destination for each file and changes nothing. `validate --fix` carries
those renames out, and `src/fix.rs` is the only code in the project that moves a file in the
library. Its rules exist because it runs against a library nobody can reconstruct:

- Every proposal is rechecked against the disk immediately before it is applied; the report
  was computed earlier and the library may have moved on.
- A destination is never overwritten, and two sources are never allowed to claim one
  destination. Both are refused and reported rather than resolved by guessing.
- A media file and the files named after it - subtitles, `.nfo` - move as a group or not at
  all, so a rename cannot silently break the `.vtt` pairing `work` depends on.
- The plan is written to disk before the first rename, then rewritten with the outcome, so
  an interrupted run leaves a record of what it intended and how far it got.
- Directories the run empties are reported, never removed.

`src/undo.rs` reverses a run from that record, and is the only other code that moves a
library file. It reverses only what the record says was *applied*, checks every reversal
against the disk first, and refuses on three counts: the file is gone from where the fix put
it, something else now holds the original path, or the file's size no longer matches what was
recorded. The record therefore has to carry the media root and the size of each moved file -
without them it is a log, not something that can be acted on. An undo writes its own record,
so it is reversible in turn.

### Cross-cutting

**`.plexifyignore`** (`src/ignore.rs`) is gitignore-like, supports nesting, and child patterns
take precedence over parent ones. `IgnoreFilter` exposes two methods and both matter for
performance: `should_ignore(path)` filters individual files, while `should_skip_dir(path)`
prunes whole subtrees during traversal so ignored directories are never walked. Use
`should_skip_dir` in any new directory walk.

**Environment variables are read as global state.** `Config::from_env` and
`QualitySettings::from_env` call `std::env::var` deep in the call stack rather than receiving
injected config. Consequence for tests: any test that sets, clears, or *reads* an
FFMPEG-prefixed variable must hold `ENV_TEST_MUTEX` (`src/job/mod.rs`), because Rust runs
tests in parallel threads of one process and env vars are process-wide. A test that reads
these variables without the mutex will pass alone and fail intermittently under `cargo test`.

Recognised variables: `FFMPEG_PRESET`, `FFMPEG_CRF`, `FFMPEG_AUDIO_BITRATE`, `SLEEP_INTERVAL`,
`RUST_LOG`.

**Async is tokio throughout**; errors are `anyhow::Result<T>`; logging is `tracing`, initialised
in `main.rs` with the `plexify=info` default filter.

**The two streams carry different things, and `main.rs` is where that is decided.** Logs are
diagnostics and go to **stderr**; **stdout** carries only what a command deliberately prints,
so a report survives being piped. `fmt::layer()` defaults to stdout, so the writer is stated
explicitly and must stay stated - dropping it puts every log line back in with the report, and
`RUST_LOG` then corrupts the output it was turned up to investigate. The exit point prints a
failure with `{:#}`, which renders an `anyhow` context chain in full; a command may therefore
attach `.context(...)` and trust that the cause underneath still reaches the user, which the
`{}` form silently discarded.

## Conventions

- Every command is a struct in `src/commands/` with `new(...)` and an `async execute()`, wired
  into the `Commands` enum in `src/main.rs`.
- `src/lib.rs` re-exports the modules so integration tests and benches can use them; a module
  must be listed there and in `main.rs` to be visible to both.
- Use `PathBuf`/`Path` for all filesystem work, never string concatenation.
- Integration tests build isolated trees with `tempfile::TempDir`. Tests that depend on
  process-wide state use `serial_test`.

## Pull requests

Transient information — "X is now faster", "this fixes the bug from last week" — belongs in
the PR description only. Code, comments, tests, and documentation must describe the current
state of the code and stay valid for its lifetime. Keep PRs short and focused.
