//! External (file-backed) custom variables.
//!
//! Claude Code's status-line payload is a fixed schema — there's no field you
//! can stuff a custom string into. This layer lets you surface arbitrary strings
//! from *outside* that payload — a background build's status, a deploy marker, a
//! ticket number — by reading them from a file (or an env var) on each refresh.
//! Each entry becomes a normal template variable, so it gates/styles exactly
//! like the built-ins:
//!
//! ```jsonc
//! // ~/.config/statusline-rs.vars.json   (or the path in $SL_VARS)
//! {
//!   "vars": [
//!     { "name": "note", "file": "~/.claude/sl-note.txt" },
//!     { "name": "build", "file": "~/.cache/acme/ci.json", "path": ".state",
//!       "map": { "passing": "✓", "failing": "✗" }, "default": "?" },
//!     { "name": "ticket", "env": "JIRA_TICKET", "max": 12 }
//!   ]
//! }
//! ```
//!
//! Then in a template: `(  dim('note:')+note)(  build)`. The bridge for "some
//! tool tells the status line something" is just this: a hook (or Claude, when
//! you ask it) writes the file; `sl` reads it on the next refresh.
//!
//! Design notes matching the feature's stated semantics:
//! - **Lazy + cached.** Specs are parsed up front and their names union into the
//!   registry, but a file is *read* only when the active template references
//!   that variable (see [`resolve_into`]) — and each file is read at most once
//!   per invocation even when several variables share it.
//! - **`default` applies only once the source has been read.** A missing /
//!   unreadable / invalid-JSON file leaves the variable *undefined* (its group
//!   collapses) — a `default` never shows on an otherwise empty line. Once the
//!   file *has* been read, `default` fills an empty/unmapped value.
//! - **Never breaks the line.** A missing config is silently ignored; a
//!   malformed one emits a single stderr warning and is skipped.
//!
//! Dependency-light on purpose: config is JSON (serde_json is already a dep),
//! the path language is a tiny dotted/indexed subset of jq, no subprocess.

use std::collections::{BTreeSet, HashMap, HashSet};

/// One configured external variable, parsed from the config but NOT yet resolved
/// (no file read). Resolution is deferred to [`resolve_spec`], called only for
/// the specs the active template actually references.
pub struct Spec {
    /// Template name this variable is exposed under (a valid identifier).
    pub name: String,
    /// File source: the raw path *template* (`~` and `${field}` unexpanded).
    file: Option<String>,
    /// Environment-variable source (mutually exclusive with `file`; `env` wins).
    env: Option<String>,
    /// Optional jq-style dotted path into the file's JSON (`file` only).
    path: Option<String>,
    /// Optional value→display lookup table.
    map: Option<serde_json::Value>,
    /// Optional fallback: with `map`, for an unmapped value; without `map`, for
    /// an empty/missing value — applied only once the source has been read.
    default: Option<String>,
    /// Optional character budget; a longer value is clipped with a trailing `…`.
    max: Option<u64>,
}

/// Parse the external-variable config into specs (no file reads). Returns an
/// empty vec when no config exists — this feature never breaks the status line.
/// `home` is `$HOME`, used to locate the default config path.
///
/// Config path precedence: `$SL_VARS` if set, else
/// `~/.config/statusline-rs.vars.json`. A missing/unreadable config is silently
/// ignored; a malformed one emits a single stderr warning and is skipped.
pub fn load_specs(home: &str) -> Vec<Spec> {
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
        Err(_) => return Vec::new(), // missing/unreadable: silently ignored
    };
    let cfg: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            // Malformed config: one warning, then skipped (never breaks the line).
            eprintln!("sl: ignoring malformed {path}: {e}");
            return Vec::new();
        }
    };
    parse_specs(&cfg)
}

/// Parse a config value's `vars` array into specs (pure; testable). Skips
/// entries with a missing/invalid `name`; keeps everything else for lazy
/// resolution later.
pub fn parse_specs(cfg: &serde_json::Value) -> Vec<Spec> {
    let mut out = Vec::new();
    let Some(list) = cfg.get("vars").and_then(|v| v.as_array()) else {
        return out;
    };
    for entry in list {
        if let Some(spec) = parse_spec(entry) {
            out.push(spec);
        }
    }
    out
}

