#!/usr/bin/env bash
# The testsrc/run action's one script (action.yml calls it twice, around actions/cache):
#   run.sh resolve   which snapshot (testsrc.lock, or the latest, written to the lock), which runner for this
#                    job's OS (GET /v1/runner), whether to cache the media, and the cache folders: step outputs
#   run.sh run       download the runner unless cached (checked against its SHA-256), run it, annotate failures,
#                    add the lock note to the job summary, and fail the job on failures unless report-only
# A fork's pull request, or Dependabot's, has no secrets: without the token it gets a notice and is skipped. Any other
# run without the token fails (a missing or misnamed secret), pull_request_target too (it has the secrets).
# The runner must be 0.2.0 or later (--discard, results kept when the API stops a run); time-limit and player-version
# need 0.3.0, and with an older runner they are left out with a warning (the job's timeout-minutes still holds).
# Expected failures (the lock file's expected list) are read by the runner itself, from 0.3.0: they run and are
# reported, but don't fail the job; one that passes gets a notice, to take it off the list.
# set: all needs a company token. On GitHub's hosted runners a job plays at most HOSTED_MAX_CASES cases and
# HOSTED_MAX_MB of files, and never all: they are for fast checks; a full run belongs on a self-hosted runner.
# The media is cached only for private repositories and sets smaller than all;
# otherwise each case's files are deleted once played (--discard), so the disk holds one case at a time.
# The token goes to curl on stdin, never on a command line.
# Needs bash, curl, jq and sha256sum: all on GitHub's Linux and Windows runners (Git Bash on Windows).
# Outside Actions, set GITHUB_OUTPUT, GITHUB_STEP_SUMMARY, RUNNER_OS, RUNNER_ARCH and RUNNER_TEMP yourself.
set -euo pipefail

