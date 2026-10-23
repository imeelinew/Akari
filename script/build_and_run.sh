#!/usr/bin/env bash
set -euo pipefail

MODE="${1:-run}"
APP_NAME="akari"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CACHE_HOME="${XDG_CACHE_HOME:-$HOME/Library/Caches}"
BUILD_DIR="$CACHE_HOME/Akari/build"
APP_BINARY="$BUILD_DIR/$APP_NAME"

case "$MODE" in
  run|--build-only)
    ;;
  *)
    echo "usage: $0 [--build-only]" >&2
    exit 2
    ;;
esac

mkdir -p "$BUILD_DIR"
BUILD_DIR="$(cd "$BUILD_DIR" && pwd -P)"
APP_BINARY="$BUILD_DIR/$APP_NAME"

(
  cd "$ROOT_DIR"
  cargo build --release --locked --target-dir "$BUILD_DIR/cargo"
  cp "$BUILD_DIR/cargo/release/$APP_NAME" "$APP_BINARY.next"
  mv -f "$APP_BINARY.next" "$APP_BINARY"
)

if [[ "$MODE" == "--build-only" ]]; then
  printf 'Built %s\n' "$APP_BINARY"
  exit 0
fi

stop_existing_build() {
  local pid executable
  local -a stopped_pids=()

  while IFS= read -r pid; do
    [[ -n "$pid" ]] || continue
    executable="$(ps -p "$pid" -o comm= 2>/dev/null || true)"
    if [[ "$executable" == "$APP_BINARY" ]]; then
      if kill -TERM "$pid" 2>/dev/null; then
        stopped_pids+=("$pid")
      fi
    fi
  done < <(pgrep -x "$APP_NAME" 2>/dev/null || true)

  if ((${#stopped_pids[@]} == 0)); then
    return
  fi

  local attempt still_running
  for ((attempt = 1; attempt <= 50; attempt++)); do
    still_running=false
    for pid in "${stopped_pids[@]}"; do
      executable="$(ps -p "$pid" -o comm= 2>/dev/null || true)"
      if [[ "$executable" == "$APP_BINARY" ]]; then
        still_running=true
        break
      fi
    done
    if [[ "$still_running" == false ]]; then
      return
    fi
    sleep 0.1
  done

  echo "$APP_NAME did not stop after SIGTERM" >&2
  exit 1
}

stop_existing_build
exec "$APP_BINARY"
