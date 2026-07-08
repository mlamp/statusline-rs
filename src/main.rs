// Format language engine (renders the status line from the variable map).
mod format;

// Variable layer: builds the `name -> value` map from the JSON payload.
mod vars;

// External (file-backed) custom variables — jq-from-a-file / env / plain file.
mod extvars;

use std::io::{Read, Write};
use std::path::Path;

const ESC: &str = "\x1b";

fn basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

// Replicate `printf '%.0f'` (round half to even, like C/Rust float formatting),
// returning the rounded value as an i64 for both display and comparisons.
fn round0(v: f64) -> i64 {
    format!("{:.0}", v).parse::<i64>().unwrap_or(0)
}

fn ratecol(p: i64) -> &'static str {
    if p >= 95 {
        "1;31"
    } else if p >= 85 {
        "91"
    } else if p >= 70 {
        "38;5;208"
    } else {
        "33"
    }
}

// Current wall-clock time as unix seconds (0 on failure).
fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// Velocity-based color for a rate-limit meter. `used` is the 0..100 usage
// percent; `elapsed` is the 0.0..1.0 fraction of the window that has already
// elapsed. Below 50% usage the meter is dimmed (plenty of runway, don't
// distract). Above that we compare the burn to the linear on-pace line:
// `used / elapsed` projects the current pace forward to the reset. A projection
// of ~100% means we'll reach the reset right as the cap is hit (green); the
// further the projection overshoots 100%, the faster we're burning and the
// warmer the color ramps, up to red.
//
// Note: this is deliberately pace-based, not level-based, so a high absolute
// usage late in the window (e.g. 92% used with 95% elapsed) still reads green —
// you paced yourself and will reset before the cap. To also warn on a high
// absolute level regardless of pace, add an early `if used >= 90 { return … }`.
fn velocity_col(used: i64, elapsed: f64) -> &'static str {
    if used < 50 {
        return "2"; // dim: idle
    }
    // Clamp elapsed away from 0 so a just-started window can't divide-by-~zero.
    let e = elapsed.clamp(0.01, 1.0);
    let ratio = used as f64 / 100.0 / e;
    if ratio <= 1.0 {
        "32" // green: on or under pace
    } else if ratio <= 1.25 {
        "33" // yellow: a bit ahead of pace
    } else if ratio <= 1.5 {
        "38;5;208" // orange
    } else {
        "91" // red: burning well over pace
    }
}

// --- truecolor gradient for the band-driven meters (c_ctx, c_session_pct,
// c_weekly_pct) ---
//
// The meters above pick one of four discrete ANSI/256 codes. When the terminal
// advertises truecolor we instead interpolate a smooth red→orange→yellow→green
// ramp and emit a `38;2;r;g;b` foreground; otherwise we fall back to the exact
// same discrete code as before, so nothing changes for 256-color terminals.

/// Decide whether to emit 24-bit truecolor. `sl` writes to a *pipe* owned by
/// Claude Code, never a TTY, so isatty is the wrong signal — the host terminal's
/// capability reaches us through the inherited `COLORTERM` env var (the same
/// thing `anstyle-query`/`supports-color` read). `SL_TRUECOLOR` force-overrides
/// it (`1`/`true`/`always`/`on` vs `0`/`false`/`never`/`off`). Pure so it's
/// testable; [`truecolor_supported`] wires it to the process environment.
fn truecolor_choice(sl_truecolor: Option<&str>, colorterm: Option<&str>) -> bool {
    if let Some(v) = sl_truecolor {
        match v.to_ascii_lowercase().as_str() {
            "1" | "true" | "always" | "on" | "yes" => return true,
            "0" | "false" | "never" | "off" | "no" => return false,
            _ => {}
        }
    }
    matches!(colorterm, Some("truecolor") | Some("24bit"))
}

/// Read the environment for a truecolor decision (see [`truecolor_choice`]).
fn truecolor_supported() -> bool {
    truecolor_choice(
        std::env::var("SL_TRUECOLOR").ok().as_deref(),
        std::env::var("COLORTERM").ok().as_deref(),
    )
}

