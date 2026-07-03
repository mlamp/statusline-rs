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

// Color for the 5h segment: a green->red gradient driven by time-to-reset,
// but the redness is capped by how much quota has been burned so a distant
// reset stays calm when usage is low ("only redden when constrained").
// Only called for p >= 50; secs is the (positive) seconds until reset.
fn five_time_col(p: i64, secs: i64) -> &'static str {
    // time severity: 0=green (reset near) .. 3=red (reset far)
    let ts = if secs < 2700 {
        0 // < 45m
    } else if secs < 5400 {
        1 // < 1.5h
    } else if secs < 9000 {
        2 // < 2.5h
    } else {
        3 // >= 2.5h
    };
    // usage cap: how red we're allowed to get for the current burn
    let cap = if p >= 85 {
        3
    } else if p >= 70 {
        2
    } else {
        1 // 50..70
    };
    match ts.min(cap) {
        0 => "32",        // green
        1 => "33",        // yellow
        2 => "38;5;208",  // orange
        _ => "91",        // red
    }
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

fn git_segment(cwd: &str) -> (String, String) {
    // Returns (branch, gstat) where gstat already includes the surrounding "()".
    let mut branch = String::new();
    let mut gstat = String::new();
    if cwd.is_empty() {
        return (branch, gstat);
    }
    let repo = match gix::discover(cwd) {
        Ok(r) => r,
        Err(_) => return (branch, gstat),
    };

    // branch --show-current, fallback to short HEAD
    if let Ok(Some(name)) = repo.head_name() {
        branch = name.shorten().to_string();
    }
    if branch.is_empty() {
        if let Ok(id) = repo.head_id() {
            branch = id.shorten_or_id().to_string();
        }
    }

    let (mut s, mut m, mut u) = (0i64, 0i64, 0i64);
    if let Ok(platform) = repo.status(gix::progress::Discard) {
        if let Ok(iter) = platform.into_iter(None) {
            use gix::status::index_worktree::Item as IwItem;
            use gix::status::Item;
            for item in iter.flatten() {
                match item {
                    Item::TreeIndex(_) => s += 1, // staged: col1 in [M,A,D,R,C]
                    Item::IndexWorktree(iw) => match iw {
                        IwItem::Modification { .. } => m += 1, // worktree col2 in [M,D]
                        IwItem::DirectoryContents { entry, .. } => {
                            if matches!(entry.status, gix::dir::entry::Status::Untracked) {
                                u += 1;
                            }
                        }
                        IwItem::Rewrite { .. } => {}
                    },
                }
            }
        }
    }

    let mut parts = String::new();
    if s > 0 {
        parts.push_str(&format!("{ESC}[32m+{s}{ESC}[0m"));
    }
    if m > 0 {
        if !parts.is_empty() {
            parts.push(' ');
        }
        parts.push_str(&format!("{ESC}[33m!{m}{ESC}[0m"));
    }
    if u > 0 {
        if !parts.is_empty() {
            parts.push(' ');
        }
        parts.push_str(&format!("{ESC}[2m?{u}{ESC}[0m"));
    }
    if !parts.is_empty() {
        gstat = format!("({parts}{ESC}[1;32m)");
    }
    (branch, gstat)
}

