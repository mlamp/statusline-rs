# statusline-rs

A fast, dependency-light **status line for [Claude Code](https://claude.com/claude-code)**, written in Rust.

Claude Code can render a custom status line by piping a JSON snapshot of the
session to an external command on every update. `statusline-rs` is that command:
it reads the JSON on stdin and prints a compact, colorized one-liner showing your
directory, git state, model, context budget, cost, and rate limits.

It compiles to a single ~2.5 MB binary and does all of its work in-process — git
status via [`gix`](https://github.com/GitoxideLabs/gitoxide), no `git` / `jq` /
`date` subprocesses — so it stays snappy even when the status line refreshes
frequently.

## Example

```text
statusline-rs   main (+6)  sonnet-5md  ctx:88%  $0.42  +57/-12
```

A busier session, with a pull request, both rate-limit meters, and vim mode:

```text
acme/src  #128  opus-4-8[1m]hi  ctx:62%  $1.37  +214/-38  5h(72%) ↻1h34m  Wk(41%) ↻2d3h  NORMAL
```

(In a real terminal every segment is colored; the PR number is an OSC 8 hyperlink
to the PR.)

## What each segment shows

Segments are separated by two spaces and are only shown when the relevant data is
present, so the line stays short in simple sessions.

| Segment | Example | Notes |
| --- | --- | --- |
| **Directory** | `acme/src` | `~` for home; `project/dir` when inside your project root, `project/…/dir` when nested deeper; otherwise just the folder name. |
| **Git branch** | ` main` | Current branch, or short commit hash when detached. |
| **Git status** | `(+6 !2 ?1)` | `+` staged · `!` modified in the worktree · `?` untracked. Hidden when the tree is clean. |
| **Pull request** | `#128` | Colored by review state — green = approved, red = changes requested, dim otherwise. Links to the PR when a URL is available. |
| **Model** | `opus-4-8[1m]hi` | `model.id` with the `claude-` prefix dropped, plus an effort suffix: `lo` / `md` / `hi` / `xh` / `max`. |
| **Context** | `ctx:62%` | Remaining context window. Green > 70%, yellow > 40%, orange > 20%, red at or below 20%. |
| **Cost** | `$1.37` | Total session cost in USD (hidden below ~half a cent). |
| **Lines** | `+214/-38` | Lines added / removed this session. |
| **5-hour limit** | `5h(72%) ↻1h34m` | Always shown; `↻` counts down to reset (`↻34m` under an hour). Dim under 50% usage; above that, colored by time-to-reset — green when reset is near, ramping to red only when it's far *and* you've burned a lot (50–70% caps at yellow, 70–85% at orange, ≥85% can go red; anything ≥2.5h out is red). |
| **Weekly limit** | `Wk(41%) ↻2d3h` | Weekly usage with a `↻` countdown to reset. Dim under 50%, then the usage color ramp (yellow → orange → red). |
| **Vim mode** | `NORMAL` | Green in `INSERT`, blue otherwise. Shown only when vim mode is active. |

## Install

Both methods need a recent stable [Rust toolchain](https://rustup.rs/).

### With `cargo install` (quickest)

Builds and installs the `sl` binary straight to `~/.cargo/bin` (make sure that's
on your `PATH`):

```sh
cargo install --git https://github.com/mlamp/statusline-rs
```

### From source

Handy if you want to hack on it or build in place:

```sh
git clone https://github.com/mlamp/statusline-rs
cd statusline-rs
cargo build --release
cp target/release/sl ~/.local/bin/sl   # or anywhere on your PATH
```

The release binary lands at `target/release/sl`.

### Wire it into Claude Code

Add this to your Claude Code settings (`~/.claude/settings.json`):

```json
{
  "statusLine": {
    "type": "command",
    "command": "sl"
  }
}
```

Use an absolute path (e.g. `/path/to/statusline-rs/target/release/sl`) if the
binary isn't on the `PATH` Claude Code sees.

## Requirements

- A **Nerd Font** / Powerline-patched font for the branch glyph (``). Every
  other character (`↻`, `…`) is standard Unicode.
- A terminal with ANSI + 256-color support. OSC 8 hyperlink support is optional —
  it only affects whether the PR number is clickable.

## Input

The program expects Claude Code's status-line JSON on stdin. It reads the
following fields (all optional — anything missing just omits that segment):

```jsonc
{
  "workspace":      { "current_dir": "…", "project_dir": "…" },
  "model":          { "id": "claude-…" },
  "effort":         { "level": "low|medium|high|xhigh|max" },
  "context_window": { "remaining_percentage": 62.4 },
  "cost":           { "total_cost_usd": 1.37, "total_lines_added": 214, "total_lines_removed": 38 },
  "rate_limits": {
    "five_hour": { "used_percentage": 72, "resets_at": 1751558400 },
    "seven_day": { "used_percentage": 41, "resets_at": 1752000000 }
  },
  "pr":  { "number": "128", "url": "https://…", "review_state": "approved|changes_requested|…" },
  "vim": { "mode": "NORMAL|INSERT|…" }
}
```

You can preview the output without Claude Code by piping a payload in yourself:

```sh
echo '{"workspace":{"current_dir":"/tmp/acme"},"model":{"id":"claude-sonnet-5"},"context_window":{"remaining_percentage":88}}' \
  | ./target/release/sl
```

## Customizing

The rendering is a single, readable `src/main.rs` (~350 lines, no macros or config
layer). Colors, thresholds, glyphs, and which segments appear are all plain code —
edit the relevant block and rebuild. A few starting points:

- Color thresholds live in `ratecol` (rate limits) and the `ctx` block (context).
- Segment order and spacing are assembled at the bottom of `main`.
- The directory-shortening rules are in the `dir` block near the top of `main`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