/// Parse a single config entry into a [`Spec`]; `None` when `name` is
/// missing/invalid.
fn parse_spec(entry: &serde_json::Value) -> Option<Spec> {
    let name = entry.get("name").and_then(|v| v.as_str())?;
    if !is_valid_name(name) {
        return None;
    }
    Some(Spec {
        name: name.to_string(),
        file: entry.get("file").and_then(|v| v.as_str()).map(str::to_string),
        env: entry.get("env").and_then(|v| v.as_str()).map(str::to_string),
        path: entry.get("path").and_then(|v| v.as_str()).map(str::to_string),
        map: entry.get("map").filter(|v| v.is_object()).cloned(),
        default: entry.get("default").and_then(|v| v.as_str()).map(str::to_string),
        max: entry.get("max").and_then(|v| v.as_u64()),
    })
}

/// Union every spec's name into the registry so the engine treats it as a
/// variable (substitute + gate its group). Built-in names win: a spec that would
/// shadow a core variable is skipped with a single warning, so templates stay
/// predictable. Registering the name is independent of whether the variable ends
/// up with a value — an unresolved ext var still gates/collapses like an unset
/// built-in.
pub fn register(specs: &[Spec], registry: &mut HashSet<String>, builtin: &[&str]) {
    for s in specs {
        if builtin.contains(&s.name.as_str()) {
            eprintln!("sl: external var '{}' shadows a built-in; ignored", s.name);
            continue;
        }
        registry.insert(s.name.clone());
    }
}

/// Resolve the external variables the active template references and insert each
/// non-empty value into `vars`. Laziness lives here: a spec whose name is not in
/// `refs` is never resolved, so its file is never read. Files are read at most
/// once per invocation (shared reads are cached). A spec that resolves to no
/// value is simply not inserted — its name is already in the registry (see
/// [`register`]), so its group collapses like an unset built-in.
///
/// `interp` supplies `${field}` interpolation for `file` paths (payload +
/// derived variables). `builtin` re-guards the shadow rule so a config name can
/// never overwrite a core variable.
pub fn resolve_into(
    specs: &[Spec],
    refs: &BTreeSet<String>,
    home: &str,
    interp: &HashMap<String, String>,
    vars: &mut HashMap<String, String>,
    builtin: &[&str],
) {
    // Per-invocation cache of raw file contents, keyed by the resolved path.
    // `None` records a read that failed, so a shared missing file isn't retried.
    let mut cache: HashMap<String, Option<String>> = HashMap::new();
    for s in specs {
        if builtin.contains(&s.name.as_str()) || !refs.contains(&s.name) {
            continue;
        }
        if let Some(val) = resolve_spec(s, home, interp, &mut cache) {
            vars.insert(s.name.clone(), val);
        }
    }
}

