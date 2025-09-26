#!/bin/bash
#
# plexify - A simple, distributed media transcoding CLI
#

# --- Script Best Practices ---
# Exit on error, treat unset variables as errors, and propagate pipeline errors.
# set -euo pipefail

# --- Configuration via Environment Variables ---
# Allow ffmpeg settings to be overridden externally.
FFMPEG_PRESET="${FFMPEG_PRESET:-veryfast}"
FFMPEG_CRF="${FFMPEG_CRF:-23}"
FFMPEG_AUDIO_BITRATE="${FFMPEG_AUDIO_BITRATE:-128k}"
SLEEP_INTERVAL="${SLEEP_INTERVAL:-60}"

# --- COMMAND: scan ---
# Scans a directory for .webm files and creates job files in the queue.
scan_jobs() {
  local MEDIA_ROOT="$1"

  if [[ -z "$MEDIA_ROOT" || ! -d "$MEDIA_ROOT" ]]; then
    echo "Error: You must provide a valid directory to scan." >&2
    echo "Usage: $0 scan /path/to/media/root" >&2
    return 1
  fi

  local QUEUE_DIR="${MEDIA_ROOT}/_queue"
  mkdir -p "$QUEUE_DIR"

  echo "🔎 Scanning directory: $MEDIA_ROOT"

  local job_count=0
  pushd "$MEDIA_ROOT" > /dev/null || return


  # Find .webm and .mkv files
  mapfile -d '' webm_list < <(find . -type f -name "*.webm" -print0)
  mapfile -d '' mkv_list < <(find . -type f -name "*.mkv" -print0)

  echo "Found ${#webm_list[@]} .webm files and ${#mkv_list[@]} .mkv files. Now creating jobs..."

  # Process .webm files (require .vtt)
  for webm_file in "${webm_list[@]}"; do
    local relative_path="${webm_file#./}"
    local mp4_file="${relative_path%.webm}.mp4"
    local vtt_file="${relative_path%.webm}.vtt"
    local job_name

    if [[ ! -f "$vtt_file" ]]; then
      echo "⚠️ SKIPPING: Missing subtitle file for '$relative_path'." >&2
      continue
    fi

    job_name=$(basename "$relative_path" .webm)
    if [[ -f "$mp4_file" || -f "${QUEUE_DIR}/${job_name}.job" ]]; then
      continue
    fi
    if mkdir "${QUEUE_DIR}/${job_name}.lock" 2>/dev/null; then
      echo "➕ Queueing job for: $relative_path"
      echo "$relative_path" > "${QUEUE_DIR}/${job_name}.job"
      rmdir "${QUEUE_DIR}/${job_name}.lock" || true
      ((job_count++))
    fi
  done

  # Process .mkv files (embedded subs)
  for mkv_file in "${mkv_list[@]}"; do
    local relative_path="${mkv_file#./}"
    local mp4_file="${relative_path%.mkv}.mp4"
    local job_name

    job_name=$(basename "$relative_path" .mkv)
    if [[ -f "$mp4_file" || -f "${QUEUE_DIR}/${job_name}.job" ]]; then
      continue
    fi
    if mkdir "${QUEUE_DIR}/${job_name}.lock" 2>/dev/null; then
      echo "➕ Queueing job for: $relative_path (embedded subs assumed)"
      echo "$relative_path" > "${QUEUE_DIR}/${job_name}.job"
      rmdir "${QUEUE_DIR}/${job_name}.lock" || true
      ((job_count++))
    fi
  done

  popd > /dev/null || return
  echo "✅ Scan complete. Added $job_count new jobs to the queue."
}

