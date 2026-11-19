#!/usr/bin/env bash
# The testsrc/run action's one script (action.yml calls it twice, around actions/cache):
#   run.sh resolve   which snapshot (testsrc.lock, or the latest, written to the lock), which runner for this
#                    job's OS (GET /v1/runner), whether to cache the media, and the cache folders: step outputs
#   run.sh run       download the runner unless cached (checked against its SHA-256), run it, annotate failures,
#                    add the lock note to the job summary, and fail the job on failures unless report-only
# A fork's pull request, or Dependabot's, has no secrets: without the token it gets a notice and is skipped. Any other
# run without the token fails (a missing or misnamed secret), pull_request_target too (it has the secrets).
# The runner must be 0.2.0 or later (--discard, results kept when the API stops a run).
# set: all needs a company token. The media is cached only for private repositories and sets smaller than all;
# otherwise each case's files are deleted once played (--discard), so the disk holds one case at a time.
# The token goes to curl on stdin, never on a command line.
# Needs bash, curl, jq and sha256sum: all on GitHub's Linux and Windows runners (Git Bash on Windows).
# Outside Actions, set GITHUB_OUTPUT, GITHUB_STEP_SUMMARY, RUNNER_OS, RUNNER_ARCH and RUNNER_TEMP yourself.
set -euo pipefail

