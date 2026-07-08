//! Variable layer for the status line (v1 §5/§6/§7).
//!
//! [`build_vars`] turns the incoming JSON payload into the `name -> String` map
//! consumed by the format engine ([`crate::format::render`]), reproducing the
//! current `main.rs` per-variable formatting exactly. [`needs_git`] reports
//! whether the chosen template references any git-derived variable (so `main`
//! can skip the repository scan otherwise). [`DEFAULT`] is the built-in template
//! that must reproduce the current status line.
//!
//! `build_vars` reproduces the current `main.rs` per-variable formatting exactly,
//! reusing the crate-root helpers (`round0`, `ratecol`, `velocity_col`,
//! `str_field`, `num_opt`, `basename`). `DEFAULT` is authored via group-abutment
//! so the golden-master acceptance test reproduces the current status line.

use std::collections::{BTreeMap, BTreeSet};

/// Built-in default template. Reproduces the current status line: each segment
/// after `dir` carries a leading `  ` separator inside a group gated by the
/// segment's variable, so the separator collapses with the segment when the
/// variable is undefined. Labels join their (often pre-colored) value with the
/// tight `+` concat join, so the value keeps its own color outside the `dim`
/// label and still gates the segment. Model+effort abut via `magenta(model)`
/// followed by `(magenta(effort))` — the effort call sits in its own group
/// (a gating boundary) so it is optional: only `model` gates the segment. The
/// worktree badge `⌥name` follows the branch/git segment in its own group gated
/// by `worktree` (dim, like the labels), so it shows only inside a linked git
/// worktree. The rate-limit countdowns tuck their `/↻` prefix inside a group
/// gated by `*_reset_in`.
pub const DEFAULT: &str = "cyan(dir)\
(  sgr('1;32', ' '+branch)( c_git))\
(  dim('\u{2325}'+worktree))\
(  c_pr)\
(  magenta(model)(magenta(effort)))\
(  dim('ctx:')+c_ctx)\
(  dim(cost))\
(  dim('S:')+c_session_pct)(dim('/\u{21bb}'+session_reset_in))\
(  dim('W:')+c_weekly_pct)(dim('/\u{21bb}'+weekly_reset_in))\
(  c_vim)";

/// Named, ready-to-use templates selectable with `--preset <name>` (the recipes
/// documented in the README's "Example templates" section). `default` is
/// [`DEFAULT`]; the rest are single-line variants. This order is what
/// `--list-presets` prints. Every preset degrades gracefully — an absent
/// segment collapses with its separator — exactly like `DEFAULT`.
pub const PRESETS: &[(&str, &str)] = &[
    ("default", DEFAULT),
    ("minimal", "cyan(dir)(  magenta(model)(magenta(effort)))(  dim('ctx:')+c_ctx)"),
    ("git", "cyan(dir)(  sgr('1;32', ' '+branch)( c_git))(  magenta(model)(magenta(effort)))(  dim('ctx:')+c_ctx)"),
    ("two-line", "cyan(dir)(  sgr('1;32', ' '+branch)( c_git))(  c_pr)br((magenta(model)(magenta(effort)))(  dim('ctx:')+c_ctx)(  dim(cost))(  dim('S:')+c_session_pct)(  dim('W:')+c_weekly_pct)(  c_vim))"),
    ("usage", "cyan(dir)(  dim('ctx ')+c_ctx)(  dim('cost ')+green(cost))(  dim('5h ')+c_session_pct(dim('/\u{21bb}'+session_reset_in)))(  dim('7d ')+c_weekly_pct(dim('/\u{21bb}'+weekly_reset_in)))"),
    ("truecolor", "hex('#5f8700', dir)(  dim('\u{2502}')  hex('#87afff', model)(hex('#5f87d7', effort)))(  dim('\u{2502}')  dim('ctx ')+c_ctx)(  dim('\u{2502}')  c_git)"),
];

/// Look up a preset template by name (see [`PRESETS`]); `None` if unknown.
pub fn preset(name: &str) -> Option<&'static str> {
    PRESETS.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
}

/// The preset names in list order — for `--list-presets` and error hints.
pub fn preset_names() -> Vec<&'static str> {
    PRESETS.iter().map(|(n, _)| *n).collect()
}

/// Every variable name the tool can produce — the registry passed to the format
/// engine. Inside a group or call, an identifier in this set substitutes its
/// value (and gates its group when undefined); any other identifier is literal
/// text. Kept in sync with the keys [`build_vars`] / `insert_git_vars` emit.
pub const VAR_NAMES: &[&str] = &[
    "current_dir", "project_dir", "dir", "worktree",
    "session_id", "session_name",
    "model", "model_full", "model_name", "effort", "effort_full",
    "ctx", "c_ctx", "ctx_raw",
    "cost", "cost_raw",
    "session_pct", "c_session_pct", "session_raw", "session_secs", "session_reset_in",
    "weekly_pct", "c_weekly_pct", "weekly_raw", "weekly_secs", "weekly_reset_in",
    "pr", "c_pr", "pr_number", "pr_state", "pr_url",
    "vim", "c_vim",
    "branch", "git", "c_git", "staged", "modified", "untracked",
];

/// Build the variable-name registry (the set of [`VAR_NAMES`]) for the engine.
pub fn registry() -> std::collections::HashSet<String> {
    VAR_NAMES.iter().map(|s| s.to_string()).collect()
}

