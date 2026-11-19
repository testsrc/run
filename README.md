# testsrc runner and GitHub Action

[testsrc](https://testsrc.com) makes video and audio test files that break players: rotated and mirrored video,
odd frame rates, edit lists, timestamp jumps, HDR tags, damaged and truncated files, and hundreds more. Each file
comes with a fact sheet saying what a correct player shows: every frame, its time, the beeps in the audio.

This repo is the open-source part:

- **`testsrc`, the runner** (`client/`, Rust): fetches cases through the testsrc API, plays each one through your
  player in its own process, and checks what the player showed against the fact sheet. Every frame carries a code,
  so the checker sees a dropped, repeated, late or rotated frame and audio that drifts. A crash or a hang is a
  result, and the run carries on with the next case. JUnit report, `results.json` and a GitHub job summary.
- **`testsrc/run`, the GitHub Action** (`action.yml`, `run.sh`): downloads the runner for the job's OS, runs it
  against your player, pins the snapshot in `testsrc.lock`, and fails the job when a case fails.
- **Workflow templates** (`examples/`).

The test files and their fact sheets aren't in this repo. They come from the testsrc API with a token.

## Quick start: testsrc in CI in five minutes

1. **Get a token** at [testsrc.com/account](https://testsrc.com/account) (sign in with GitHub). Open-source
   projects get one free, on request. Companies start with a 14-day evaluation token.
2. **Add it to your repo** as a secret named `TESTSRC_TOKEN`: Settings > Secrets and variables > Actions >
   New repository secret.
3. **Copy a template** into `.github/workflows/`:
   - [`examples/libvlc.yml`](examples/libvlc.yml): your app plays through libvlc (Linux and Windows).
   - [`examples/ffmpeg-player.yml`](examples/ffmpeg-player.yml): your player is built on FFmpeg.
   - [`examples/cmd-player.yml`](examples/cmd-player.yml): any other player, through its command line.
   - [`examples/update-lock.yml`](examples/update-lock.yml): once a month, a pull request that moves you to the
     newest snapshot.
4. **Push.** The first run tests the latest snapshot and writes it to `testsrc.lock`. Commit that file, so every
   later run tests the same files until you move it.

The templates run the 10 starter cases on pull requests, and every Monday a fixed set of 30 different cases (the
starter cases and 20 more, in `TESTSRC_WEEKLY`), which an evaluation token can run. Every case (`set: all`) is
for company tokens, from a manual run (Actions > the workflow > Run workflow).

The smallest workflow step:

```yaml
- uses: testsrc/run@v1
  with:
    token: ${{ secrets.TESTSRC_TOKEN }}
    player: cmd
    command: ./build/myplayer --headless {file}
```

## The action's inputs

| Input | Default | |
| --- | --- | --- |
| `token` | (required) | A testsrc token, from a secret. |
| `player` | `libvlc` | `libvlc`, `ffmpeg`, `gstreamer`, `mpv`, or `cmd` for any player through its command line. |
| `command` | | For `cmd`: the command line, `{file}` for the case's path. |
| `set` | `starter` | `starter` (10 cases, about two minutes), case ids and prefixes (`starter hls/ edge/rotation-90-display-matrix.mp4`), or `all`, which needs a company token. |
| `exclude` | | Prefixes to leave out, like `duration/` (the long files). |
| `snapshot` | | Empty: the one in `testsrc.lock`, or the latest, written to `testsrc.lock`. `latest`: the newest, lock untouched. Or a date: `2026-11-06`. |
| `lock` | `testsrc.lock` | Where the snapshot is pinned. |
| `report-only` | `false` | `true`: failures become warnings and the job passes. Errors (a bad token, an unknown case, a limit reached mid-run) still fail it, after the report. |
| `runner-version` | `latest` | A runner version, like `0.1.0`. |
| `out` | `testsrc-results` | Where `results.json`, `report.xml` and each case's events go. |

Outputs: `snapshot`, `results`, `junit`, and `failed` (how many cases failed, crashed or hung).

The runner is cached per version. On a private repository the files are cached too, per snapshot and set, so a run
downloads nothing on a cache hit (it still asks the API for the runner's version and the catalog). On a public
repository, and for `set: all`, nothing is cached and each case's files are deleted once it has played: a public
repository's cache can be restored by anyone's pull request, and every case doesn't fit the cache or the disk.

If the API stops a run part-way (the daily quota, an evaluation token's 30 cases), the cases already played are
in the report and the summary, the ones left are marked skipped, and the job fails with the API's reason.

## Players

| `player` | What it checks | Needs on the machine |
| --- | --- | --- |
| `libvlc` | Every frame, its time, orientation, aspect, colour, and the audio against its beeps | libvlc 3.0 (`libvlc5 vlc-plugin-base`; Windows: VLC in `C:\Program Files\VideoLAN\VLC`). `LIBVLC` or `VLC_DIR` point at your own build. |
| `ffmpeg` | The same, through `ffmpeg` and `ffprobe` | FFmpeg 5.1 or later on `PATH`: build the one your player links |
| `gstreamer` | The same, through `playbin3` | GStreamer 1.x |
| `mpv` | The same, through libmpv | `libmpv.so.2`, and JACK (`jackd`, `libjack.so.0`) for the audio |
| `cmd` | What the process does: exit code, crash, hang, time | Your player, made to play to the end and exit |

GitHub's hosted runners: Linux x64 (`ubuntu-24.04`, `ubuntu-22.04`) and Windows x64. For macOS and arm64 there is no
runner build yet.

## How tokens work

- The token goes in the `Authorization: Bearer` header of every request. Keep it in a secret, never in the
  workflow file. You can revoke it at [testsrc.com/account](https://testsrc.com/account) and make a new one.
- The catalog and case pages are public. Files and full fact sheets need a token, and each token has daily limits.
  Open source: 5,000 requests and 5 GB a day. An evaluation token: the same, for 30 distinct cases over 14 days.
  Downloading the runner doesn't count against the limits.
- **Pull requests from forks get no secrets.** There the action notes that it had no token and skips itself, so
  outside contributions don't get a red check; testsrc runs on the branch once merged.
- The player under test never gets the token: the runner removes it from the player's environment.

## Snapshots and `testsrc.lock`

The files come in snapshots named by date. A snapshot never changes once published, so a pinned snapshot gives
the same files, facts and verdicts on every run. New cases arrive in new snapshots. `testsrc.lock` holds the one
your repo tests against:

```toml
snapshot = "2026-11-06"
```

`examples/update-lock.yml` opens a pull request once a month that moves it to the newest snapshot, with the cases
added and removed. The testsrc run on that pull request shows what the new cases find before you merge.

## Running it on your machine

Download the runner with your token. `GET https://testsrc.com/v1/runner` lists the builds, with a download URL each:

```sh
export TESTSRC_TOKEN=tsk_...
curl -fsSL -H "Authorization: Bearer $TESTSRC_TOKEN" -o testsrc \
  https://testsrc.com/v1/runner/latest/x86_64-unknown-linux-gnu && chmod +x testsrc
./testsrc run --player libvlc starter --junit report.xml
./testsrc run --player cmd --command "myplayer --headless {file}" hls/
```

Or build it: `cd client && cargo build --release --locked -p testsrc` (releases are built with Rust 1.95). Nothing
else is needed at build time: the players are loaded at run time.

## What is open, what isn't

The runner (frame-code reader, checker, player adapters) and the action are open source (MIT), so you can read exactly
how a verdict is reached, run it anywhere, and add an adapter for your own player. The test files and their fact
sheets are testsrc's work and come through the API with a token, under the testsrc file licence: test with them, commit them to
your repo, ship them in your test suite and attach them to bug reports; don't host them as a collection, resell them
or put them in a dataset.

Open-source projects that test media handling (players, decoders, transcoders, frameworks) get a free token: ask
at [testsrc.com](https://testsrc.com/#help) with what you'll test and roughly how much.

## Licence

MIT, for the runner (`client/`), the action and the examples: see [`LICENSE`](LICENSE). The test files and fact
sheets served by the testsrc API aren't covered by it: they stay under the
[testsrc file licence](https://testsrc.com/licence).