API=${TESTSRC_API:-https://testsrc.com/v1}
MIN_RUNNER=0.2.0   # the first runner with --discard and results kept when the API stops a run
VERSIONS_RUNNER=0.3.0   # the first with --time-limit, --player-version and --lock (expected failures)
older() { [ "$1" != "$2" ] && [ "$(printf '%s\n' "$1" "$2" | sort -V | head -1)" = "$1" ]; }   # is $1 before $2
API=${API%/}
# GitHub's hosted runners: fast checks only. The whole catalog is many hours of real-time playback per player and many
# GB, the kind of use that gets a GitHub account suspended; on a self-hosted runner there is no cap.
HOSTED_MAX_CASES=30
HOSTED_MAX_MB=2000
FULL_RUNS="https://github.com/testsrc/run#fast-checks-on-githubs-runners-full-runs-on-your-own"
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
  local why; why=$(jq -r '.error // empty' <<<"$REPLY" 2>/dev/null || true)
  # a refused token: say where the token comes from and where to get a new one
  [ "$code" != 401 ] || why="${why%.}. The token is the secret passed as token (secrets.TESTSRC_TOKEN in the templates): check it is set and current, or make a new one at https://testsrc.com/account and update the secret"
  [ "$code" = 200 ] || die "$code from $API$1: $why"
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
    die "no token: the token input is empty. Make a token at https://testsrc.com/account (sign in with GitHub), add it as a repository secret named TESTSRC_TOKEN (Settings > Secrets and variables > Actions), and pass it: token: \${{ secrets.TESTSRC_TOKEN }}"
  fi
  out skip false
  case "${IN_PLAYER:-libvlc}" in libvlc|ffmpeg|gstreamer|mpv|cmd) ;; *) die "player must be libvlc, ffmpeg, gstreamer, mpv or cmd, not ${IN_PLAYER}" ;; esac
  local limit="${IN_TIME_LIMIT:-}" pver="${IN_PLAYER_VERSION:-}"
  limit=${limit//[[:space:]]/}
  if [ -n "$limit" ]; then
    [[ $limit =~ ^[0-9]+(\.[0-9]+)?$ ]] && [[ $limit =~ [1-9] ]] \
      || die "time-limit is in minutes, like 25, not $limit. Set it a few minutes below the job's timeout-minutes"
  fi
  [[ $pver != *$'\n'* ]] || die "player-version must be one line, like 1.4.2 or \${{ github.ref_name }}"

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
  local set="${IN_SET:-starter}" hosted=false
  [ "${IN_RUNNER_ENV:-}" != github-hosted ] || hosted=true
  if [ "$set" = all ] && [ "$hosted" = true ]; then
    die "set: all plays every case, many hours and many GB: too much for GitHub's hosted runners. Run it on a self-hosted runner, or pick up to $HOSTED_MAX_CASES cases. See $FULL_RUNS"
  fi
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
  if older "$ver" "$VERSIONS_RUNNER"; then
    [ -z "$limit" ] || echo "::warning title=testsrc::time-limit needs runner $VERSIONS_RUNNER or later; runner $ver runs without it, so only the job's timeout-minutes stops it (and then without a report)."
    [ -z "$pver" ] || echo "::warning title=testsrc::player-version needs runner $VERSIONS_RUNNER or later; runner $ver doesn't record it in results.json."
    if [ -f "$lock" ] && grep -q '^[[:space:]]*expected[[:space:]]*=' "$lock"; then
      echo "::warning title=testsrc::the expected failures in $lock need runner $VERSIONS_RUNNER or later; runner $ver doesn't read them, so they fail the job."
    fi
    limit="" pver="" lock=""
  fi
  out lock-file "$lock"
  out time-limit "$limit"
  out player-version "$pver"
  out snapshot "$snap"
  out hosted "$hosted"
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

# Send results.json back (POST /v1/runs, phase 8a) and add what changed since the last run on this branch to the
# summary. Never fails the job: a run that can't be sent is still a run. The body goes through a file, since the
# token is on curl's stdin (kept out of the process list).
# GET /v1/usage says first what testsrc takes from this token: nothing when the account's runs are switched off, and
# no log lines for a company token (or when it can't tell), so those lines never leave the job.
upload() {
  local o="$1"
  if [ "${IN_UPLOAD:-true}" = false ]; then echo "testsrc: upload: false, the results stay in this job"; return; fi
  local body reply code policy
  policy=$(auth | curl -sS -H @- --fail "$API/usage" 2>/dev/null) || policy='{}'
  if [ "$(jq -r '.uploads' <<<"$policy" 2>/dev/null)" = false ]; then
    echo "testsrc: this account's runs aren't kept by testsrc, the results stay in this job"; return
  fi
  body=$(mktemp)
  if [ "$(jq -r '.upload_logs' <<<"$policy" 2>/dev/null)" = true ]; then
    jq -c . "$o/results.json" > "$body"
  else
    jq -c '.results |= map(del(.log) | if .events then .events |= map(select(.t != "log")) else . end)' "$o/results.json" > "$body"
  fi
  reply=$(auth | curl -sS -H @- -H 'content-type: application/json' --data-binary "@$body" -w '\n%{http_code}' "$API/runs" 2>&1) || {
    rm -f "$body"; echo "::warning title=testsrc::couldn't send the results to testsrc (${reply##*$'\n'}); they are in $o"; return; }
  rm -f "$body"
  code=${reply##*$'\n'}; reply=${reply%$'\n'*}
  case "$code" in
    201) ;;
    404|405) echo "testsrc: this API doesn't take runs yet; the results are in $o"; return ;;
    403) if [ "$(jq -r '.code // empty' <<<"$reply" 2>/dev/null)" = uploads_off ]; then
           echo "testsrc: this account's runs aren't kept by testsrc, the results stay in this job"; return; fi
         echo "::warning title=testsrc::testsrc didn't keep this run (403: $(jq -r '.error // empty' <<<"$reply" 2>/dev/null)); the results are in $o"; return ;;
    *) echo "::warning title=testsrc::testsrc didn't keep this run ($code: $(jq -r '.error // empty' <<<"$reply" 2>/dev/null)); the results are in $o"; return ;;
  esac
  local url new fixed
  url=$(jq -r '.url // empty' <<<"$reply")
  [ -z "$url" ] || out run-url "$url"
  if [ "$(jq -r '.previous != null' <<<"$reply")" = true ]; then
    new=$(jq -r '.new_failures | length' <<<"$reply"); fixed=$(jq -r '.fixed | length' <<<"$reply")
    {
      printf '\n**Since the last run on this branch** (%s): ' "$(jq -r '(.previous.commit // "")[0:7] as $c | if $c == "" then .previous.at[0:10] else "`\($c)`" end' <<<"$reply")"
      if [ "$new" = 0 ] && [ "$fixed" = 0 ]; then printf 'no change.'; fi
      [ "$new" = 0 ] || printf '%s newly failing: %s. ' "$new" "$(jq -r '[.new_failures[:10][] | "`\(.)`"] | join(", ")' <<<"$reply")"
      [ "$fixed" = 0 ] || printf '%s fixed: %s.' "$fixed" "$(jq -r '[.fixed[:10][] | "`\(.)`"] | join(", ")' <<<"$reply")"
      printf '\n'
    } >> "$GITHUB_STEP_SUMMARY"
  fi
  [ -z "$url" ] || printf '\n[This run on testsrc](%s)\n' "$url" >> "$GITHUB_STEP_SUMMARY"
  echo "testsrc: run sent${url:+: $url}"
}