# --- COMMAND: work ---
# Processes jobs from the queue in a given media root.
process_jobs() {
  local MEDIA_ROOT=""
  local FFMPEG_CMD=(ffmpeg)
  local MODE="Power Worker (Foreground)"
  local DETACHED=0

  # Argument parsing loop
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --background)
        FFMPEG_CMD=(nice -n 19 ionice -c 3 ffmpeg)
        MODE="Low Priority Worker"
        shift
        ;;
      --detached)
        DETACHED=1
        shift
        ;;
      -*)
        echo "Unknown option: $1" >&2; return 1
        ;;
      *)
        if [[ -z "$MEDIA_ROOT" ]]; then
          MEDIA_ROOT="$1"
        fi
        shift
        ;;
    esac
  done

  if [[ -z "$MEDIA_ROOT" || ! -d "$MEDIA_ROOT" ]]; then
    echo "❌ Error: You must provide a valid media root directory." >&2
    echo "Usage: $0 work /path/to/media/root [--background]" >&2
    return 1
  fi

  echo "✅ Starting worker in ${MODE}${DETACHED:+ (Detached)} mode."
  echo "Watching for jobs in: ${MEDIA_ROOT}/_queue"

  local QUEUE_DIR="${MEDIA_ROOT}/_queue"
  local IN_PROGRESS_DIR="${MEDIA_ROOT}/_in_progress"
  local COMPLETED_DIR="${MEDIA_ROOT}/_completed"
  mkdir -p "$QUEUE_DIR" "$IN_PROGRESS_DIR" "$COMPLETED_DIR"

  # If detached, re-exec in background and exit parent
  if [[ "$DETACHED" -eq 1 ]]; then
    echo "🧑‍💻 Detaching worker to run in background..."
    local extra_args=()
    if [[ "$MODE" == "Low Priority Worker" ]]; then
      extra_args+=(--background)
    fi
    nohup "$0" work "$MEDIA_ROOT" "${extra_args[@]}" > "${MEDIA_ROOT}/_worker.log" 2>&1 &
    echo "Worker started in background. Log: ${MEDIA_ROOT}/_worker.log"
    return 0
  fi

  SHUTDOWN_REQUESTED=0
  trap 'SHUTDOWN_REQUESTED=1' SIGINT SIGTERM

  # Main processing loop
  while true; do
    local job_file=""

    # Atomic job claiming to prevent race conditions
    for potential_job in "${QUEUE_DIR}"/*.job; do
      [[ -e "$potential_job" ]] || continue

      job_name=$(basename "$potential_job")
      if mv "$potential_job" "${IN_PROGRESS_DIR}/${job_name}"; then
        job_file="${IN_PROGRESS_DIR}/${job_name}"
        echo "➡️ Claimed job: $job_name"
        break
      fi
    done

    if [[ -n "$job_file" ]]; then
      local relative_path
      relative_path=$(<"$job_file")

      local webm_file="${MEDIA_ROOT}/${relative_path}"
      local ext="${relative_path##*.}"
      local input_file="$webm_file"
      local mp4_file
      local ffmpeg_args=()
      if [[ "$ext" == "webm" ]]; then
        local vtt_file="${input_file%.webm}.vtt"
        mp4_file="${input_file%.webm}.mp4"
        echo "🚀 Starting conversion for: $input_file (using .vtt)"
        ffmpeg_args=(
          -fflags +genpts
          -avoid_negative_ts make_zero
          -i "$input_file"
          -i "$vtt_file"
          -map 0:v:0 -map 0:a:0 -map 1:s:0
          -c:v libx264 -preset "$FFMPEG_PRESET" -crf "$FFMPEG_CRF"
          -c:a aac -b:a "$FFMPEG_AUDIO_BITRATE"
          -c:s mov_text
          -y "$mp4_file"
        )
      elif [[ "$ext" == "mkv" ]]; then
        mp4_file="${input_file%.mkv}.mp4"
        echo "🚀 Starting conversion for: $input_file (using embedded subs)"
        ffmpeg_args=(
          -fflags +genpts
          -avoid_negative_ts make_zero
          -fix_sub_duration
          -i "$input_file"
          -map 0:v:0 -map 0:a:0 -map 0:s:0
          -c:v libx264 -preset "$FFMPEG_PRESET" -crf "$FFMPEG_CRF"
          -c:a aac -b:a "$FFMPEG_AUDIO_BITRATE"
          -c:s mov_text
          -y "$mp4_file"
        )
      else
        echo "❌ Unknown file extension: $ext. Skipping."
        mv "$job_file" "${QUEUE_DIR}/${job_name}"
        continue
      fi

      # Use a subshell for the command to trap the exit code cleanly
      if (set -x; "${FFMPEG_CMD[@]}" "${ffmpeg_args[@]}"); then
        echo "✅ Conversion successful."
        mv "$job_file" "${COMPLETED_DIR}/${job_name}"
        # Rename input and subtitle files to .disabled after successful conversion
        mv "$input_file" "${input_file}.disabled"
        if [[ "$ext" == "webm" ]]; then
          mv "$vtt_file" "${vtt_file}.disabled"
        fi
      else
        echo "❌ Conversion FAILED. Moving back to queue."
        mv "$job_file" "${QUEUE_DIR}/${job_name}"
        sleep 10 
      fi

      # After finishing the job, check for shutdown
      if [[ "$SHUTDOWN_REQUESTED" -eq 1 ]]; then
        echo "🛑 Shutdown requested. Exiting after current job."
        break
      fi
    else
      echo "💤 No jobs found. Sleeping for ${SLEEP_INTERVAL} seconds."
      sleep "$SLEEP_INTERVAL"
      # If shutdown requested and no job is running, exit immediately
      if [[ "$SHUTDOWN_REQUESTED" -eq 1 ]]; then
        echo "🛑 Shutdown requested. No jobs running. Exiting."
        break
      fi
    fi
  done
}

# --- COMMAND: clean ---
clean_up() {
  local MEDIA_ROOT="$1"
  local QUEUE_DIR="${MEDIA_ROOT}/_queue"
  local IN_PROGRESS_DIR="${MEDIA_ROOT}/_in_progress"
  local COMPLETED_DIR="${MEDIA_ROOT}/_completed"

  echo "🧹 Cleaning up temporary files..."
  rm -rf "$IN_PROGRESS_DIR"
  rm -rf "$COMPLETED_DIR"
  rm -rf "$QUEUE_DIR"
}

# --- Main CLI Router ---
show_usage() {
  # --- CHANGE: Renamed CLI ---
  echo "plexify - A simple, distributed media transcoding CLI"
  echo "Usage: $0 <command> [options]"
  echo ""
  echo "Commands:"
  echo "  scan <path>          Scan a directory for .webm files to create jobs."
  echo "  work <path> [--bg]   Process jobs from the queue. --bg for low-priority."
  echo "  clean <path>         Remove all temporary files."
  echo ""
}

main() {
  if [[ $# -eq 0 ]]; then
    show_usage
    exit 1
  fi

  local command="$1"
  shift

  case "$command" in
    scan)
      scan_jobs "$@"
      ;;
    work)
      process_jobs "$@"
      ;;
    clean)
      clean_up "$@"
      ;;
    *)
      show_usage
      exit 1
      ;;
  esac
}

main "$@"
