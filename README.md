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
statusline-rs   main (+6)  sonnet-5md  ctx:88%  $0.42
```

A busier session, with a pull request, both rate-limit meters, and vim mode:

```text
acme/src  #128  opus-4-8[1m]hi  ctx:62%  $1.37  S:72%/↻1h34m  W:41%/↻2d3h  NORMAL
```

(In a real terminal every segment is colored; the PR number is an OSC 8 hyperlink
to the PR.)

## What each segment shows

Segments are separated by two spaces and are only shown when the relevant data is
present, so the line stays short in simple sessions. This layout is itself a
template you can override — see [Customizing the layout](#customizing-the-layout).

| Segment | Example | Notes |
| --- | --- | --- |
| **Directory** | `acme/src` | `~` for home; `project/dir` when inside your project root, `project/…/dir` when nested deeper; otherwise just the folder name. |
| **Git branch** | ` main` | Current branch, or short commit hash when detached. Rendered with a leading space (a slot for a branch glyph if you add one). |
| **Git status** | `(+6 !2 ?1)` | `+` staged · `!` modified in the worktree · `?` untracked. Hidden when the tree is clean. |
| **Git worktree** | `⌥my-feature` | Worktree name, shown only when you're inside a linked worktree (`git worktree add`); hidden in the main tree. Comes straight from Claude Code's payload — no repo scan. |
| **Pull request** | `#128` | Colored by review state — green = approved, red = changes requested, dim otherwise. Links to the PR when a URL is available. |
| **Model** | `opus-4-8[1m]hi` | `model.id` with the `claude-` prefix dropped, plus an effort suffix: `lo` / `md` / `hi` / `xh` / `max`. |
| **Context** | `ctx:62%` | Remaining context window. Green > 70%, yellow > 40%, orange > 20%, red at or below 20%. |
| **Cost** | `$1.37` | Total session cost in USD (hidden below ~half a cent). |
| **Session limit (5h)** | `S:72%/↻1h34m` | `S:` = 5-hour usage; `↻` counts down to reset (`↻34m` under an hour). Dim under 50% usage. Above that, colored by **burn velocity** — usage compared to how far through the 5-hour window you are: green when on or under pace, ramping yellow → orange → red the faster you're projected to overshoot the cap before reset. So a high absolute usage late in the window can still read green if you paced yourself. |
| **Weekly limit (7d)** | `W:41%/↻2d3h` | Same `↻` countdown and velocity coloring as the session meter, over a 7-day window. |
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