/// Interpolate the 4-stop red→orange→yellow→green palette at `t` in `[0,1]`
/// (`0` = red/bad, `1` = green/good), returning an `(r,g,b)` foreground. The
/// orange stop is exactly `#ff8700` — the `38;5;208` the discrete bands use — so
/// truecolor and 256-color output stay visually consistent.
fn ramp_rgb(t: f64) -> (u8, u8, u8) {
    const STOPS: [(u8, u8, u8); 4] = [
        (215, 0, 0),   // t=0    red
        (255, 135, 0), // t=1/3  orange (#ff8700 == xterm 208)
        (255, 215, 0), // t=2/3  yellow
        (95, 175, 0),  // t=1    green
    ];
    let t = t.clamp(0.0, 1.0);
    let seg = (t * 3.0).min(2.999_999_999);
    let i = seg as usize;
    let f = seg - i as f64;
    let (r0, g0, b0) = STOPS[i];
    let (r1, g1, b1) = STOPS[i + 1];
    let lerp = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * f).round() as u8;
    (lerp(r0, r1), lerp(g0, g1), lerp(b0, b1))
}

/// SGR foreground params for a meter: the truecolor ramp at `t` when
/// `truecolor`, else the discrete `band` code unchanged.
fn meter_sgr(t: f64, band: &str, truecolor: bool) -> String {
    if truecolor {
        let (r, g, b) = ramp_rgb(t);
        format!("38;2;{r};{g};{b}")
    } else {
        band.to_string()
    }
}

/// Ramp position for the context meter from remaining-percent `r`: full green at
/// ≥75% remaining, ramping down through yellow/orange to red near empty.
fn ctx_t(r: i64) -> f64 {
    (r as f64 / 75.0).clamp(0.0, 1.0)
}

/// Ramp position for the absolute-usage (no-reset) meter: `p≤50` → yellow,
/// `p≥100` → red. Higher usage burns hotter. (The meter only reaches this path
/// at `p ≥ 50`; the band starts at yellow, so the ramp does too — it never goes
/// green here.)
fn level_t(p: i64) -> f64 {
    let over = (100 - p).clamp(0, 50) as f64 / 50.0; // 1 at p<=50, 0 at p>=100
    (2.0 / 3.0) * over // p<=50 -> yellow(2/3), p=100 -> red(0)
}

/// Ramp position for the velocity meter, or `None` when idle (`used < 50`, which
/// stays dim in both color modes). Maps the pace ratio onto `[0,1]`: on-pace
/// (`ratio ≤ 1`) → green, `ratio ≥ 1.5` → red — the same window the discrete
/// bands use.
fn velocity_t(used: i64, elapsed: f64) -> Option<f64> {
    if used < 50 {
        return None; // idle: dim, no gradient
    }
    let e = elapsed.clamp(0.01, 1.0);
    let ratio = used as f64 / 100.0 / e;
    Some(((1.5 - ratio) / 0.5).clamp(0.0, 1.0))
}

// serde_json helpers mirroring jq `// default` + tostring semantics.
fn str_field(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        _ => String::new(),
    }
}