API=${TESTSRC_API:-https://testsrc.com/v1}
MIN_RUNNER=0.2.0   # the first runner with --discard and results kept when the API stops a run
API=${API%/}
: "${RUNNER_TEMP:=${TMPDIR:-/tmp}}" "${GITHUB_OUTPUT:=/dev/null}" "${GITHUB_STEP_SUMMARY:=/dev/null}"

die() { echo "::error title=testsrc::$*"; exit 2; }
out() { echo "$1=$2" >> "$GITHUB_OUTPUT"; }
need() { command -v "$1" >/dev/null || die "testsrc/run needs $1 on the runner"; }
# a path that both bash and the Node actions (actions/cache) read the same way, on Windows too
path() { if command -v cygpath >/dev/null; then cygpath -m "$1"; else printf '%s\n' "$1"; fi; }
# GET from the API with the token, into $REPLY (not $(get ...): die has to end the script, not a subshell). On an
# HTTP error, the API's own message.
get() {
  local code
  REPLY=$(auth | curl -sS -H @- -w '\n%{http_code}' "$API$1") || die "can't reach $API$1"
  code=${REPLY##*$'\n'}; REPLY=${REPLY%$'\n'*}
  [ "$code" = 200 ] || die "$code from $API$1: $(jq -r '.error // empty' <<<"$REPLY" 2>/dev/null || true)"
}
words() { tr -s ' \t\r\n' '\n' <<<"$1" | sed '/^$/d'; }
auth() { printf 'Authorization: Bearer %s\n' "$TESTSRC_TOKEN"; }

resolve() {
  need curl; need jq; need sha256sum
  if [ -z "${TESTSRC_TOKEN:-}" ]; then
    if [ "${GITHUB_EVENT_NAME:-}" = pull_request ] && { [ "${IN_FORK:-}" = true ] || [ "${IN_ACTOR:-}" = "dependabot[bot]" ]; }; then
      echo "::notice title=testsrc::No token on this pull request (GitHub gives pull requests from forks and Dependabot no secrets), so testsrc didn't run. It runs on the branch once merged."
      out skip true
      return
    fi
    die "no token: add a repository secret with a token from https://testsrc.com/account and pass it as token"
  fi
  out skip false
  case "${IN_PLAYER:-libvlc}" in libvlc|ffmpeg|gstreamer|mpv|cmd) ;; *) die "player must be libvlc, ffmpeg, gstreamer, mpv or cmd, not ${IN_PLAYER}" ;; esac

  # the snapshot: an input, the lock file, or the latest (then written to the lock file)
  local snap="${IN_SNAPSHOT:-}" lock="${IN_LOCK:-testsrc.lock}" note=""
  if [ -z "$snap" ] && [ -f "$lock" ]; then
    snap=$(sed -n 's/^snapshot *= *"\{0,1\}\([0-9][0-9.-]*\)"\{0,1\}.*/\1/p' "$lock" | head -1)
    [ -n "$snap" ] || die "$lock has no line like: snapshot = \"2026-11-06\""
    note="Snapshot $snap from \`$lock\`."
  elif [ -z "$snap" ] || [ "$snap" = latest ]; then
    get /snapshots; snap=$(jq -r '.latest // empty' <<<"$REPLY")
    [ -n "$snap" ] || die "$API/snapshots names no latest snapshot"
    if [ -z "${IN_SNAPSHOT:-}" ]; then
      printf '# The testsrc snapshot this repository tests against, written by testsrc/run. Commit it to keep\n# every run on the same files; the monthly update job moves it to the next snapshot.\nsnapshot = "%s"\n' "$snap" > "$lock"
      note="Snapshot $snap, the latest, written to \`$lock\`: commit that file to keep runs on this snapshot."
    else
      note="Snapshot $snap, the latest (\`snapshot: latest\`; \`$lock\` untouched)."
    fi
  fi
  # named by its day only, as the API takes it (no <date>.N); the lock parser keeps any dots, so they are refused here
  [[ $snap =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] || die "snapshot must be a date like 2026-11-06, or latest, not $snap"

  # every case is for company tokens: an evaluation token stops at 30 distinct cases, an open-source one at its
  # daily bytes, long before the catalog's end
  local set="${IN_SET:-starter}"
  if [ "$set" = all ]; then
    get /usage
    local plan; plan=$(jq -r '.plan // empty' <<<"$REPLY")
    case "$plan" in
      company|internal) ;;
      *) die "set: all needs a company token; this one is ${plan:-unknown}. Use starter or a list of cases (an evaluation token can fetch 30 different cases)." ;;
    esac
  fi
  # the media in actions/cache only where nobody else can restore it (a private repository) and where it fits
  # (not all); otherwise nothing is kept and the runner deletes each case's files once played
  if [ "${IN_PRIVATE:-}" = true ] && [ "$set" != all ]; then out media-cache true; out discard false; else out media-cache false; out discard true; fi

  # the runner for this job's OS
  local ver="${IN_RUNNER_VERSION:-latest}" m target
  if [ "$ver" = latest ]; then get /runner; else get "/runner/$ver"; fi
  m=$REPLY
  case "${RUNNER_OS:-$(uname -s)}/${RUNNER_ARCH:-$(uname -m)}" in
    Linux/X64|Linux/x86_64) target=$(jq -r '[.files[].target | select(. == "x86_64-unknown-linux-gnu")][0] // empty' <<<"$m") ;;
    Windows/X64) target=$(jq -r '[.files[].target | select(startswith("x86_64-pc-windows-"))][0] // empty' <<<"$m") ;;
    *) target="" ;;
  esac
  [ -n "$target" ] || die "no testsrc runner for ${RUNNER_OS:-$(uname -s)} ${RUNNER_ARCH:-$(uname -m)} yet (runner $(jq -r .version <<<"$m") is built for: $(jq -r '[.files[].target] | join(", ")' <<<"$m"))"
  ver=$(jq -r .version <<<"$m")
  [ "$(printf '%s\n' "$MIN_RUNNER" "$ver" | sort -V | head -1)" = "$MIN_RUNNER" ] \
    || die "runner $ver is older than this action needs ($MIN_RUNNER or later): pass a newer runner-version, or latest"
  out snapshot "$snap"
  out lock-note "$note"
  out runner-version "$ver"
  out target "$target"
  out runner-url "$(jq -r --arg t "$target" '.files[] | select(.target == $t) | .url' <<<"$m")"
  out runner-sha256 "$(jq -r --arg t "$target" '.files[] | select(.target == $t) | .sha256' <<<"$m")"
  out runner-name "$(jq -r --arg t "$target" '.files[] | select(.target == $t) | .name' <<<"$m")"
  out runner-dir "$(path "$RUNNER_TEMP/testsrc/runner/$ver/$target")"
  out media-dir "$(path "$RUNNER_TEMP/testsrc/media")"
  out set-key "$(printf '%s|%s' "${IN_SET:-starter}" "${IN_EXCLUDE:-}" | sha256sum | cut -c1-16)"
  echo "testsrc: snapshot $snap, runner $ver for $target"
}

