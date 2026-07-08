//! External (file-backed) custom variables — a proof-of-concept.
//!
//! Lets you surface arbitrary strings from *outside* Claude Code's status-line
//! payload: a plain text file, a value extracted from a JSON file with a
//! jq-like dotted path, or an environment variable. Each entry becomes a normal
//! template variable, so it gates/styles exactly like the built-ins:
//!
//! ```jsonc
//! // ~/.config/statusline-rs.vars.json   (or the path in $SL_VARS)
//! {
//!   "vars": [
//!     { "name": "note", "file": "~/.claude/sl-note.txt" },
//!     { "name": "ci",   "file": "~/.claude/ctx.json", "path": ".ci.status" },
//!     { "name": "ticket", "env": "JIRA_TICKET" }
//!   ]
//! }
//! ```
//!
//! Then in a template: `(  dim('note:')+note)(  magenta(ci))`. The bridge for
//! "Claude tells the status line something" is just this: a hook (or Claude,
//! when you ask it) writes the file; `sl` reads it on the next refresh.
//!
//! Dependency-light on purpose: config is JSON (serde_json is already a dep),
//! the path language is a tiny dotted/indexed subset of jq, no subprocess.

use std::collections::{HashMap, HashSet};

/// One configured external variable: its template name and the resolved value
/// (already trimmed to a single line). The value may be EMPTY — a configured
/// name is always produced so it registers as a known variable and its segment
/// collapses (rather than printing the bare name) when the source is
/// missing/empty, exactly like an unset built-in.
pub struct ExtVar {
    pub name: String,
    pub value: String,
}