/// Build the `name -> value` variable map from the status-line JSON payload,
/// reproducing the current `main.rs` per-variable formatting exactly.
///
/// `now` is injected unix-seconds (so rate-limit countdown/velocity are
/// deterministic under test); `home` is the value of `$HOME` used for the `~`
/// collapse in the `dir` block. `truecolor` selects the color style of the three
/// band-driven meters (`c_ctx`, `c_session_pct`, `c_weekly_pct`): when `true`
/// they emit a smooth `38;2;r;g;b` gradient, when `false` the exact discrete
/// ANSI/256 bands as before (every other variable is unaffected). Values that
/// carry no meaningful content are OMITTED (the render engine treats
/// absent/empty as undefined). Git-derived variables are never produced here —
/// `main` inserts them after a repo scan.
pub fn build_vars(
    json: &serde_json::Value,
    now: i64,
    home: &str,
    truecolor: bool,
) -> BTreeMap<String, String> {
    let mut v: BTreeMap<String, String> = BTreeMap::new();

    // jq-like nested lookup returning Null for any missing path segment.
    let get = |path: &[&str]| -> serde_json::Value {
        let mut cur = json;
        for k in path {
            cur = match cur.get(k) {
                Some(x) => x,
                None => return serde_json::Value::Null,
            };
        }
        cur.clone()
    };

    // --- gather raw fields up front ---
    let cwd = crate::str_field(&get(&["workspace", "current_dir"]));
    let proj = crate::str_field(&get(&["workspace", "project_dir"]));
    let model_id = crate::str_field(&get(&["model", "id"]));
    let model_name = crate::str_field(&get(&["model", "display_name"]));
    let effort = crate::str_field(&get(&["effort", "level"]));
    let vim = crate::str_field(&get(&["vim", "mode"]));
    // Per-session identifiers straight from the payload — handy as raw text and,
    // notably, to key an external per-session file (e.g. a template that reads
    // `~/.dir/` + session_id + `.json`).
    let session_id = crate::str_field(&get(&["session_id"]));
    let session_name = crate::str_field(&get(&["session_name"]));
    // Populated for any linked git worktree (`git worktree add`), absent in the
    // main working tree — comes straight from the payload, no repo scan needed.
    let worktree = crate::str_field(&get(&["workspace", "git_worktree"]));

    let rem = crate::num_opt(&get(&["context_window", "remaining_percentage"]));
    let cost = crate::num_opt(&get(&["cost", "total_cost_usd"]));
    let h5 = crate::num_opt(&get(&["rate_limits", "five_hour", "used_percentage"]));
    let h5rst = crate::num_opt(&get(&["rate_limits", "five_hour", "resets_at"]));
    let wk = crate::num_opt(&get(&["rate_limits", "seven_day", "used_percentage"]));
    let wkrst = crate::num_opt(&get(&["rate_limits", "seven_day", "resets_at"]));

    // pr.number may be a JSON string or number; empty string => no PR.
    let prnum = match get(&["pr", "number"]) {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => s,
        _ => String::new(),
    };
    let prurl = crate::str_field(&get(&["pr", "url"]));
    let prstate = crate::str_field(&get(&["pr", "review_state"]));

    // --- workspace (raw) ---
    if !cwd.is_empty() {
        v.insert("current_dir".to_string(), cwd.clone());
    }
    if !proj.is_empty() {
        v.insert("project_dir".to_string(), proj.clone());
    }
    if !worktree.is_empty() {
        v.insert("worktree".to_string(), worktree);
    }
    if !session_id.is_empty() {
        v.insert("session_id".to_string(), session_id);
    }
    if !session_name.is_empty() {
        v.insert("session_name".to_string(), session_name);
    }

    // --- dir block ---
    let dir = if cwd == home || cwd.is_empty() {
        "~".to_string()
    } else if !proj.is_empty() && cwd != proj && cwd.starts_with(&format!("{}/", proj)) {
        let parent = std::path::Path::new(&cwd).parent();
        if parent == Some(std::path::Path::new(&proj)) {
            format!("{}/{}", crate::basename(&proj), crate::basename(&cwd))
        } else {
            format!("{}/\u{2026}/{}", crate::basename(&proj), crate::basename(&cwd))
        }
    } else {
        crate::basename(&cwd)
    };
    v.insert("dir".to_string(), dir);

    // --- model / effort ---
    if !model_id.is_empty() {
        v.insert("model_full".to_string(), model_id.clone());
        let base = model_id.strip_prefix("claude-").unwrap_or(&model_id);
        if !base.is_empty() {
            v.insert("model".to_string(), base.to_string());
        }
    }
    // Human-friendly model name from the payload (e.g. "Opus 4.8"), distinct from
    // the id-derived `model`/`model_full`.
    if !model_name.is_empty() {
        v.insert("model_name".to_string(), model_name);
    }
    if !effort.is_empty() {
        v.insert("effort_full".to_string(), effort.clone());
        let code = match effort.as_str() {
            "xhigh" => "xh",
            "high" => "hi",
            "medium" => "md",
            "low" => "lo",
            "max" => "max",
            _ => "",
        };
        if !code.is_empty() {
            v.insert("effort".to_string(), code.to_string());
        }
    }

    // --- context window ---
    if let Some(remv) = rem {
        let r = crate::round0(remv);
        v.insert("ctx".to_string(), format!("{r}%"));
        v.insert("ctx_raw".to_string(), format!("{r}"));
        let band = if r > 70 {
            "32"
        } else if r > 40 {
            "33"
        } else if r > 20 {
            "38;5;208"
        } else {
            "91"
        };
        let sgr = crate::meter_sgr(crate::ctx_t(r), band, truecolor);
        v.insert("c_ctx".to_string(), format!("\x1b[{sgr}m{r}%\x1b[0m"));
    }

    // --- cost ---
    if let Some(cv) = cost {
        v.insert("cost_raw".to_string(), format!("{cv:.2}"));
        if cv >= 0.005 {
            v.insert("cost".to_string(), format!("${cv:.2}"));
        }
    }

    // --- session (5h) rate limit ---
    const FIVE_HOUR: f64 = 5.0 * 3600.0;
    if let Some(h5v) = h5 {
        let p = crate::round0(h5v);
        v.insert("session_pct".to_string(), format!("{p}%"));
        v.insert("session_raw".to_string(), format!("{p}"));
        let secs = h5rst.map(|rst| rst as i64 - now).filter(|s| *s > 0);
        if let Some(s) = secs {
            v.insert("session_secs".to_string(), format!("{s}"));
            let h = s / 3600;
            let m = (s % 3600) / 60;
            let cd = if h > 0 {
                format!("{h}h{m}m")
            } else {
                format!("{m}m")
            };
            v.insert("session_reset_in".to_string(), cd);
        }
        let sgr: String = match secs {
            Some(s) => {
                let elapsed = 1.0 - s as f64 / FIVE_HOUR;
                match crate::velocity_t(p, elapsed) {
                    Some(t) => crate::meter_sgr(t, crate::velocity_col(p, elapsed), truecolor),
                    None => "2".to_string(), // idle: dim in both color modes
                }
            }
            None if p >= 50 => crate::meter_sgr(crate::level_t(p), crate::ratecol(p), truecolor),
            None => "2".to_string(),
        };
        v.insert("c_session_pct".to_string(), format!("\x1b[{sgr}m{p}%\x1b[0m"));
    }

    // --- weekly (7d) rate limit ---
    const SEVEN_DAY: f64 = 7.0 * 86400.0;
    if let Some(wkv) = wk {
        let p = crate::round0(wkv);
        v.insert("weekly_pct".to_string(), format!("{p}%"));
        v.insert("weekly_raw".to_string(), format!("{p}"));
        let secs = wkrst.map(|rst| rst as i64 - now).filter(|s| *s > 0);
        if let Some(s) = secs {
            v.insert("weekly_secs".to_string(), format!("{s}"));
            let d = s / 86400;
            let h = (s % 86400) / 3600;
            let cd = if d > 0 {
                format!("{d}d{h}h")
            } else if h > 0 {
                format!("{h}h")
            } else {
                format!("{}m", s / 60)
            };
            v.insert("weekly_reset_in".to_string(), cd);
        }
        let sgr: String = match secs {
            Some(s) => {
                let elapsed = 1.0 - s as f64 / SEVEN_DAY;
                match crate::velocity_t(p, elapsed) {
                    Some(t) => crate::meter_sgr(t, crate::velocity_col(p, elapsed), truecolor),
                    None => "2".to_string(), // idle: dim in both color modes
                }
            }
            None if p >= 50 => crate::meter_sgr(crate::level_t(p), crate::ratecol(p), truecolor),
            None => "2".to_string(),
        };
        v.insert("c_weekly_pct".to_string(), format!("\x1b[{sgr}m{p}%\x1b[0m"));
    }

    // --- PR badge ---
    if !prnum.is_empty() {
        v.insert("pr".to_string(), format!("#{prnum}"));
        v.insert("pr_number".to_string(), prnum.clone());
        if !prstate.is_empty() {
            v.insert("pr_state".to_string(), prstate.clone());
        }
        if !prurl.is_empty() {
            v.insert("pr_url".to_string(), prurl.clone());
        }
        let prc = match prstate.as_str() {
            "approved" => "\x1b[32m",
            "changes_requested" => "\x1b[31m",
            _ => "\x1b[2m",
        };
        let cpr = if !prurl.is_empty() {
            format!("{prc}\x1b]8;;{prurl}\x07#{prnum}\x1b]8;;\x07\x1b[0m")
        } else {
            format!("{prc}#{prnum}\x1b[0m")
        };
        v.insert("c_pr".to_string(), cpr);
    }

    // --- vim mode ---
    if !vim.is_empty() {
        v.insert("vim".to_string(), vim.clone());
        let code = if vim == "INSERT" { "32" } else { "34" };
        v.insert("c_vim".to_string(), format!("\x1b[{code}m{vim}\x1b[0m"));
    }

    v
}