/// Resolve one spec to its display value, or `None` when the variable is
/// *undefined* (its group should collapse): the source was absent, or the
/// computed value came out empty. `cache` dedupes file reads within an
/// invocation.
fn resolve_spec(
    spec: &Spec,
    home: &str,
    interp: &HashMap<String, String>,
    cache: &mut HashMap<String, Option<String>>,
) -> Option<String> {
    // Read the raw source. `None` = the source is ABSENT (undefined): env unset,
    // or file missing / unreadable / invalid-JSON. `Some(_)` = the source was
    // READ (the value may be empty, which a `default` can then fill). `env` wins
    // over `file` when both are set.
    let read: Option<String> = if let Some(env) = &spec.env {
        std::env::var(env).ok().map(|v| first_line(&v))
    } else if let Some(file) = &spec.file {
        let path = expand_home(&interpolate(file, interp), home);
        match read_file_cached(&path, cache) {
            None => None, // missing / unreadable -> undefined
            Some(raw) => match &spec.path {
                Some(p) => match serde_json::from_str::<serde_json::Value>(&raw) {
                    // Valid JSON: a missing/non-scalar path is an empty (but READ)
                    // value, so a `default` still applies.
                    Ok(json) => Some(json_path(&json, p).map(|v| first_line(&v)).unwrap_or_default()),
                    Err(_) => None, // invalid JSON -> undefined
                },
                None => Some(first_line(&raw)),
            },
        }
    } else {
        None // no source configured
    };

    let value = apply_map(read, spec.map.as_ref(), spec.default.as_deref());
    let value = clip(&value, spec.max);
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// Turn a read result into a display string, applying `map`/`default`.
///
/// - **Absent** (`read == None`): empty, *regardless of* `default` — a default
///   shows only once the source has actually been read.
/// - **With `map`**: a value present in the table → its mapping; otherwise (an
///   unmapped or empty value) → `default` if set, else the value unchanged.
/// - **Without `map`**: an empty value → `default` if set, else empty; a
///   non-empty value passes through.
fn apply_map(read: Option<String>, map: Option<&serde_json::Value>, default: Option<&str>) -> String {
    let Some(value) = read else {
        return String::new();
    };
    if let Some(map) = map.and_then(|m| m.as_object()) {
        if !value.is_empty() {
            if let Some(mapped) = map.get(&value).and_then(|v| v.as_str()) {
                return mapped.to_string();
            }
        }
        return default.map(str::to_string).unwrap_or(value);
    }
    if value.is_empty() {
        return default.unwrap_or("").to_string();
    }
    value
}

/// Read a file's contents once per invocation. `None` = the read failed (missing
/// / unreadable); a failed read is cached too, so a shared missing file isn't
/// retried. Status files are tiny, so caching the whole string is cheap.
fn read_file_cached(path: &str, cache: &mut HashMap<String, Option<String>>) -> Option<String> {
    if let Some(hit) = cache.get(path) {
        return hit.clone();
    }
    let val = std::fs::read_to_string(path).ok();
    cache.insert(path.to_string(), val.clone());
    val
}

/// Substitute `${name}` with `interp[name]` (empty when undefined). A `${` with
/// no closing `}` is left literal. Used to key a `file` path by a payload field
/// or derived variable (e.g. `~/dir/${session_id}.json`).
fn interpolate(s: &str, interp: &HashMap<String, String>) -> String {
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' && i + 1 < chars.len() && chars[i + 1] == '{' {
            if let Some(close) = chars[i + 2..].iter().position(|&c| c == '}') {
                let name: String = chars[i + 2..i + 2 + close].iter().collect();
                if let Some(val) = interp.get(&name) {
                    out.push_str(val);
                }
                i = i + 2 + close + 1; // past the '}'
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Read a value from a file source — the shared reader behind the inline
/// `file()` / `json()` template builtins (the config vars go through
/// [`resolve_spec`], which adds caching and the read/absent distinction).
/// Returns the raw file contents (first line, trimmed) when `jq` is `None`, or
/// the jq-path scalar when `jq` is `Some`. Empty string on any failure (missing
/// file, parse error, missing/non-scalar path) so callers uniformly treat empty
/// as "no value". `home` expands a leading `~` in `file`.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn jv(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("valid json")
    }

    /// Resolve a single parsed entry with a fresh cache — a concise harness for
    /// the resolver tests. Returns `None` for an undefined (collapsing) variable.
    fn resolve1(entry: &serde_json::Value, home: &str, interp: &HashMap<String, String>) -> Option<String> {
        let spec = parse_spec(entry).expect("valid spec");
        let mut cache = HashMap::new();
        resolve_spec(&spec, home, interp, &mut cache)
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
    fn resolve_env_var_first_line() {
        std::env::set_var("SL_TEST_TICKET", "PROJ-123\nsecond line");
        let out = resolve1(&jv(r#"{"name":"ticket","env":"SL_TEST_TICKET"}"#), "/home/u", &HashMap::new());
        assert_eq!(out, Some("PROJ-123".to_string())); // first line only
        std::env::remove_var("SL_TEST_TICKET");
    }

    #[test]
    fn resolve_absent_source_is_undefined() {
        // A missing file resolves to None (undefined), so its group collapses —
        // it is NOT registered here (that's register()'s job), it just has no
        // value. An invalid name is dropped entirely at parse time.
        assert_eq!(resolve1(&jv(r#"{"name":"x","file":"/no/such/file/xyz"}"#), "/home/u", &HashMap::new()), None);
        assert!(parse_spec(&jv(r#"{"name":"bad name","env":"HOME"}"#)).is_none());
    }

    #[test]
    fn interpolate_substitutes_and_leaves_literal() {
        let interp: HashMap<String, String> =
            [("session_id".to_string(), "abc-123".to_string())].into_iter().collect();
        assert_eq!(interpolate("~/d/${session_id}.json", &interp), "~/d/abc-123.json");
        // Undefined -> empty; an unterminated ${ stays literal.
        assert_eq!(interpolate("${nope}x", &interp), "x");
        assert_eq!(interpolate("a${b", &interp), "a${b");
    }

    #[test]
    fn resolve_interpolates_file_path_by_field() {
        // A per-session file resolved via ${sid} in the path.
        let dir = std::env::temp_dir();
        std::fs::write(dir.join("sl_ev_sess_ZZ.json"), r#"{"phase":"R3"}"#).unwrap();
        let entry = jv(&format!(
            r#"{{"name":"phase","file":"{}/sl_ev_sess_${{sid}}.json","path":".phase"}}"#,
            dir.to_string_lossy()
        ));
        let interp: HashMap<String, String> = [("sid".to_string(), "ZZ".to_string())].into_iter().collect();
        assert_eq!(resolve1(&entry, "", &interp), Some("R3".to_string()));
    }

    #[test]
    fn apply_map_absent_never_defaults() {
        // The key semantic: with the source ABSENT, `default` must NOT show — the
        // variable stays undefined so its group collapses on a missing file.
        let entry = jv(r#"{"map":{"a":"b"},"default":"?"}"#);
        assert_eq!(apply_map(None, entry.get("map"), entry.get("default").and_then(|v| v.as_str())), "");
    }

    #[test]
    fn apply_map_default_fills_once_read() {
        // With a map: mapped value wins; an unmapped-but-read value -> default; an
        // empty-but-read value (path missing in a valid JSON) -> default too.
        let m = jv(r#"{"map":{"passing":"✓","failing":"✗"},"default":"?"}"#);
        let map = m.get("map");
        let def = m.get("default").and_then(|v| v.as_str());
        assert_eq!(apply_map(Some("passing".into()), map, def), "✓");
        assert_eq!(apply_map(Some("queued".into()), map, def), "?"); // unmapped -> default
        assert_eq!(apply_map(Some(String::new()), map, def), "?"); // empty read -> default

        // Without a map: a default fills an empty READ value, non-empty passes
        // through; with no default an empty read stays empty (undefined).
        assert_eq!(apply_map(Some(String::new()), None, Some("-")), "-");
        assert_eq!(apply_map(Some("hi".into()), None, Some("-")), "hi");
        assert_eq!(apply_map(Some(String::new()), None, None), "");
        // Map, no default, unmapped -> keep the raw value.
        let m2 = jv(r#"{"map":{"a":"b"}}"#);
        assert_eq!(apply_map(Some("y".into()), m2.get("map"), None), "y");
    }

    #[test]
    fn resolve_default_only_after_file_read() {
        // End-to-end of the read/absent distinction through resolve_spec.
        let dir = std::env::temp_dir();
        let p = dir.join("sl_ev_default_read.json");
        std::fs::write(&p, r#"{"other":"x"}"#).unwrap(); // valid JSON, .state MISSING
        let present = jv(&format!(
            r#"{{"name":"build","file":"{}","path":".state","default":"?"}}"#,
            p.to_string_lossy()
        ));
        // File read, path missing -> default shows.
        assert_eq!(resolve1(&present, "", &HashMap::new()), Some("?".to_string()));
        // File absent -> undefined (default does NOT show).
        let absent = jv(r#"{"name":"build","file":"/no/such/ev/file.json","path":".state","default":"?"}"#);
        assert_eq!(resolve1(&absent, "", &HashMap::new()), None);
        // Invalid JSON -> undefined (default does NOT show).
        std::fs::write(&p, "not json").unwrap();
        assert_eq!(resolve1(&present, "", &HashMap::new()), None);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn read_file_cached_reads_once() {
        // Proves the read happens once: after the first read we delete the file,
        // and the second call still returns the cached contents.
        let dir = std::env::temp_dir();
        let p = dir.join("sl_ev_cache.txt");
        std::fs::write(&p, "cached-value\n").unwrap();
        let path = p.to_string_lossy().to_string();
        let mut cache = HashMap::new();
        let first = read_file_cached(&path, &mut cache);
        let _ = std::fs::remove_file(&p); // file gone
        let second = read_file_cached(&path, &mut cache);
        assert_eq!(first.as_deref(), Some("cached-value\n"));
        assert_eq!(first, second, "second read must come from the cache");
    }

    #[test]
    fn resolve_into_is_lazy() {
        // Only a referenced spec is resolved; an unreferenced one gets no value
        // even though its file exists.
        let dir = std::env::temp_dir();
        std::fs::write(dir.join("sl_ev_lazy_a.txt"), "AAA\n").unwrap();
        std::fs::write(dir.join("sl_ev_lazy_b.txt"), "BBB\n").unwrap();
        let cfg = jv(&format!(
            r#"{{"vars":[
                {{"name":"aaa","file":"{d}/sl_ev_lazy_a.txt"}},
                {{"name":"bbb","file":"{d}/sl_ev_lazy_b.txt"}}
            ]}}"#,
            d = dir.to_string_lossy()
        ));
        let specs = parse_specs(&cfg);
        let refs: BTreeSet<String> = ["aaa".to_string()].into_iter().collect(); // only aaa
        let mut vars: HashMap<String, String> = HashMap::new();
        resolve_into(&specs, &refs, "", &HashMap::new(), &mut vars, &[]);
        assert_eq!(vars.get("aaa"), Some(&"AAA".to_string()));
        assert_eq!(vars.get("bbb"), None, "unreferenced spec must not be resolved");
    }

    #[test]
    fn register_unions_names_and_blocks_shadow() {
        let mut reg: HashSet<String> = ["dir".to_string()].into_iter().collect();
        let specs = parse_specs(&jv(
            r#"{"vars":[{"name":"dir","env":"HOME"},{"name":"note","file":"/x"}]}"#,
        ));
        register(&specs, &mut reg, &["dir"]);
        assert!(reg.contains("note"), "non-shadowing name registers");
        assert!(reg.contains("dir")); // still the built-in (present before)
        // A shadowing spec must not resolve into a value either.
        let refs: BTreeSet<String> = ["dir".to_string(), "note".to_string()].into_iter().collect();
        std::env::set_var("HOME", "/tmp/whatever");
        let mut vars: HashMap<String, String> = HashMap::new();
        resolve_into(&specs, &refs, "", &HashMap::new(), &mut vars, &["dir"]);
        assert_eq!(vars.get("dir"), None, "shadow blocked from resolving");
    }

    #[test]
    fn resolve_into_registers_empty_collapses() {
        // A referenced-but-absent source inserts no value (undefined -> collapse),
        // while the name still gets registered by register().
        let specs = parse_specs(&jv(r#"{"vars":[{"name":"note","file":"/no/such/xyz"}]}"#));
        let mut reg: HashSet<String> = HashSet::new();
        register(&specs, &mut reg, &["dir"]);
        assert!(reg.contains("note"), "name must register for gating");
        let refs: BTreeSet<String> = ["note".to_string()].into_iter().collect();
        let mut vars: HashMap<String, String> = HashMap::new();
        resolve_into(&specs, &refs, "", &HashMap::new(), &mut vars, &["dir"]);
        assert_eq!(vars.get("note"), None, "empty value must not be inserted");
    }

    #[test]
    fn max_clips_with_ellipsis() {
        let dir = std::env::temp_dir();
        std::fs::write(dir.join("sl_ev_clip.txt"), "hello world").unwrap();
        let entry = jv(&format!(
            r#"{{"name":"m","file":"{}/sl_ev_clip.txt","max":5}}"#,
            dir.to_string_lossy()
        ));
        assert_eq!(resolve1(&entry, "", &HashMap::new()), Some("hell\u{2026}".to_string()));
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
    fn read_source_shared_reader_unchanged() {
        // The reader behind the inline file()/json() builtins: empty on any
        // failure, first line for plain files, scalar for a jq path.
        let dir = std::env::temp_dir();
        std::fs::write(dir.join("sl_ev_rs.json"), r#"{"ci":{"status":"passing"}}"#).unwrap();
        let p = format!("{}/sl_ev_rs.json", dir.to_string_lossy());
        assert_eq!(read_source(&p, Some(".ci.status"), ""), "passing");
        assert_eq!(read_source("/no/such/file", None, ""), ""); // failure -> empty
    }
}