/// Load and resolve external variables from the config file. Returns an empty
/// vec when no config exists or it can't be parsed — this feature never breaks
/// the status line. `home` is `$HOME` for `~` expansion in `file` paths; `vars`
/// is the built-in variable map, used to interpolate `${name}` in `file` paths
/// (e.g. `~/dir/${session_id}.json`).
///
/// Config path precedence: `$SL_VARS` if set, else `~/.config/statusline-rs.vars.json`.
pub fn load(home: &str, vars: &HashMap<String, String>) -> Vec<ExtVar> {
    let path = match std::env::var("SL_VARS") {
        Ok(p) if !p.is_empty() => p,
        _ => {
            if home.is_empty() {
                return Vec::new();
            }
            format!("{home}/.config/statusline-rs.vars.json")
        }
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let cfg: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    resolve(&cfg, home, vars)
}

/// Resolve a parsed config value into external variables (pure; testable).
/// Skips entries with a missing/invalid `name`, but KEEPS validly-named entries
/// even when they resolve to empty — so the name still registers and its
/// segment collapses instead of printing the literal name. `vars` supplies
/// `${name}` interpolation for `file` paths.
pub fn resolve(cfg: &serde_json::Value, home: &str, vars: &HashMap<String, String>) -> Vec<ExtVar> {
    let mut out = Vec::new();
    let Some(list) = cfg.get("vars").and_then(|v| v.as_array()) else {
        return out;
    };
    for entry in list {
        let Some(name) = entry.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        if !is_valid_name(name) {
            continue;
        }
        let value = resolve_entry(entry, home, vars);
        let value = clip(&value, entry.get("max").and_then(|v| v.as_u64()));
        out.push(ExtVar {
            name: name.to_string(),
            value,
        });
    }
    out
}

/// Resolve a single entry to its value (first line, trimmed; `max` applied by
/// the caller). `env` > `file`(+`path`) in precedence; empty when unresolved.
/// A `file` path interpolates `${name}` from `vars`. An optional `map`
/// (value→string) with an optional `default` rewrites a non-empty value — a
/// conditional-free way to turn e.g. a status into a symbol.
fn resolve_entry(entry: &serde_json::Value, home: &str, vars: &HashMap<String, String>) -> String {
    let raw = if let Some(var) = entry.get("env").and_then(|v| v.as_str()) {
        first_line(&std::env::var(var).unwrap_or_default())
    } else if let Some(file) = entry.get("file").and_then(|v| v.as_str()) {
        let path = interpolate(file, vars);
        read_source(&path, entry.get("path").and_then(|v| v.as_str()), home)
    } else {
        String::new()
    };
    apply_map(raw, entry)
}

/// Rewrite a non-empty `value` through an entry's optional `map`
/// (`{"done":"✓", …}`): the mapped string if `value` is a key, else the entry's
/// `default` if present, else `value` unchanged. Empty stays empty (so the
/// segment still collapses). No `map` → `value` unchanged.
fn apply_map(value: String, entry: &serde_json::Value) -> String {
    if value.is_empty() {
        return value;
    }
    let Some(map) = entry.get("map").and_then(|v| v.as_object()) else {
        return value;
    };
    if let Some(mapped) = map.get(&value).and_then(|v| v.as_str()) {
        return mapped.to_string();
    }
    match entry.get("default").and_then(|v| v.as_str()) {
        Some(def) => def.to_string(),
        None => value,
    }
}

/// Substitute `${name}` with `vars[name]` (empty when undefined). A `${` with no
/// closing `}` is left literal. Used to key a `file` path by a payload variable.
fn interpolate(s: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '$' && i + 1 < bytes.len() && bytes[i + 1] == '{' {
            if let Some(close) = bytes[i + 2..].iter().position(|&c| c == '}') {
                let name: String = bytes[i + 2..i + 2 + close].iter().collect();
                if let Some(val) = vars.get(&name) {
                    out.push_str(val);
                }
                i = i + 2 + close + 1; // past the '}'
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Read a value from a file source — the shared reader behind both the config
/// vars and the inline `file()` / `json()` template builtins. Returns the raw
/// file contents (first line, trimmed) when `jq` is `None`, or the jq-path
/// scalar when `jq` is `Some`. Empty string on any failure (missing file, parse
/// error, missing/non-scalar path) so callers uniformly treat empty as "no
/// value". `home` expands a leading `~` in `file`.
pub fn read_source(file: &str, jq: Option<&str>, home: &str) -> String {
    let path = expand_home(file, home);
    let raw = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    match jq {
        Some(p) => {
            let json: serde_json::Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => return String::new(),
            };
            json_path(&json, p).map(|v| first_line(&v)).unwrap_or_default()
        }
        None => first_line(&raw),
    }
}

/// A valid template variable name: `[A-Za-z_][A-Za-z0-9_]*`. Keeps injected
/// names parseable by the format engine and blocks surprises.
fn is_valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Expand a leading `~` / `~/` to `home`. Other `~user` forms are left as-is.
fn expand_home(path: &str, home: &str) -> String {
    if home.is_empty() {
        return path.to_string();
    }
    if path == "~" {
        home.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        path.to_string()
    }
}

/// First line, trimmed. A status line is one line — a whole file (or a stray
/// trailing newline) must not leak newlines into it.
fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

/// Clip `s` to at most `max` chars (Unicode scalar values), appending `…` when
/// truncated. `None` leaves it unbounded.
fn clip(s: &str, max: Option<u64>) -> String {
    match max {
        Some(m) if (s.chars().count() as u64) > m && m > 0 => {
            let keep = (m as usize).saturating_sub(1);
            let mut out: String = s.chars().take(keep).collect();
            out.push('\u{2026}');
            out
        }
        _ => s.to_string(),
    }
}

/// Extract a scalar from `json` with a tiny jq-like path: dot-separated keys
/// with optional `[n]` array indices, e.g. `.a.b`, `.items[0].name`, `a.b`
/// (leading `.` optional). Returns the scalar as a string (numbers/bools
/// stringified); `None` for a missing path, or a non-scalar (object/array/null)
/// result. Deliberately not full jq — just "pull a field out", dependency-free.
pub fn json_path(json: &serde_json::Value, path: &str) -> Option<String> {
    let mut cur = json;
    for seg in path.trim().trim_start_matches('.').split('.') {
        if seg.is_empty() {
            continue;
        }
        // Split `key[0][1]` into the key and any bracketed indices.
        let (key, rest) = match seg.find('[') {
            Some(i) => (&seg[..i], &seg[i..]),
            None => (seg, ""),
        };
        if !key.is_empty() {
            cur = cur.get(key)?;
        }
        // Apply each `[n]` index in order.
        let mut brk = rest;
        while let Some(close) = brk.find(']') {
            if !brk.starts_with('[') {
                return None;
            }
            let idx: usize = brk[1..close].parse().ok()?;
            cur = cur.get(idx)?;
            brk = &brk[close + 1..];
        }
    }
    match cur {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None, // null / object / array -> treated as "no value"
    }
}

/// Merge resolved external variables into the registry and variable map. Every
/// configured name extends the registry (so the engine substitutes/gates it),
/// but only NON-EMPTY values are inserted into `vars` — an empty ext var stays
/// "undefined", so its segment collapses like an unset built-in. Built-in names
/// win: an ext var may not shadow a core variable (skipped with a warning) so
/// templates stay predictable.
pub fn merge(
    ext: Vec<ExtVar>,
    registry: &mut HashSet<String>,
    vars: &mut HashMap<String, String>,
    builtin: &[&str],
) {
    for ev in ext {
        if builtin.contains(&ev.name.as_str()) {
            eprintln!("sl: external var '{}' shadows a built-in; ignored", ev.name);
            continue;
        }
        registry.insert(ev.name.clone());
        if !ev.value.is_empty() {
            vars.insert(ev.name, ev.value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jv(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("valid json")
    }

    #[test]
    fn json_path_nested_object() {
        let v = jv(r#"{"ci":{"status":"passing"}}"#);
        assert_eq!(json_path(&v, ".ci.status"), Some("passing".to_string()));
        assert_eq!(json_path(&v, "ci.status"), Some("passing".to_string())); // leading dot optional
    }

    #[test]
    fn json_path_array_index() {
        let v = jv(r#"{"items":[{"name":"a"},{"name":"b"}]}"#);
        assert_eq!(json_path(&v, ".items[1].name"), Some("b".to_string()));
        assert_eq!(json_path(&v, ".items[0].name"), Some("a".to_string()));
    }

    #[test]
    fn json_path_stringifies_scalars() {
        let v = jv(r#"{"n":42,"f":1.5,"b":true}"#);
        assert_eq!(json_path(&v, ".n"), Some("42".to_string()));
        assert_eq!(json_path(&v, ".f"), Some("1.5".to_string()));
        assert_eq!(json_path(&v, ".b"), Some("true".to_string()));
    }

    #[test]
    fn json_path_missing_and_nonscalar_are_none() {
        let v = jv(r#"{"a":{"b":1},"arr":[1,2]}"#);
        assert_eq!(json_path(&v, ".a.nope"), None); // missing key
        assert_eq!(json_path(&v, ".a"), None); // object -> no scalar
        assert_eq!(json_path(&v, ".arr"), None); // array -> no scalar
        assert_eq!(json_path(&v, ".arr[9]"), None); // out of bounds
    }

    #[test]
    fn resolve_env_var() {
        std::env::set_var("SL_TEST_TICKET", "PROJ-123\nsecond line");
        let cfg = jv(r#"{"vars":[{"name":"ticket","env":"SL_TEST_TICKET"}]}"#);
        let out = resolve(&cfg, "/home/u", &HashMap::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "ticket");
        assert_eq!(out[0].value, "PROJ-123"); // first line only
        std::env::remove_var("SL_TEST_TICKET");
    }

    #[test]
    fn resolve_skips_invalid_name_but_keeps_empty_value() {
        // Invalid name -> dropped entirely; valid name with a missing source ->
        // KEPT with an empty value (so it registers and its segment collapses).
        let cfg = jv(
            r#"{"vars":[
                {"name":"bad name","env":"HOME"},
                {"name":"missing","file":"/no/such/file/xyz"}
            ]}"#,
        );
        let out = resolve(&cfg, "/home/u", &HashMap::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "missing");
        assert_eq!(out[0].value, "");
    }

    #[test]
    fn interpolate_substitutes_and_leaves_literal() {
        let vars: HashMap<String, String> =
            [("session_id".to_string(), "abc-123".to_string())].into_iter().collect();
        assert_eq!(interpolate("~/d/${session_id}.json", &vars), "~/d/abc-123.json");
        // Undefined -> empty; an unterminated ${ stays literal.
        assert_eq!(interpolate("${nope}x", &vars), "x");
        assert_eq!(interpolate("a${b", &vars), "a${b");
    }

    #[test]
    fn resolve_interpolates_file_path_by_session_id() {
        // A per-session file resolved via ${session_id} in the path.
        let dir = std::env::temp_dir();
        std::fs::write(dir.join("sl_ev_sess_ZZ.json"), r#"{"phase":"R3"}"#).unwrap();
        let cfg = jv(&format!(
            r#"{{"vars":[{{"name":"phase","file":"{}/sl_ev_sess_${{sid}}.json","path":".phase"}}]}}"#,
            dir.to_string_lossy()
        ));
        let vars: HashMap<String, String> =
            [("sid".to_string(), "ZZ".to_string())].into_iter().collect();
        let out = resolve(&cfg, "", &vars);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value, "R3");
    }

    #[test]
    fn apply_map_rewrites_value() {
        let entry = jv(r#"{"map":{"done":"✓","parked":"⏸"},"default":"●"}"#);
        assert_eq!(apply_map("done".to_string(), &entry), "✓"); // mapped
        assert_eq!(apply_map("running".to_string(), &entry), "●"); // default
        assert_eq!(apply_map(String::new(), &entry), ""); // empty stays empty
        // No map -> unchanged.
        assert_eq!(apply_map("x".to_string(), &jv("{}")), "x");
        // Map, no default, unmapped -> keep raw.
        assert_eq!(apply_map("y".to_string(), &jv(r#"{"map":{"a":"b"}}"#)), "y");
    }

    #[test]
    fn merge_registers_empty_but_inserts_no_value() {
        // An empty ext var registers its name (so it gates/collapses) but is not
        // inserted as a value (stays "undefined").
        let mut reg: HashSet<String> = HashSet::new();
        let mut vars: HashMap<String, String> = HashMap::new();
        let ext = vec![ExtVar { name: "note".to_string(), value: String::new() }];
        merge(ext, &mut reg, &mut vars, &["dir"]);
        assert!(reg.contains("note"), "name must register for gating");
        assert_eq!(vars.get("note"), None, "empty value must not be inserted");
    }

    #[test]
    fn clip_truncates_with_ellipsis() {
        assert_eq!(clip("hello world", Some(5)), "hell\u{2026}");
        assert_eq!(clip("hi", Some(5)), "hi"); // under limit untouched
        assert_eq!(clip("hello", None), "hello"); // unbounded
    }

    #[test]
    fn expand_home_leading_tilde() {
        assert_eq!(expand_home("~/.claude/x", "/home/u"), "/home/u/.claude/x");
        assert_eq!(expand_home("~", "/home/u"), "/home/u");
        assert_eq!(expand_home("/abs/path", "/home/u"), "/abs/path");
    }

    #[test]
    fn merge_blocks_builtin_shadow() {
        let mut reg: HashSet<String> = ["dir".to_string()].into_iter().collect();
        let mut vars: HashMap<String, String> = HashMap::new();
        let ext = vec![
            ExtVar { name: "dir".to_string(), value: "x".to_string() },
            ExtVar { name: "note".to_string(), value: "hi".to_string() },
        ];
        merge(ext, &mut reg, &mut vars, &["dir"]);
        assert_eq!(vars.get("dir"), None); // shadow blocked
        assert_eq!(vars.get("note"), Some(&"hi".to_string()));
        assert!(reg.contains("note"));
    }
}