/// True if `refs` references any git-derived variable, i.e. it intersects
/// {`branch`, `git`, `c_git`, `staged`, `modified`, `untracked`}.
pub fn needs_git(refs: &BTreeSet<String>) -> bool {
    const GIT_VARS: [&str; 6] = ["branch", "git", "c_git", "staged", "modified", "untracked"];
    GIT_VARS.iter().any(|k| refs.contains(*k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Golden-master fixtures (git-less, resets_at-less), vendored under the
    // crate. Resolved via CARGO_MANIFEST_DIR so the tests are portable — no
    // machine-specific absolute path.
    const GOLDEN_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden");

    // A HOME that never prefixes the `/tmp/...` fixture paths, so `dir` never
    // collapses to `~` in the golden test.
    const HOME_NB: &str = "/home/nobody";

    /// Parse `json` and build the variable map (discrete-band colors, the
    /// pre-gradient default — golden masters lock this exact output).
    fn bv(json: &str, now: i64, home: &str) -> BTreeMap<String, String> {
        let v: serde_json::Value = serde_json::from_str(json).expect("valid json");
        build_vars(&v, now, home, false)
    }

    /// Like [`bv`] but with the truecolor gradient enabled for the meters.
    fn bv_tc(json: &str, now: i64, home: &str) -> BTreeMap<String, String> {
        let v: serde_json::Value = serde_json::from_str(json).expect("valid json");
        build_vars(&v, now, home, true)
    }

    /// Fetch a variable as `Option<&str>` for concise assertions.
    fn get<'a>(m: &'a BTreeMap<String, String>, k: &str) -> Option<&'a str> {
        m.get(k).map(String::as_str)
    }

    /// Build a `BTreeSet<String>` from string literals (for `needs_git`).
    fn bset(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// Remove empty styled runs — regex `\x1b\[[0-9;]*m\x1b\[0m` → `` — so the
    /// clean engine's output can be compared to the current code's golden output
    /// (which emits a few such no-op artifacts). std-only manual scanner.
    fn norm(s: &str) -> String {
        let b = s.as_bytes();
        let n = b.len();
        let mut out: Vec<u8> = Vec::with_capacity(n);
        let mut i = 0;
        while i < n {
            if b[i] == 0x1b && i + 1 < n && b[i + 1] == b'[' {
                // opener: ESC '[' [0-9;]* 'm'
                let mut j = i + 2;
                while j < n && (b[j].is_ascii_digit() || b[j] == b';') {
                    j += 1;
                }
                if j < n && b[j] == b'm' {
                    let k = j + 1;
                    // reset immediately after: ESC '[' '0' 'm'
                    if k + 3 < n
                        && b[k] == 0x1b
                        && b[k + 1] == b'['
                        && b[k + 2] == b'0'
                        && b[k + 3] == b'm'
                    {
                        i = k + 4;
                        continue;
                    }
                }
            }
            out.push(b[i]);
            i += 1;
        }
        String::from_utf8(out).expect("norm keeps valid utf-8")
    }

    // ============================ dir block ============================

    #[test]
    fn dir_collapses_to_home() {
        let m = bv(r#"{"workspace":{"current_dir":"/home/u"}}"#, 0, "/home/u");
        assert_eq!(get(&m, "dir"), Some("~"));
        assert_eq!(get(&m, "current_dir"), Some("/home/u"));
        assert_eq!(get(&m, "project_dir"), None);
    }

    #[test]
    fn dir_empty_cwd_is_home() {
        let m = bv(r#"{"workspace":{"current_dir":""}}"#, 0, "/home/u");
        assert_eq!(get(&m, "dir"), Some("~"));
        // empty current_dir carries no value -> omitted
        assert_eq!(get(&m, "current_dir"), None);
    }

    #[test]
    fn dir_proj_slash_dir_when_direct_child() {
        let m = bv(
            r#"{"workspace":{"current_dir":"/tmp/acme/src","project_dir":"/tmp/acme"}}"#,
            0,
            HOME_NB,
        );
        assert_eq!(get(&m, "dir"), Some("acme/src"));
        assert_eq!(get(&m, "current_dir"), Some("/tmp/acme/src"));
        assert_eq!(get(&m, "project_dir"), Some("/tmp/acme"));
    }

    #[test]
    fn dir_proj_ellipsis_dir_when_deeper() {
        let m = bv(
            r#"{"workspace":{"current_dir":"/tmp/acme/a/b/c","project_dir":"/tmp/acme"}}"#,
            0,
            HOME_NB,
        );
        assert_eq!(get(&m, "dir"), Some("acme/\u{2026}/c"));
    }

    #[test]
    fn dir_basename_when_no_project() {
        let m = bv(r#"{"workspace":{"current_dir":"/tmp/foo/bar"}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "dir"), Some("bar"));
    }

    #[test]
    fn dir_basename_when_cwd_equals_project() {
        let m = bv(
            r#"{"workspace":{"current_dir":"/tmp/acme","project_dir":"/tmp/acme"}}"#,
            0,
            HOME_NB,
        );
        assert_eq!(get(&m, "dir"), Some("acme"));
    }

    #[test]
    fn dir_basename_when_cwd_outside_project() {
        let m = bv(
            r#"{"workspace":{"current_dir":"/tmp/other","project_dir":"/tmp/acme"}}"#,
            0,
            HOME_NB,
        );
        assert_eq!(get(&m, "dir"), Some("other"));
    }

    // ============================ worktree ============================

    #[test]
    fn worktree_from_git_worktree_field() {
        let m = bv(
            r#"{"workspace":{"current_dir":"/tmp/acme","git_worktree":"my-feature"}}"#,
            0,
            HOME_NB,
        );
        assert_eq!(get(&m, "worktree"), Some("my-feature"));
    }

    #[test]
    fn worktree_omitted_when_absent() {
        // Main working tree: no git_worktree in the payload -> var omitted.
        let m = bv(r#"{"workspace":{"current_dir":"/tmp/acme"}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "worktree"), None);
    }

    #[test]
    fn worktree_omitted_when_empty() {
        let m = bv(r#"{"workspace":{"git_worktree":""}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "worktree"), None);
    }

    #[test]
    fn worktree_is_not_a_git_var() {
        // Comes from JSON, so it must never trigger the repo scan.
        assert!(!needs_git(&bset(&["worktree"])));
    }

    #[test]
    fn default_renders_worktree_badge() {
        // The DEFAULT segment `(  dim('⌥'+worktree))` shows a dim ⌥name badge.
        let vars: HashMap<String, String> = bv(
            r#"{"workspace":{"current_dir":"/tmp/acme","git_worktree":"my-feature"}}"#,
            1_000_000,
            HOME_NB,
        )
        .into_iter()
        .collect();
        let out = crate::format::render(DEFAULT, &vars, &registry()).expect("DEFAULT parses");
        assert!(
            out.contains("\x1b[2m\u{2325}my-feature\x1b[0m"),
            "worktree badge missing, got: {out:?}"
        );
    }

    #[test]
    fn default_omits_worktree_badge_in_main_tree() {
        let vars: HashMap<String, String> = bv(
            r#"{"workspace":{"current_dir":"/tmp/acme"}}"#,
            1_000_000,
            HOME_NB,
        )
        .into_iter()
        .collect();
        let out = crate::format::render(DEFAULT, &vars, &registry()).expect("DEFAULT parses");
        assert!(!out.contains('\u{2325}'), "unexpected worktree badge, got: {out:?}");
    }

    // ========================= model / effort =========================

    #[test]
    fn model_strips_claude_prefix() {
        let m = bv(r#"{"model":{"id":"claude-opus-4-8[1m]"}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "model"), Some("opus-4-8[1m]"));
        assert_eq!(get(&m, "model_full"), Some("claude-opus-4-8[1m]"));
    }

    #[test]
    fn model_without_prefix_verbatim() {
        let m = bv(r#"{"model":{"id":"gpt-4"}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "model"), Some("gpt-4"));
        assert_eq!(get(&m, "model_full"), Some("gpt-4"));
    }

    #[test]
    fn model_omitted_when_absent() {
        let m = bv(r#"{}"#, 0, HOME_NB);
        assert_eq!(get(&m, "model"), None);
        assert_eq!(get(&m, "model_full"), None);
    }

    #[test]
    fn effort_codes_all_known_levels() {
        for (level, code) in [
            ("xhigh", "xh"),
            ("high", "hi"),
            ("medium", "md"),
            ("low", "lo"),
            ("max", "max"),
        ] {
            let m = bv(&format!(r#"{{"effort":{{"level":"{level}"}}}}"#), 0, HOME_NB);
            assert_eq!(get(&m, "effort"), Some(code), "level {level}");
            assert_eq!(get(&m, "effort_full"), Some(level), "level {level}");
        }
    }

    #[test]
    fn effort_omitted_for_unknown_level_but_full_kept() {
        let m = bv(r#"{"effort":{"level":"giant"}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "effort"), None);
        assert_eq!(get(&m, "effort_full"), Some("giant"));
    }

    #[test]
    fn effort_omitted_when_absent() {
        let m = bv(r#"{"model":{"id":"claude-sonnet-5"}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "effort"), None);
        assert_eq!(get(&m, "effort_full"), None);
    }

    // ============================== ctx ==============================

    #[test]
    fn ctx_green_above_70() {
        let m = bv(r#"{"context_window":{"remaining_percentage":88}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "ctx"), Some("88%"));
        assert_eq!(get(&m, "c_ctx"), Some("\x1b[32m88%\x1b[0m"));
        assert_eq!(get(&m, "ctx_raw"), Some("88"));
    }

    #[test]
    fn ctx_yellow_band() {
        let m = bv(r#"{"context_window":{"remaining_percentage":62}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "c_ctx"), Some("\x1b[33m62%\x1b[0m"));
    }

    #[test]
    fn ctx_orange_band() {
        let m = bv(r#"{"context_window":{"remaining_percentage":40}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "c_ctx"), Some("\x1b[38;5;208m40%\x1b[0m"));
    }

    #[test]
    fn ctx_red_band() {
        let m = bv(r#"{"context_window":{"remaining_percentage":15}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "c_ctx"), Some("\x1b[91m15%\x1b[0m"));
    }

    #[test]
    fn ctx_boundary_70_is_yellow() {
        // 70 is NOT > 70, so falls to the >40 band.
        let m = bv(r#"{"context_window":{"remaining_percentage":70}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "c_ctx"), Some("\x1b[33m70%\x1b[0m"));
    }

    #[test]
    fn ctx_boundary_20_is_red() {
        // 20 is NOT > 20, so falls to the red band.
        let m = bv(r#"{"context_window":{"remaining_percentage":20}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "c_ctx"), Some("\x1b[91m20%\x1b[0m"));
    }

    #[test]
    fn ctx_rounds_half_to_even() {
        // round0(88.6) == 89 via printf %.0f semantics.
        let m = bv(r#"{"context_window":{"remaining_percentage":88.6}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "ctx"), Some("89%"));
        assert_eq!(get(&m, "ctx_raw"), Some("89"));
    }

    #[test]
    fn ctx_omitted_when_absent() {
        let m = bv(r#"{}"#, 0, HOME_NB);
        assert_eq!(get(&m, "ctx"), None);
        assert_eq!(get(&m, "c_ctx"), None);
        assert_eq!(get(&m, "ctx_raw"), None);
    }

    // ============================== cost ==============================

    #[test]
    fn cost_formats_two_decimals() {
        let m = bv(r#"{"cost":{"total_cost_usd":1.37}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "cost"), Some("$1.37"));
        assert_eq!(get(&m, "cost_raw"), Some("1.37"));
    }

    #[test]
    fn cost_pads_trailing_zero() {
        let m = bv(r#"{"cost":{"total_cost_usd":2.5}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "cost"), Some("$2.50"));
        assert_eq!(get(&m, "cost_raw"), Some("2.50"));
    }

    #[test]
    fn cost_omitted_below_threshold() {
        // total_cost_usd < 0.005 -> cost omitted.
        let m = bv(r#"{"cost":{"total_cost_usd":0.004}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "cost"), None);
    }

    #[test]
    fn cost_omitted_when_absent() {
        let m = bv(r#"{}"#, 0, HOME_NB);
        assert_eq!(get(&m, "cost"), None);
        assert_eq!(get(&m, "cost_raw"), None);
    }

    // ===================== session (5h) rate limit =====================

    #[test]
    fn session_velocity_and_countdown_with_reset() {
        // §7 example: now=1000, used 72, resets_at 1000+5640 -> secs 5640.
        // velocity_col(72, 1-5640/18000) = "33"; countdown "1h34m".
        let m = bv(
            r#"{"rate_limits":{"five_hour":{"used_percentage":72,"resets_at":6640}}}"#,
            1000,
            HOME_NB,
        );
        assert_eq!(get(&m, "session_pct"), Some("72%"));
        assert_eq!(get(&m, "c_session_pct"), Some("\x1b[33m72%\x1b[0m"));
        assert_eq!(get(&m, "session_raw"), Some("72"));
        assert_eq!(get(&m, "session_secs"), Some("5640"));
        assert_eq!(get(&m, "session_reset_in"), Some("1h34m"));
    }

    #[test]
    fn session_countdown_minutes_only_and_green_pace() {
        // now=0, used 90, resets_at 600 -> secs 600; h==0 -> "10m".
        // velocity_col(90, 1-600/18000) = "32".
        let m = bv(
            r#"{"rate_limits":{"five_hour":{"used_percentage":90,"resets_at":600}}}"#,
            0,
            HOME_NB,
        );
        assert_eq!(get(&m, "c_session_pct"), Some("\x1b[32m90%\x1b[0m"));
        assert_eq!(get(&m, "session_reset_in"), Some("10m"));
        assert_eq!(get(&m, "session_secs"), Some("600"));
    }

    #[test]
    fn session_no_reset_uses_ratecol_when_high() {
        // No resets_at; p=72>=50 -> ratecol(72) == "38;5;208". No countdown/secs.
        let m = bv(
            r#"{"rate_limits":{"five_hour":{"used_percentage":72}}}"#,
            0,
            HOME_NB,
        );
        assert_eq!(get(&m, "c_session_pct"), Some("\x1b[38;5;208m72%\x1b[0m"));
        assert_eq!(get(&m, "session_reset_in"), None);
        assert_eq!(get(&m, "session_secs"), None);
    }

    #[test]
    fn session_no_reset_ratecol_boundaries() {
        let m96 = bv(r#"{"rate_limits":{"five_hour":{"used_percentage":96}}}"#, 0, HOME_NB);
        assert_eq!(get(&m96, "c_session_pct"), Some("\x1b[1;31m96%\x1b[0m"));
        let m85 = bv(r#"{"rate_limits":{"five_hour":{"used_percentage":85}}}"#, 0, HOME_NB);
        assert_eq!(get(&m85, "c_session_pct"), Some("\x1b[91m85%\x1b[0m"));
    }

    #[test]
    fn session_no_reset_dim_when_low() {
        // No resets_at; p=41<50 -> dim "2".
        let m = bv(
            r#"{"rate_limits":{"five_hour":{"used_percentage":41}}}"#,
            0,
            HOME_NB,
        );
        assert_eq!(get(&m, "c_session_pct"), Some("\x1b[2m41%\x1b[0m"));
        assert_eq!(get(&m, "session_reset_in"), None);
    }

    #[test]
    fn session_past_reset_falls_back_to_ratecol() {
        // resets_at in the past -> secs<=0 treated as absent.
        let m = bv(
            r#"{"rate_limits":{"five_hour":{"used_percentage":72,"resets_at":1000}}}"#,
            2000,
            HOME_NB,
        );
        assert_eq!(get(&m, "c_session_pct"), Some("\x1b[38;5;208m72%\x1b[0m"));
        assert_eq!(get(&m, "session_reset_in"), None);
        assert_eq!(get(&m, "session_secs"), None);
    }

    #[test]
    fn session_omitted_when_absent() {
        let m = bv(r#"{}"#, 0, HOME_NB);
        assert_eq!(get(&m, "session_pct"), None);
        assert_eq!(get(&m, "c_session_pct"), None);
    }

    // ====================== weekly (7d) rate limit ======================

    #[test]
    fn weekly_velocity_and_day_hour_countdown() {
        // now=0, used 60, resets_at 200000 -> secs 200000; "2d7h".
        // velocity_col(60, 1-200000/604800) = "32".
        let m = bv(
            r#"{"rate_limits":{"seven_day":{"used_percentage":60,"resets_at":200000}}}"#,
            0,
            HOME_NB,
        );
        assert_eq!(get(&m, "weekly_pct"), Some("60%"));
        assert_eq!(get(&m, "c_weekly_pct"), Some("\x1b[32m60%\x1b[0m"));
        assert_eq!(get(&m, "weekly_raw"), Some("60"));
        assert_eq!(get(&m, "weekly_secs"), Some("200000"));
        assert_eq!(get(&m, "weekly_reset_in"), Some("2d7h"));
    }

    #[test]
    fn weekly_hours_only_countdown() {
        // secs 7200 -> d==0, h==2 -> "2h".
        let m = bv(
            r#"{"rate_limits":{"seven_day":{"used_percentage":55,"resets_at":7200}}}"#,
            0,
            HOME_NB,
        );
        assert_eq!(get(&m, "weekly_reset_in"), Some("2h"));
    }

    #[test]
    fn weekly_minutes_only_countdown() {
        // secs 1800 -> d==0, h==0 -> "30m".
        let m = bv(
            r#"{"rate_limits":{"seven_day":{"used_percentage":60,"resets_at":1800}}}"#,
            0,
            HOME_NB,
        );
        assert_eq!(get(&m, "weekly_reset_in"), Some("30m"));
    }

    #[test]
    fn weekly_no_reset_dim_when_low() {
        // No resets_at; p=41<50 -> dim "2" (matches golden p3).
        let m = bv(
            r#"{"rate_limits":{"seven_day":{"used_percentage":41}}}"#,
            0,
            HOME_NB,
        );
        assert_eq!(get(&m, "c_weekly_pct"), Some("\x1b[2m41%\x1b[0m"));
        assert_eq!(get(&m, "weekly_reset_in"), None);
        assert_eq!(get(&m, "weekly_secs"), None);
    }

    #[test]
    fn weekly_omitted_when_absent() {
        let m = bv(r#"{}"#, 0, HOME_NB);
        assert_eq!(get(&m, "weekly_pct"), None);
        assert_eq!(get(&m, "c_weekly_pct"), None);
    }

    // ============================ pr badge ============================

    #[test]
    fn pr_approved_with_url_osc8() {
        let m = bv(
            r#"{"pr":{"number":"128","url":"https://github.com/x/y/pull/128","review_state":"approved"}}"#,
            0,
            HOME_NB,
        );
        assert_eq!(get(&m, "pr"), Some("#128"));
        assert_eq!(
            get(&m, "c_pr"),
            Some("\x1b[32m\x1b]8;;https://github.com/x/y/pull/128\x07#128\x1b]8;;\x07\x1b[0m")
        );
        assert_eq!(get(&m, "pr_number"), Some("128"));
        assert_eq!(get(&m, "pr_state"), Some("approved"));
        assert_eq!(get(&m, "pr_url"), Some("https://github.com/x/y/pull/128"));
    }

    #[test]
    fn pr_changes_requested_no_url() {
        let m = bv(
            r#"{"pr":{"number":"5","review_state":"changes_requested"}}"#,
            0,
            HOME_NB,
        );
        assert_eq!(get(&m, "pr"), Some("#5"));
        assert_eq!(get(&m, "c_pr"), Some("\x1b[31m#5\x1b[0m"));
        assert_eq!(get(&m, "pr_url"), None);
    }

    #[test]
    fn pr_other_state_is_dim() {
        let m = bv(r#"{"pr":{"number":"7","review_state":"commented"}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "c_pr"), Some("\x1b[2m#7\x1b[0m"));
    }

    #[test]
    fn pr_number_as_json_integer() {
        let m = bv(r#"{"pr":{"number":42}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "pr"), Some("#42"));
        assert_eq!(get(&m, "pr_number"), Some("42"));
        // no review_state -> dim
        assert_eq!(get(&m, "c_pr"), Some("\x1b[2m#42\x1b[0m"));
    }

    #[test]
    fn pr_omitted_when_number_empty() {
        let m = bv(r#"{"pr":{"number":"","review_state":"approved"}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "pr"), None);
        assert_eq!(get(&m, "c_pr"), None);
    }

    #[test]
    fn pr_omitted_when_absent() {
        let m = bv(r#"{}"#, 0, HOME_NB);
        assert_eq!(get(&m, "pr"), None);
        assert_eq!(get(&m, "c_pr"), None);
    }

    // ============================== vim ==============================

    #[test]
    fn vim_insert_is_green() {
        let m = bv(r#"{"vim":{"mode":"INSERT"}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "vim"), Some("INSERT"));
        assert_eq!(get(&m, "c_vim"), Some("\x1b[32mINSERT\x1b[0m"));
    }

    #[test]
    fn vim_other_mode_is_blue() {
        let m = bv(r#"{"vim":{"mode":"NORMAL"}}"#, 0, HOME_NB);
        assert_eq!(get(&m, "vim"), Some("NORMAL"));
        assert_eq!(get(&m, "c_vim"), Some("\x1b[34mNORMAL\x1b[0m"));
    }

    #[test]
    fn vim_omitted_when_absent() {
        let m = bv(r#"{}"#, 0, HOME_NB);
        assert_eq!(get(&m, "vim"), None);
        assert_eq!(get(&m, "c_vim"), None);
    }

    // ===================== build_vars omits git vars =====================

    #[test]
    fn build_vars_never_emits_git_vars() {
        // git-derived vars are inserted by main after a repo scan, never by build_vars.
        let m = bv(
            r#"{"workspace":{"current_dir":"/tmp/acme"},"model":{"id":"claude-sonnet-5"}}"#,
            0,
            HOME_NB,
        );
        for k in ["branch", "git", "c_git", "staged", "modified", "untracked"] {
            assert_eq!(get(&m, k), None, "git var {k} must not come from build_vars");
        }
    }

    // ============================ needs_git (v1 §6) ============================

    #[test]
    fn needs_git_false_for_non_git_refs() {
        assert!(!needs_git(&bset(&["dir", "c_ctx"])));
    }

    #[test]
    fn needs_git_true_for_branch() {
        assert!(needs_git(&bset(&["branch"])));
    }

    #[test]
    fn needs_git_true_for_c_git() {
        assert!(needs_git(&bset(&["c_git"])));
    }

    #[test]
    fn needs_git_false_for_empty_set() {
        assert!(!needs_git(&bset(&[])));
    }

    #[test]
    fn needs_git_true_for_each_git_var() {
        for k in ["branch", "git", "c_git", "staged", "modified", "untracked"] {
            assert!(needs_git(&bset(&[k])), "ref {k} should require git");
        }
    }

    #[test]
    fn needs_git_true_when_mixed_with_non_git() {
        assert!(needs_git(&bset(&["dir", "model", "staged"])));
    }

    // ==================== golden-master acceptance (v1 §7) ====================

    /// Render the DEFAULT template against build_vars(pN.json) and compare to
    /// pN.out under `norm` (empty styled runs stripped). Goldens are git-less &
    /// resets_at-less, so `now` is irrelevant and no git vars appear.
    fn golden_case(name: &str) {
        let json = std::fs::read_to_string(format!("{GOLDEN_DIR}/{name}.json"))
            .unwrap_or_else(|e| panic!("read {name}.json: {e}"));
        let expected = std::fs::read_to_string(format!("{GOLDEN_DIR}/{name}.out"))
            .unwrap_or_else(|e| panic!("read {name}.out: {e}"));
        let vars: HashMap<String, String> = bv(&json, 1_000_000, HOME_NB).into_iter().collect();
        let rendered =
            crate::format::render(DEFAULT, &vars, &registry()).expect("DEFAULT parses");
        assert_eq!(norm(&rendered), norm(&expected), "golden {name}");
    }

    #[test]
    fn golden_p1() {
        golden_case("p1");
    }

    #[test]
    fn golden_p2() {
        golden_case("p2");
    }

    #[test]
    fn golden_p3() {
        golden_case("p3");
    }

    #[test]
    fn golden_p4() {
        golden_case("p4");
    }

    // Sanity: norm strips exactly the empty styled runs present in p3's golden.
    #[test]
    fn norm_strips_empty_styled_runs() {
        assert_eq!(norm("\x1b[2m\x1b[0m"), "");
        assert_eq!(norm("\x1b[38;5;208m72%\x1b[0m\x1b[2m\x1b[0m"), "\x1b[38;5;208m72%\x1b[0m");
        // a non-empty styled run is preserved
        assert_eq!(norm("\x1b[32m88%\x1b[0m"), "\x1b[32m88%\x1b[0m");
    }

    // ============================ presets ============================

    #[test]
    fn preset_lookup_and_names() {
        assert_eq!(preset("default"), Some(DEFAULT));
        assert!(preset("minimal").is_some());
        assert_eq!(preset("nope"), None);
        assert_eq!(
            preset_names(),
            vec!["default", "minimal", "git", "two-line", "usage", "truecolor"]
        );
    }

    #[test]
    fn every_preset_parses_and_renders() {
        // Each built-in preset must parse and render (never error), and produce
        // visible output against a payload where its segments have data.
        let json = r#"{
            "workspace":{"current_dir":"/tmp/acme","project_dir":"/tmp/acme"},
            "model":{"id":"claude-opus-4-8[1m]"},"effort":{"level":"high"},
            "context_window":{"remaining_percentage":62},"cost":{"total_cost_usd":1.37},
            "rate_limits":{"five_hour":{"used_percentage":72,"resets_at":5640},
                           "seven_day":{"used_percentage":41,"resets_at":183600}},
            "pr":{"number":"128","review_state":"approved"},"vim":{"mode":"NORMAL"}
        }"#;
        let vars: HashMap<String, String> = bv(json, 0, HOME_NB).into_iter().collect();
        for (name, tmpl) in PRESETS {
            let out = crate::format::render(tmpl, &vars, &registry())
                .unwrap_or_else(|e| panic!("preset {name} failed to parse: {e}"));
            assert!(!out.is_empty(), "preset {name} rendered empty");
        }
    }

    // ================= truecolor gradient (c_ctx / meters) =================
    // `bv` (truecolor=false) must reproduce the discrete bands byte-for-byte;
    // `bv_tc` (truecolor=true) emits a smooth `38;2;r;g;b` ramp instead.

    #[test]
    fn ctx_truecolor_gradient_and_fallback() {
        // 88% remaining sits at the green end of the ramp; 15% is red-orange.
        let hi = bv_tc(r#"{"context_window":{"remaining_percentage":88}}"#, 0, HOME_NB);
        assert_eq!(get(&hi, "c_ctx"), Some("\x1b[38;2;95;175;0m88%\x1b[0m"));
        let lo = bv_tc(r#"{"context_window":{"remaining_percentage":15}}"#, 0, HOME_NB);
        assert_eq!(get(&lo, "c_ctx"), Some("\x1b[38;2;239;81;0m15%\x1b[0m"));
        // Fallback is unchanged from the discrete bands (regression lock).
        let fb = bv(r#"{"context_window":{"remaining_percentage":88}}"#, 0, HOME_NB);
        assert_eq!(get(&fb, "c_ctx"), Some("\x1b[32m88%\x1b[0m"));
    }

    #[test]
    fn session_velocity_truecolor_vs_band() {
        // Same payload as session_velocity_and_countdown_with_reset.
        let j = r#"{"rate_limits":{"five_hour":{"used_percentage":72,"resets_at":6640}}}"#;
        assert_eq!(get(&bv(j, 1000, HOME_NB), "c_session_pct"), Some("\x1b[33m72%\x1b[0m"));
        let tc = bv_tc(j, 1000, HOME_NB);
        let c = get(&tc, "c_session_pct").unwrap();
        assert!(
            c.starts_with("\x1b[38;2;") && c.ends_with("m72%\x1b[0m"),
            "expected a 38;2 ramp, got {c:?}"
        );
    }

    #[test]
    fn weekly_absolute_level_truecolor_vs_band() {
        // No resets_at, p>=50 -> absolute-level ramp (truecolor) / ratecol (band).
        let j = r#"{"rate_limits":{"seven_day":{"used_percentage":72}}}"#;
        assert_eq!(get(&bv(j, 0, HOME_NB), "c_weekly_pct"), Some("\x1b[38;5;208m72%\x1b[0m"));
        let tc = bv_tc(j, 0, HOME_NB);
        let c = get(&tc, "c_weekly_pct").unwrap();
        assert!(
            c.starts_with("\x1b[38;2;") && c.ends_with("m72%\x1b[0m"),
            "expected a 38;2 ramp, got {c:?}"
        );
    }

    #[test]
    fn meter_idle_stays_dim_in_both_modes() {
        // used < 50 -> idle -> dim "2" regardless of color mode (with a reset)...
        let j = r#"{"rate_limits":{"five_hour":{"used_percentage":30,"resets_at":600}}}"#;
        assert_eq!(get(&bv(j, 0, HOME_NB), "c_session_pct"), Some("\x1b[2m30%\x1b[0m"));
        assert_eq!(get(&bv_tc(j, 0, HOME_NB), "c_session_pct"), Some("\x1b[2m30%\x1b[0m"));
        // ...and on the no-reset low path.
        let k = r#"{"rate_limits":{"five_hour":{"used_percentage":41}}}"#;
        assert_eq!(get(&bv(k, 0, HOME_NB), "c_session_pct"), Some("\x1b[2m41%\x1b[0m"));
        assert_eq!(get(&bv_tc(k, 0, HOME_NB), "c_session_pct"), Some("\x1b[2m41%\x1b[0m"));
    }
}