# On GitHub's hosted runners: the cases this job would play, counted and sized by `testsrc list` before anything is
# fetched, against HOSTED_MAX_CASES and HOSTED_MAX_MB. list prints one line per case: id, "layer", n, MB, "MB", outcome.
# With --player (runner 0.3.0 on), list counts starter as run plays it: the general cases plus the engine's own;
# an older runner's list counts the general cases only.
hosted_limits() {
  local bin=$1 listing n mb gb player=()
  shift
  if "$bin" list --help 2>/dev/null | grep -q -- '--player'; then player=(--player "${IN_PLAYER:-libvlc}"); fi
  listing=$("$bin" list "${player[@]}" "$@") || die "can't list the cases (see the runner's message above)"
  read -r n mb gb < <(awk '$5 == "MB" { n++; mb += $4 } END { printf "%d %.0f %.1f\n", n, mb, mb / 1000 }' <<<"$listing")
  if [ "$n" -gt "$HOSTED_MAX_CASES" ] || [ "$mb" -gt "$HOSTED_MAX_MB" ]; then
    die "this set is $n cases and $gb GB. On GitHub's hosted runners a job plays at most $HOSTED_MAX_CASES cases and $((HOSTED_MAX_MB / 1000)) GB: pick fewer cases (exclude: duration/ leaves out the long files), or run it on a self-hosted runner. See $FULL_RUNS"
  fi
  echo "testsrc: $n cases, $mb MB (GitHub's hosted runners: up to $HOSTED_MAX_CASES cases and $HOSTED_MAX_MB MB a job)"
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

  # the cases: the set's ids and prefixes (none for all) and the excludes, the same for list and run
  local sel=()
  case "${IN_SET:-starter}" in
    all) ;;
    *) while read -r w; do sel+=("$w"); done < <(words "${IN_SET:-starter}") ;;
  esac
  while read -r w; do sel+=(--exclude "$w"); done < <(words "${IN_EXCLUDE:-}")
  [ "${IN_HOSTED:-false}" != true ] || hosted_limits "$bin" "${sel[@]}"

  local o="${IN_OUT:-testsrc-results}" args=(run --player "${IN_PLAYER:-libvlc}")
  [ -z "${IN_COMMAND:-}" ] || args+=(--command "$IN_COMMAND")
  args+=("${sel[@]}" --out "$o" --junit "$o/report.xml")
  [ "${IN_DISCARD:-false}" != true ] || args+=(--discard)
  [ -z "${IN_TIME_LIMIT:-}" ] || args+=(--time-limit "$IN_TIME_LIMIT")
  [ -z "${IN_PLAYER_VERSION:-}" ] || args+=(--player-version "$IN_PLAYER_VERSION")
  [ -z "${IN_LOCK_FILE:-}" ] || args+=(--lock "$IN_LOCK_FILE")

  local rc=0
  "$bin" "${args[@]}" || rc=$?
  [ -z "${IN_LOCK_NOTE:-}" ] || printf '\n%s\n\n' "$IN_LOCK_NOTE" >> "$GITHUB_STEP_SUMMARY"
  # 0: all passed, 1: failures, 2: stopped (the API refused more, mid-run) with the results so far, or an error
  [ "$rc" -le 2 ] && [ -f "$o/results.json" ] || die "the runner stopped with exit code $rc (see its message above)"

  out results "$o/results.json"
  out junit "$o/report.xml"
  local failed
  # what fails the job: a case that failed, crashed or hung, unless it is on the lock file's expected list
  local broke='(.status == "fail" or .status == "crash" or .status == "hang") and .expected != true'
  failed=$(jq "[.results[] | select($broke)] | length" "$o/results.json")
  out failed "$failed"
  upload "$o"
  # one annotation per failing case (GitHub shows the first ten)
  local level=error
  [ "${IN_REPORT_ONLY:-false}" != true ] || level=warning
  jq -r --arg level "$level" ".results[] | select($broke)"' | "::\($level) title=testsrc \(.status): \(.id)::"
         + ([.checks[] | select(.[0] == "FAIL") | "\(.[1]): \(.[2])"][0] // .status)' "$o/results.json"
  # an expected failure that passes: say so, since it no longer guards anything while it stays on the list
  jq -r --arg lock "${IN_LOCK_FILE:-the lock file}" '.results[] | select(.expected == true and .status == "pass")
         | "::notice title=testsrc: \(.id) passes::It is on the expected failures in \($lock): take it off the list, so a new failure fails the job."' \
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