fn main() {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let json: serde_json::Value = serde_json::from_str(&input).unwrap_or(serde_json::Value::Null);

    let get = |path: &[&str]| -> serde_json::Value {
        let mut cur = &json;
        for k in path {
            cur = match cur.get(k) {
                Some(v) => v,
                None => return serde_json::Value::Null,
            };
        }
        cur.clone()
    };

    let cwd = str_field(&get(&["workspace", "current_dir"]));
    let proj = str_field(&get(&["workspace", "project_dir"]));
    let rem = num_opt(&get(&["context_window", "remaining_percentage"]));
    let model_id = str_field(&get(&["model", "id"]));
    let effort = str_field(&get(&["effort", "level"]));
    let cost = num_opt(&get(&["cost", "total_cost_usd"]));
    let ladd = num_opt(&get(&["cost", "total_lines_added"])).unwrap_or(0.0) as i64;
    let ldel = num_opt(&get(&["cost", "total_lines_removed"])).unwrap_or(0.0) as i64;
    let h5 = num_opt(&get(&["rate_limits", "five_hour", "used_percentage"]));
    let h5rst = num_opt(&get(&["rate_limits", "five_hour", "resets_at"]));
    let wk = num_opt(&get(&["rate_limits", "seven_day", "used_percentage"]));
    let wkrst = num_opt(&get(&["rate_limits", "seven_day", "resets_at"]));
    let vim = str_field(&get(&["vim", "mode"]));
    // pr.number is a string field in the payloads (empty => no PR)
    let prnum = {
        let v = get(&["pr", "number"]);
        match v {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::String(s) => s,
            _ => String::new(),
        }
    };
    let prurl = str_field(&get(&["pr", "url"]));
    let prstate = str_field(&get(&["pr", "review_state"]));

    let home = std::env::var("HOME").unwrap_or_default();

    // --- directory ---
    let dir = if cwd == home || cwd.is_empty() {
        "~".to_string()
    } else if !proj.is_empty() && cwd != proj && cwd.starts_with(&format!("{}/", proj)) {
        let parent = Path::new(&cwd).parent();
        if parent == Some(Path::new(&proj)) {
            format!("{}/{}", basename(&proj), basename(&cwd))
        } else {
            format!("{}/\u{2026}/{}", basename(&proj), basename(&cwd))
        }
    } else {
        basename(&cwd)
    };

    let (branch, gstat) = git_segment(&cwd);

    // --- PR badge ---
    let mut pr = String::new();
    if !prnum.is_empty() {
        let prc = match prstate.as_str() {
            "approved" => "\x1b[32m",
            "changes_requested" => "\x1b[31m",
            _ => "\x1b[2m",
        };
        if !prurl.is_empty() {
            pr = format!("{prc}{ESC}]8;;{prurl}\x07#{prnum}{ESC}]8;;\x07{ESC}[0m");
        } else {
            pr = format!("{prc}#{prnum}{ESC}[0m");
        }
    }

    // --- model: derive from .model.id, strip the "claude-" prefix, append the effort code ---
    //   e.g.  claude-opus-4-8[1m]  +  xhigh  ->  opus-4-8[1m]xh
    let mut mshort = String::new();
    if !model_id.is_empty() {
        let base = model_id.strip_prefix("claude-").unwrap_or(&model_id);
        let ef = match effort.as_str() {
            "xhigh" => "xh",
            "high" => "hi",
            "medium" => "md",
            "low" => "lo",
            "max" => "max",
            _ => "",
        };
        mshort = format!("{base}{ef}");
    }

    // --- context ---
    let mut ctx = String::new();
    if let Some(remv) = rem {
        let r = round0(remv);
        let c = if r > 70 {
            "\x1b[32m"
        } else if r > 40 {
            "\x1b[33m"
        } else if r > 20 {
            "\x1b[38;5;208m"
        } else {
            "\x1b[91m"
        };
        ctx = format!("{ESC}[2mctx:{ESC}[0m{c}{r}%{ESC}[0m");
    }

    // --- cost ---
    let mut costs = String::new();
    if let Some(cv) = cost {
        if cv >= 0.005 {
            costs = format!("{ESC}[2m${:.2}{ESC}[0m", cv);
        }
    }

    // --- lines ---
    let mut lines = String::new();
    if ladd > 0 {
        lines.push_str(&format!("{ESC}[32m+{ladd}{ESC}[0m"));
    }
    if ldel > 0 {
        if !lines.is_empty() {
            lines.push_str(&format!("{ESC}[2m/{ESC}[0m"));
        }
        lines.push_str(&format!("{ESC}[31m-{ldel}{ESC}[0m"));
    }

    // --- 5h rate limit ---
    // Always shown; a `↻` countdown to reset replaces the old wall-clock time.
    // Dim below 50% usage (idle), otherwise colored by time-to-reset (see
    // `five_time_col`). Falls back to the usage color when there's no reset ts.
    let mut l5 = String::new();
    if let Some(h5v) = h5 {
        let p = round0(h5v);
        let mut secs = None;
        if let Some(rst) = h5rst {
            let s = rst as i64 - now_secs();
            if s > 0 {
                secs = Some(s);
            }
        }
        let tpart = match secs {
            Some(s) => {
                let h = s / 3600;
                let m = (s % 3600) / 60;
                if h > 0 {
                    format!(" \u{21bb}{h}h{m}m")
                } else {
                    format!(" \u{21bb}{m}m")
                }
            }
            None => String::new(),
        };
        let col = if p < 50 {
            "\x1b[2m".to_string()
        } else {
            match secs {
                Some(s) => format!("{ESC}[{}m", five_time_col(p, s)),
                None => format!("{ESC}[{}m", ratecol(p)),
            }
        };
        l5 = format!("{col}5h({p}%){tpart}{ESC}[0m");
    }

    // --- weekly rate limit ---
    let mut lwk = String::new();
    if let Some(wkv) = wk {
        let p = round0(wkv);
        let mut cd = String::new();
        if let Some(rst) = wkrst {
            let secs = rst as i64 - now_secs();
            if secs > 0 {
                let d = secs / 86400;
                let h = (secs % 86400) / 3600;
                if d > 0 {
                    cd = format!("\u{21bb}{d}d{h}h");
                } else if h > 0 {
                    cd = format!("\u{21bb}{h}h");
                } else {
                    cd = format!("\u{21bb}{}m", secs / 60);
                }
            }
        }
        let wc = if p >= 50 {
            format!("{ESC}[{}m", ratecol(p))
        } else {
            "\x1b[2m".to_string()
        };
        let cdpart = if cd.is_empty() {
            String::new()
        } else {
            format!(" {cd}")
        };
        lwk = format!("{wc}Wk({p}%){cdpart}{ESC}[0m");
    }

    // --- assemble ---
    let mut out = format!("{ESC}[36m{dir}{ESC}[0m");
    if !branch.is_empty() {
        out.push_str(&format!("  {ESC}[1;32m {branch}{ESC}[0m"));
        if !gstat.is_empty() {
            out.push_str(&format!(" {ESC}[1;32m{gstat}{ESC}[0m"));
        }
    }
    if !pr.is_empty() {
        out.push_str(&format!("  {pr}"));
    }
    if !mshort.is_empty() {
        out.push_str(&format!("  {ESC}[35m{mshort}{ESC}[0m"));
    }
    if !ctx.is_empty() {
        out.push_str(&format!("  {ctx}"));
    }
    if !costs.is_empty() {
        out.push_str(&format!("  {costs}"));
    }
    if !lines.is_empty() {
        out.push_str(&format!("  {lines}"));
    }
    if !l5.is_empty() {
        out.push_str(&format!("  {l5}"));
    }
    if !lwk.is_empty() {
        out.push_str(&format!("  {lwk}"));
    }
    if !vim.is_empty() {
        if vim == "INSERT" {
            out.push_str(&format!("  {ESC}[32m{vim}{ESC}[0m"));
        } else {
            out.push_str(&format!("  {ESC}[34m{vim}{ESC}[0m"));
        }
    }

    let stdout = std::io::stdout();
    let mut h = stdout.lock();
    let _ = h.write_all(out.as_bytes());
    let _ = h.flush();
}