run() {
  local bin="$RUNNER_DIR/$RUNNER_NAME"
  if [ -x "$bin" ] && [ "$(sha256sum "$bin" | cut -d' ' -f1)" = "$RUNNER_SHA256" ]; then
    echo "testsrc: runner from the cache"
  else
    mkdir -p "$RUNNER_DIR"
    auth | curl -sS --fail-with-body -H @- -o "$bin.part" "$RUNNER_URL" \
      || die "can't download the runner from $RUNNER_URL: $(cat "$bin.part" 2>/dev/null)"
    [ "$(sha256sum "$bin.part" | cut -d' ' -f1)" = "$RUNNER_SHA256" ] || die "the runner from $RUNNER_URL doesn't match its SHA-256"
    chmod +x "$bin.part" && mv -f "$bin.part" "$bin"
  fi

  local o="${IN_OUT:-testsrc-results}" args=(run --player "${IN_PLAYER:-libvlc}")
  [ -z "${IN_COMMAND:-}" ] || args+=(--command "$IN_COMMAND")
  case "${IN_SET:-starter}" in
    all) ;;
    *) while read -r w; do args+=("$w"); done < <(words "${IN_SET:-starter}") ;;
  esac
  while read -r w; do args+=(--exclude "$w"); done < <(words "${IN_EXCLUDE:-}")
  args+=(--out "$o" --junit "$o/report.xml")
  [ "${IN_DISCARD:-false}" != true ] || args+=(--discard)

  local rc=0
  "$bin" "${args[@]}" || rc=$?
  [ -z "${IN_LOCK_NOTE:-}" ] || printf '\n%s\n\n' "$IN_LOCK_NOTE" >> "$GITHUB_STEP_SUMMARY"
  # 0: all passed, 1: failures, 2: stopped (the API refused more, mid-run) with the results so far, or an error
  [ "$rc" -le 2 ] && [ -f "$o/results.json" ] || die "the runner stopped with exit code $rc (see its message above)"

  out results "$o/results.json"
  out junit "$o/report.xml"
  local failed
  failed=$(jq '[.results[] | select(.status == "fail" or .status == "crash" or .status == "hang")] | length' "$o/results.json")
  out failed "$failed"
  # one annotation per failing case (GitHub shows the first ten)
  local level=error
  [ "${IN_REPORT_ONLY:-false}" != true ] || level=warning
  jq -r --arg level "$level" '.results[] | select(.status == "fail" or .status == "crash" or .status == "hang")
         | "::\($level) title=testsrc \(.status): \(.id)::" + ([.checks[] | select(.[0] == "FAIL") | "\(.[1]): \(.[2])"][0] // .status)' \
    "$o/results.json"
  if [ "$rc" = 2 ]; then
    local why left
    why=$(jq -r '.stopped // "see the runner'"'"'s message above"' "$o/results.json")
    left=$(jq '[.results[] | select(.status == "skipped" and (.checks[0][2] // "" | startswith("not run")))] | length' "$o/results.json")
    die "the run stopped early: ${why%.}. $left cases weren't run."
  fi
  if [ "$failed" -gt 0 ] && [ "${IN_REPORT_ONLY:-false}" != true ]; then
    exit 1
  fi
}

case "${1:-}" in
  resolve) resolve ;;
  run) run ;;
  *) echo "usage: run.sh resolve|run (called by action.yml)" >&2; exit 2 ;;
esac