fn num_opt(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// Branch name plus staged / modified / untracked counts for the current repo.
struct GitInfo {
    branch: String,
    staged: i64,
    modified: i64,
    untracked: i64,
}

/// Scan the repository at `cwd`, returning the branch (short HEAD when detached)
/// and the raw staged/modified/untracked counts. All zero / empty when `cwd` is
/// not inside a repository. The colored/uncolored badge strings are assembled by
/// the caller from these counts.
fn git_info(cwd: &str) -> GitInfo {
    let mut gi = GitInfo {
        branch: String::new(),
        staged: 0,
        modified: 0,
        untracked: 0,
    };
    if cwd.is_empty() {
        return gi;
    }
    let repo = match gix::discover(cwd) {
        Ok(r) => r,
        Err(_) => return gi,
    };

    // branch --show-current, fallback to short HEAD
    if let Ok(Some(name)) = repo.head_name() {
        gi.branch = name.shorten().to_string();
    }
    if gi.branch.is_empty() {
        if let Ok(id) = repo.head_id() {
            gi.branch = id.shorten_or_id().to_string();
        }
    }

    if let Ok(platform) = repo.status(gix::progress::Discard) {
        if let Ok(iter) = platform.into_iter(None) {
            use gix::status::index_worktree::Item as IwItem;
            use gix::status::Item;
            for item in iter.flatten() {
                match item {
                    Item::TreeIndex(_) => gi.staged += 1, // staged: col1 in [M,A,D,R,C]
                    Item::IndexWorktree(iw) => match iw {
                        IwItem::Modification { .. } => gi.modified += 1, // worktree col2 in [M,D]
                        IwItem::DirectoryContents { entry, .. } => {
                            if matches!(entry.status, gix::dir::entry::Status::Untracked) {
                                gi.untracked += 1;
                            }
                        }
                        IwItem::Rewrite { .. } => {}
                    },
                }
            }
        }
    }
    gi
}

/// Insert the git-derived variables (`branch`, `staged`, `modified`,
/// `untracked`, `git`, `c_git`) into `v` from a repo scan of `cwd`. Each count
/// var is omitted when 0; `git`/`c_git` are omitted when the tree is clean.
/// `c_git` reproduces the current colored badge exactly.
fn insert_git_vars(v: &mut std::collections::BTreeMap<String, String>, cwd: &str) {
    let gi = git_info(cwd);
    if !gi.branch.is_empty() {
        v.insert("branch".to_string(), gi.branch.clone());
    }
    if gi.staged > 0 {
        v.insert("staged".to_string(), gi.staged.to_string());
    }
    if gi.modified > 0 {
        v.insert("modified".to_string(), gi.modified.to_string());
    }
    if gi.untracked > 0 {
        v.insert("untracked".to_string(), gi.untracked.to_string());
    }
    let mut plain = String::new();
    let mut colored = String::new();
    for (n, sign, code) in [(gi.staged, '+', "32"), (gi.modified, '!', "33"), (gi.untracked, '?', "2")] {
        if n > 0 {
            if !plain.is_empty() {
                plain.push(' ');
            }
            plain.push(sign);
            plain.push_str(&n.to_string());
            if !colored.is_empty() {
                colored.push(' ');
            }
            colored.push_str(&format!("{ESC}[{code}m{sign}{n}{ESC}[0m"));
        }
    }
    if !colored.is_empty() {
        v.insert("git".to_string(), format!("({plain})"));
        v.insert(
            "c_git".to_string(),
            format!("{ESC}[1;32m({colored}{ESC}[1;32m){ESC}[0m"),
        );
    }
}

/// Resolve the template string per precedence (first non-empty wins):
/// `--format <STR>` arg > `--preset <NAME>` arg > `$SL_FORMAT` > config file
/// (`$SL_CONFIG` path if set, else `~/.config/statusline-rs.tmpl`, trailing
/// newline trimmed) > `DEFAULT`. An unknown `--preset` name warns on stderr and
/// falls through to the rest of the chain (so the status line never breaks).
fn pick_template() -> String {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut fmt: Option<String> = None;
    let mut preset: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--format" {
            if fmt.is_none() {
                if let Some(val) = args.get(i + 1) {
                    if !val.is_empty() {
                        fmt = Some(val.clone());
                    }
                }
            }
            i += 2;
            continue;
        } else if let Some(val) = a.strip_prefix("--format=") {
            if !val.is_empty() && fmt.is_none() {
                fmt = Some(val.to_string());
            }
        } else if a == "--preset" {
            if preset.is_none() {
                if let Some(val) = args.get(i + 1) {
                    if !val.is_empty() {
                        preset = Some(val.clone());
                    }
                }
            }
            i += 2;
            continue;
        } else if let Some(val) = a.strip_prefix("--preset=") {
            if !val.is_empty() && preset.is_none() {
                preset = Some(val.to_string());
            }
        }
        i += 1;
    }
    // 1. --format literal template wins over a named preset.
    if let Some(f) = fmt {
        return f;
    }
    // 2. --preset NAME: resolve, or warn and fall through on an unknown name.
    if let Some(name) = preset {
        if let Some(t) = vars::preset(&name) {
            return t.to_string();
        }
        eprintln!(
            "sl: unknown preset '{name}' (try one of: {}); falling back",
            vars::preset_names().join(", ")
        );
    }
    if let Ok(f) = std::env::var("SL_FORMAT") {
        if !f.is_empty() {
            return f;
        }
    }
    let path = match std::env::var("SL_CONFIG") {
        Ok(p) if !p.is_empty() => p,
        _ => {
            let home = std::env::var("HOME").unwrap_or_default();
            if home.is_empty() {
                String::new()
            } else {
                format!("{home}/.config/statusline-rs.tmpl")
            }
        }
    };
    if !path.is_empty() {
        if let Ok(content) = std::fs::read_to_string(&path) {
            let trimmed = content.strip_suffix('\n').unwrap_or(&content);
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    vars::DEFAULT.to_string()
}

fn main() {
    // Handle `--list-presets` before touching stdin (it would otherwise block
    // waiting for the JSON payload when run interactively).
    if std::env::args().skip(1).any(|a| a == "--list-presets") {
        for name in vars::preset_names() {
            println!("{name}");
        }
        return;
    }

    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let json: serde_json::Value = serde_json::from_str(&input).unwrap_or(serde_json::Value::Null);

    let home = std::env::var("HOME").unwrap_or_default();
    let tmpl = pick_template();

    // Registry of known variable names, shared by the reference scan and render.
    let mut registry = vars::registry();

    // Referenced vars drive the git-scan decision. On a parse error, fall back to
    // the known-good DEFAULT's references (and, below, to rendering DEFAULT).
    let refs = format::referenced_vars(&tmpl, &registry)
        .or_else(|_| format::referenced_vars(vars::DEFAULT, &registry))
        .unwrap_or_default();

    let mut varmap = vars::build_vars(&json, now_secs(), &home, truecolor_supported());

    if vars::needs_git(&refs) {
        let cwd = varmap.get("current_dir").cloned().unwrap_or_default();
        insert_git_vars(&mut varmap, &cwd);
    }

    let mut vars_map: std::collections::HashMap<String, String> = varmap.into_iter().collect();

    // External (file-backed) custom variables: merged after the built-ins so a
    // stray config can never shadow a core variable. Each becomes a normal
    // template variable, gating and styling exactly like the built-ins. `load`
    // reads the built-in vars for `${name}` path interpolation (e.g. keying a
    // file by session_id).
    let ext = extvars::load(&home, &vars_map);
    extvars::merge(ext, &mut registry, &mut vars_map, vars::VAR_NAMES);
    let out = match format::render_home(&tmpl, &vars_map, &registry, &home) {
        Ok(s) => s,
        Err(_) => format::render_home(vars::DEFAULT, &vars_map, &registry, &home).unwrap_or_default(),
    };

    let stdout = std::io::stdout();
    let mut h = stdout.lock();
    let _ = h.write_all(out.as_bytes());
    let _ = h.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truecolor_choice_override_beats_env() {
        // Explicit SL_TRUECOLOR override wins over COLORTERM, both ways.
        assert!(truecolor_choice(Some("1"), None));
        assert!(truecolor_choice(Some("always"), Some("256color")));
        assert!(!truecolor_choice(Some("never"), Some("truecolor")));
        assert!(!truecolor_choice(Some("0"), None));
        // An unrecognized override value falls through to COLORTERM.
        assert!(truecolor_choice(Some("maybe"), Some("truecolor")));
    }

    #[test]
    fn truecolor_choice_reads_colorterm() {
        assert!(truecolor_choice(None, Some("truecolor")));
        assert!(truecolor_choice(None, Some("24bit")));
        assert!(!truecolor_choice(None, Some("256color")));
        assert!(!truecolor_choice(None, None));
    }

    #[test]
    fn ramp_rgb_stops_and_clamp() {
        assert_eq!(ramp_rgb(0.0), (215, 0, 0)); // red
        assert_eq!(ramp_rgb(1.0 / 3.0), (255, 135, 0)); // orange (#ff8700 == xterm 208)
        assert_eq!(ramp_rgb(2.0 / 3.0), (255, 215, 0)); // yellow
        assert_eq!(ramp_rgb(1.0), (95, 175, 0)); // green
        assert_eq!(ramp_rgb(-1.0), (215, 0, 0)); // clamps to red
        assert_eq!(ramp_rgb(2.0), (95, 175, 0)); // clamps to green
    }

    #[test]
    fn ctx_t_maps_remaining_to_ramp() {
        assert_eq!(ctx_t(75), 1.0); // full green at >=75% remaining
        assert_eq!(ctx_t(150), 1.0); // clamped
        assert_eq!(ctx_t(0), 0.0); // red when empty
    }

    #[test]
    fn level_t_hotter_with_usage() {
        assert_eq!(level_t(50), 2.0 / 3.0); // yellow at p<=50
        assert_eq!(level_t(100), 0.0); // red at max usage
        assert_eq!(level_t(120), 0.0); // clamped
        assert!(level_t(90) < level_t(70) && level_t(70) < level_t(50));
    }

    #[test]
    fn velocity_t_idle_is_none_else_paced() {
        assert_eq!(velocity_t(40, 0.5), None); // idle: dim, no gradient
        assert_eq!(velocity_t(50, 0.5), Some(1.0)); // ratio 1.0 -> green
        assert_eq!(velocity_t(75, 0.5), Some(0.0)); // ratio 1.5 -> red
        assert_eq!(velocity_t(100, 0.5), Some(0.0)); // ratio 2.0 -> clamped red
    }

    #[test]
    fn meter_sgr_selects_mode() {
        assert_eq!(meter_sgr(0.0, "91", false), "91"); // fallback: discrete band
        assert_eq!(meter_sgr(0.0, "91", true), "38;2;215;0;0"); // truecolor: ramp
    }
}