- A terminal with ANSI + 256-color support. Every character in the default line
  (`↻`, `…`, `⌥`, and a plain space before the branch) is standard Unicode — no
  special font required. If you want a Powerline / Nerd Font branch glyph, add it
  to your own template (see [Customizing the layout](#customizing-the-layout)).
- 24-bit **truecolor** is used automatically for the context/rate meters when your
  terminal advertises it (`COLORTERM`), with a clean fallback to the 256-color
  bands otherwise — see [Truecolor gradient](#truecolor-gradient). Nothing to
  configure.
- OSC 8 hyperlink support is optional — it only affects whether the PR number is
  clickable.

## Input

The program expects Claude Code's status-line JSON on stdin. It reads the
following fields (all optional — anything missing just omits that segment):

```jsonc
{
  "workspace":      { "current_dir": "…", "project_dir": "…" },
  "model":          { "id": "claude-…" },
  "effort":         { "level": "low|medium|high|xhigh|max" },
  "context_window": { "remaining_percentage": 62.4 },
  "cost":           { "total_cost_usd": 1.37 },
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

## Customizing the layout

The status line is produced by a small **template language**. The variables
(directory, colors, counts, …) are computed in Rust; a template decides the
labels, spacing, colors, and which segments appear. The built-in default
reproduces the line shown above — override it to rearrange, restyle, or trim it.

### Choosing a template

The quickest option is a built-in **preset** — a ready-made template picked by
name (no template string to write):

```sh
sl --preset minimal      # or: git · two-line · usage · truecolor · default
sl --list-presets        # print the available names
```

The presets are described under [Example templates](#example-templates-presets)
below. To pick a template `sl` uses the first of these that is non-empty:

1. `--format '<template>'` — an inline template string
2. `--preset <name>` — a built-in named template (`sl --list-presets`)
3. `SL_FORMAT` — environment variable
4. a **config file** — the path in `SL_CONFIG` if set, otherwise
   `~/.config/statusline-rs.tmpl` (a trailing newline is trimmed)
5. the built-in default

```sh
# try one out (preview without Claude Code)
echo '{"model":{"id":"claude-sonnet-5"},"context_window":{"remaining_percentage":88}}' \
  | sl --preset minimal

# a custom template of your own
echo '{"model":{"id":"claude-sonnet-5"},"context_window":{"remaining_percentage":88}}' \
  | sl --format "cyan(dir)(  magenta(model))(  dim('ctx:')+c_ctx)"

# make a custom one stick
mkdir -p ~/.config
printf '%s' "cyan(dir)(  magenta(model))(  dim('ctx:')+c_ctx)" > ~/.config/statusline-rs.tmpl
```

To use one from Claude Code, put the flag in the command:

```json
{ "statusLine": { "type": "command", "command": "sl --preset git" } }
```

or point `SL_CONFIG` / `~/.config/statusline-rs.tmpl` at a template of your own.

An unknown `--preset` name warns on stderr and falls back to the next option; if a
template fails to parse, `sl` silently falls back to the built-in default — it never
errors out or prints a broken line.

### Template syntax

**Text is literal by default.** Anything you type prints as-is: `S:` → `S:`.

**Variables** (see the [table below](#variables)) substitute only inside a group
`(…)` or a style call `name(…)`, and only when the name is a known variable. An
unknown name stays literal, so a typo shows up as text instead of an error.

```
dir            → dir             (literal at the top level)
(dir)          → statusline-rs   (substituted inside a group)
(dir branch)   → statusline-rs main
```

**Groups `(…)` are optional segments.** A group disappears only when *every*
variable inside it is empty — "show it if there's anything to show". A group that
references no variable never disappears.

```
(dim('ctx:')+c_ctx)   → ctx:62%     when c_ctx has a value
                      →             when c_ctx is empty (label and all)
```

Because a group survives if even one variable inside it is set, a group with
several variables shows partial content with a gap where a missing one was. Put
each optional piece in its own group so its separator collapses with it too:

```
(model effort)      → "sonnet-5 "   one group — a trailing gap where effort was
(model)(  effort)   → "sonnet-5"    separate groups — effort and its space collapse
```

**Style calls** wrap their content:

| Call | Effect |
| --- | --- |
| `bold` `dim` `italic` `underline` | text attributes |
| `gray` `orange` `red` `green` `yellow` `blue` `magenta` `cyan` | foreground colors |
| `sgr('<params>', …)` | raw SGR params — `sgr('1;31', model)` is bold red |
| `hex('#rrggbb', …)` | 24-bit truecolor foreground — `hex('#5f8700', model)` |
| `br(…)` | a line break, then the content (for multi-line status lines) |

Calls nest and compose: `red(bold(c_ctx))` → bold red.

**Value-producing builtins** pull text from *outside* Claude Code's payload.
Unlike the style calls above they don't wrap content — they *are* content, and
they gate their group like a variable (an empty/missing source collapses the
segment):

| Call | Effect |
| --- | --- |
| `file('path')` | the file's contents (first line, trimmed) — `~` expands to `$HOME` |
| `json('path', '.a.b')` | a scalar pulled from a JSON file by a jq-like dotted/indexed path |

```
(dim('note:')+yellow(file('~/.claude/sl-note.txt')))
(dim('CI:')+green(json('~/.claude/ctx.json', '.ci.status')))
```

See [Including data from files](#including-data-from-files) for the full story
(including a config file for reusable named variables).

**Commas** separate call arguments and ignore surrounding whitespace:
`sgr('1;32', branch)`.

**`+` concatenates** and swallows the whitespace around it — `dim('ctx:') + c_ctx`
and `dim('ctx:')+c_ctx` are the same tight join. It's the idiomatic way to attach a
pre-colored value to a label while keeping the value's own color (the value stays
*outside* the label's style):

```
dim('S:')+c_session_pct   → dim "S:" then the velocity-colored percentage
```

**Quotes.** `'…'` and `"…"` both delimit a literal string (the opposite quote is an
ordinary character inside, and spaces print). Use them to print text that collides
with a variable name or the syntax: `green("it's")`, `'dir'`.

**Escapes** (outside quotes): `\(`, `\)`, `\"`, `\'`, `\\`, `\+`, `\,` each print
that character. Any other `\x` is a parse error.

**Whitespace & newlines.** Leading whitespace is stripped. A literal newline
becomes one line break and the indentation after it is dropped, so you can lay a
template out across several lines; end a line with `\` to join the next line with
no break.

### Variables

`c_`-prefixed variables arrive **already colored** — drop them in bare (e.g.
`(c_ctx)`). The plain variants are raw text you color yourself. Beyond these
built-ins you can define your own, sourced from a file, with
[a variables config](#reusable--a-variables-config-file).

**Directory & workspace**

| Variable | Shows |
| --- | --- |
| `dir` | shortened current directory (`~`, `project/dir`, `project/…/dir`, or the folder name) |
| `current_dir` / `project_dir` | the raw absolute paths |
| `worktree` | git worktree name when inside a linked worktree (`git worktree add`), empty in the main tree |
| `session_id` | the session's unique id — useful to key a per-session file (see [Including data from files](#including-data-from-files)) |
| `session_name` | the custom session name (`--name` / `/rename`), if set |

**Model**

| Variable | Shows |
| --- | --- |
| `model` | model id without the `claude-` prefix (`opus-4-8[1m]`) |
| `model_full` | the full model id |
| `model_name` | the friendly display name (`Opus 4.8`) |
| `effort` | short effort code — `lo` `md` `hi` `xh` `max` |
| `effort_full` | the raw effort level (`xhigh`) |

**Context window**

| Variable | Shows |
| --- | --- |
| `c_ctx` | remaining %, color-banded (green > 70, yellow > 40, orange > 20, red ≤ 20) |
| `ctx` | remaining % as plain text (`62%`) |
| `ctx_raw` | the number only (`62`) |

**Cost**

| Variable | Shows |
| --- | --- |
| `cost` | session cost `$1.37` (empty below ~half a cent) |
| `cost_raw` | the number only (`1.37`) |

**Session (5-hour) limit**

| Variable | Shows |
| --- | --- |
| `c_session_pct` | usage %, velocity-colored |
| `session_pct` / `session_raw` | `72%` / `72` |
| `session_reset_in` | countdown to reset (`1h34m`, `34m`) |
| `session_secs` | seconds until reset |

**Weekly (7-day) limit** — `c_weekly_pct`, `weekly_pct`, `weekly_raw`,
`weekly_reset_in`, `weekly_secs`: the same as the session set over a 7-day window
(countdown formatted `2d3h` / `3h` / `30m`).

**Pull request**

| Variable | Shows |
| --- | --- |
| `c_pr` | PR badge colored by review state, OSC 8 link when a URL is present |
| `pr` | `#128` |
| `pr_number` / `pr_state` / `pr_url` | the raw fields |

**Vim mode**

| Variable | Shows |
| --- | --- |
| `c_vim` | mode, green in `INSERT` and blue otherwise |
| `vim` | the raw mode string |

**Git** — computed by a repository scan that runs **only when the template
references one of these**:

| Variable | Shows |
| --- | --- |
| `branch` | branch name, or short commit when detached |
| `c_git` | colored status badge `(+6 !2 ?1)` |
| `git` | the same badge, uncolored |
| `staged` / `modified` / `untracked` | the counts (empty when 0) |

### The default template

For reference, the built-in default (each `\`-continued source line is joined into
one before parsing):

```
cyan(dir)
(  sgr('1;32', ' '+branch)( c_git))
(  dim('⌥'+worktree))
(  c_pr)
(  magenta(model)(magenta(effort)))
(  dim('ctx:')+c_ctx)
(  dim(cost))
(  dim('S:')+c_session_pct)(dim('/↻'+session_reset_in))
(  dim('W:')+c_weekly_pct)(dim('/↻'+weekly_reset_in))
(  c_vim)
```

Each segment leads with its `  ` separator *inside* the group, so the separator
collapses together with the segment when its data is missing.

### Example templates (presets)

These five are built into the binary — select one by name with `--preset <name>`
(`sl --list-presets` lists them). Each is also printed here as its raw `--format`
string, so you can copy one and tweak it into a template of your own. Every one
degrades gracefully: segments with no data collapse, separators and all, so the
line stays tidy in a bare session.

**`minimal`** — just where you are, the model, and remaining context:

```sh
sl --preset minimal
# → statusline-rs  opus-4-8[1m]hi  ctx:62%
# --format "cyan(dir)(  magenta(model)(magenta(effort)))(  dim('ctx:')+c_ctx)"
```

**`git`** — leads with branch and working-tree status, then model + context:

```sh
sl --preset git
# → statusline-rs   main (!2 ?2)  opus-4-8[1m]hi  ctx:62%
# --format "cyan(dir)(  sgr('1;32', ' '+branch)( c_git))(  magenta(model)(magenta(effort)))(  dim('ctx:')+c_ctx)"
```

**`two-line`** — place / git / PR on top, the moving numbers below (`br(…)` breaks
the line):

```sh
sl --preset two-line
# → statusline-rs   main (!2 ?2)  #128
#   opus-4-8[1m]hi  ctx:62%  $1.37  S:72%  W:41%  NORMAL
# --format "cyan(dir)(  sgr('1;32', ' '+branch)( c_git))(  c_pr)br((magenta(model)(magenta(effort)))(  dim('ctx:')+c_ctx)(  dim(cost))(  dim('S:')+c_session_pct)(  dim('W:')+c_weekly_pct)(  c_vim))"
```

**`usage`** — context, spend, and both rate-limit meters with their reset
countdowns front and center:

```sh
sl --preset usage
# → statusline-rs  ctx 62%  cost $1.37  5h 72%/↻1h32m  7d 41%/↻2d2h
# --format "cyan(dir)(  dim('ctx ')+c_ctx)(  dim('cost ')+green(cost))(  dim('5h ')+c_session_pct(dim('/↻'+session_reset_in)))(  dim('7d ')+c_weekly_pct(dim('/↻'+weekly_reset_in)))"
```

**`truecolor`** — `hex('#rrggbb', …)` for 24-bit color and a `│` divider between
segments:

```sh
sl --preset truecolor
# → statusline-rs  │  opus-4-8[1m]hi  │  ctx 62%  │  (!2 ?2)
# --format "hex('#5f8700', dir)(  dim('│')  hex('#87afff', model)(hex('#5f87d7', effort)))(  dim('│')  dim('ctx ')+c_ctx)(  dim('│')  c_git)"
```

(`--preset default` is the built-in line shown at the top of this section.)

To tweak one, copy its `--format` string and: swap a style call (`magenta` →
`cyan`, or `hex('#…')`), reorder the `(  …)` groups, change a label (`'ctx:'` →
`'ctx '`), or swap a variable for its sibling — `model` → `model_full`, `c_ctx` →
`ctx_raw`, `session_reset_in` → `session_secs` (see [Variables](#variables)).

### Colors & thresholds

The template controls layout and the styling of plain variables, but the
**computed** (`c_*`) colors and numeric formatting are Rust — edit and rebuild:

- Color bands and per-variable formatting: `build_vars` in `src/vars.rs` (the
  `ctx`, `cost`, and session/weekly blocks).
- Rate-limit velocity / absolute coloring: `velocity_col` and `ratecol` in
  `src/main.rs`.
- Directory shortening: the `dir` block in `build_vars`.

#### Truecolor gradient

The three "meter" variables — `c_ctx`, `c_session_pct`, `c_weekly_pct` — use a
smooth **red → orange → yellow → green gradient** when the terminal advertises
24-bit color, instead of the four discrete color bands. The percentage's position
in its range picks a point on the ramp and the exact `38;2;r;g;b` truecolor is
emitted; `ctx:88%` reads solid green, `ctx:50%` yellow, `ctx:15%` a warm red. The
idle (dim) state and every other color are unchanged.

Detection is by the **`COLORTERM` environment variable** (`truecolor` or `24bit`),
not by testing the output stream — `sl`'s stdout is a pipe owned by Claude Code,
never a TTY, so the usual isatty check would wrongly disable color. `COLORTERM` is
set by most modern terminals (iTerm2, kitty, WezTerm, VS Code, recent
gnome-terminal) and inherited through Claude Code. When it's absent or reports
only 256 colors, `sl` falls back to the exact discrete bands, so nothing changes
on a 256-color terminal.

Override the detection with **`SL_TRUECOLOR`** — `1`/`true`/`always`/`on` forces
the gradient on, `0`/`false`/`never`/`off` forces the bands. Handy to preview the
gradient (`SL_TRUECOLOR=1`) or to pin the old look:

```sh
echo '{"context_window":{"remaining_percentage":50}}' | SL_TRUECOLOR=1 sl --format "(c_ctx)"
```

The ramp stops and the percentage→position mapping live in `ramp_rgb` / `ctx_t` /
`level_t` / `velocity_t` in `src/main.rs` (no dependencies — plain RGB
interpolation). The `hex('#rrggbb', …)` template call always emits truecolor
regardless of this detection.

## Including data from files

Claude Code's status-line payload is a **fixed schema** — there's no field you (or
the assistant) can stuff a custom string into. To surface anything *outside* that
payload — a deploy note, a CI status, a ticket number, whatever — the pattern is a
**file**: some process writes it, and `sl` reads it on the next refresh. `sl` gives
you two ways to read files, staying dependency-light (no `jq`, no subprocess).

### Inline — `file()` / `json()` in a template

For one-off use, read straight from the template string (see the
[builtins table](#template-syntax)):

```sh
# a plain text file, colored, with a label; collapses when the file is empty/missing
sl --format "cyan(dir)(  dim('note:')+yellow(file('~/.claude/sl-note.txt')))"

# a value pulled from a JSON file by a jq-like path (dotted keys + [n] indices)
sl --format "cyan(dir)(  dim('CI:')+green(json('~/.claude/ctx.json', '.ci.status')))"
```

The path language is a small subset of jq — enough to *pull a field out*:
`.a.b`, `.items[0].name`, leading `.` optional. A missing path, a non-scalar
(object/array) result, or an unreadable/…invalid file all resolve to empty, so the
segment simply collapses. Only the first line of a plain file is used (a status
line is one line).

**The path is an expression, not just a literal** — concatenate variables into it
with `+` to read a file whose name depends on the session. This is how you read a
per-session file (a common pattern: another tool writes `~/dir/<session-id>.json`,
your status line reads it back):

```sh
sl --format "cyan(dir)(  green(json('~/mytool/' + session_id + '.json', '.status')))"
```

If an interpolated variable is empty the path collapses to nothing and the file is
never read, so the segment disappears — e.g. in a session your tool hasn't written
a file for.

### Reusable — a variables config file

For named variables you use across templates, list them once in a JSON config at
`$SL_VARS` (or `~/.config/statusline-rs.vars.json`). Each becomes a normal
template variable that gates and styles like the built-ins:

```jsonc
{
  "vars": [
    { "name": "note", "file": "~/.claude/sl-note.txt" },                        // plain file
    { "name": "build", "file": "~/.cache/acme/ci-status.json", "path": ".state", // CI status -> symbol
      "map": { "passing": "✓", "failing": "✗", "running": "…" }, "default": "?" },
    { "name": "buildmsg", "file": "~/.cache/acme/ci-status.json", "path": ".summary", "max": 48 },
    { "name": "ticket", "env": "JIRA_TICKET", "max": 12 }                        // env var, clipped
  ]
}
```

Then reference them by name — each in its own group so it collapses when there's
no data:

```sh
sl --format "cyan(dir)(  dim('ci ')+build)(  dim(buildmsg))(  blue(ticket))"
```

Each entry needs a `name` (a valid identifier) and one source:

- **`file`** — the file's contents, optionally with **`path`** for jq-from-JSON
  extraction (`.a.b`, `.items[0].name`; leading `.` optional). The path may
  interpolate any **top-level field of the status payload** — or any built-in
  variable — as **`${field}`** (e.g. `${session_id}`, `${version}`,
  `~/mytool/${session_id}.json`), so a variable can be keyed by the session.
- **`env`** — an environment variable.

Optional per-entry modifiers:

- **`max`** — clips the value to that many characters with a trailing `…`.
- **`map`** — a value → display lookup, a conditional-free way to turn e.g. a
  status string into a symbol. A value not in the table falls back to `default`.
- **`default`** — a fallback applied **only once the file has been read**: with a
  `map`, for an unmapped value; without one, for an empty/missing value.

The read-then-default rule is what keeps the line honest: when the file is
**absent** (missing, unreadable, or invalid JSON) the variable stays *undefined*
and its group collapses — you never see a lone `default` on an otherwise empty
line. A `default` shows only when the file exists but the value is missing/empty.

Resolution is **lazy and cached**: a file is read only when the active template
references that variable, and each file is read once per invocation even when
several variables share it. A config name can never shadow a built-in variable
(it's ignored with a warning); a missing config is silently skipped, a malformed
one warns once on stderr and is skipped.

### Getting a string *from Claude* onto the status line

There's no direct channel, but the file is the bridge — anything that can write a
file can drive the status line:

- **Ask the assistant.** "Set my status-line note to `deploying`" → Claude writes
  `~/.claude/sl-note.txt`; your `file('~/.claude/sl-note.txt')` (or `note` config
  var) shows it on the next refresh.
- **A hook.** A `Stop` / `PostToolUse` hook can write the file
  (`echo "$something" > ~/.claude/sl-note.txt`) — the documented, idiomatic way to
  feed a status line.
- **Any external process.** A watcher, a CI script, a cron job — same file, same result.

`sl` re-runs after each assistant message (debounced ~300ms). If a file is written
while the session is **idle** (e.g. by a background job), add a refresh timer so the
change still shows:

```json
{ "statusLine": { "type": "command", "command": "sl", "refreshInterval": 5 } }
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
