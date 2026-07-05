//! statusline-rs format language engine.
//!
//! A tiny template language for the status line. Variables are computed in Rust
//! (some already ANSI-colored); the template controls layout, labels, optional
//! segments, and the styling of plain variables. Pipeline: [`render`] parses the
//! template into an AST (`Literal`/`Var`/`Group`/`Call`/`Br`), flattens it into
//! text runs carrying the enclosing style codes, then merges and serializes them
//! to a single ANSI string.
//!
//! Grammar in brief — "literal-default with a variable registry":
//! - Bare text is **literal** by default. A bare identifier substitutes as a
//!   variable ONLY inside a group or call, and only when it's in the `registry`
//!   passed to [`render`] / [`referenced_vars`]; otherwise it stays literal (so a
//!   typo'd name renders as text rather than erroring).
//! - `( … )` is an optional group. It collapses to nothing ONLY when EVERY
//!   variable it references (transitively, through style calls AND nested groups)
//!   is undefined — "show if anything has content". A group referencing no
//!   variable never collapses. So `(dim('ctx:')+c_ctx)` disappears entirely when
//!   `c_ctx` is unset, while `(sgr('1;32',branch)( c_git))` still shows the branch
//!   when only `c_git` is unset. To keep a multi-part segment tidy, give each
//!   optional piece its own group: `(dir)( branch)( pct)`.
//! - `name( … )` is a composable style call when `name` is a known style or the
//!   `br`/`sgr`/`hex` builtin; any other `name(` is literal text plus a group.
//!   Call arguments are separated by `,` (whitespace-insensitive): `sgr('1;32',
//!   branch)`, `hex('#5f8700', x)`.
//! - `a+b` (inside a group or call) is a concatenation join. It swallows the
//!   whitespace on both sides, so `'ctx: '+v` and `'ctx: ' + v` are identical and
//!   both tight. A literal `+`/`,` in a group/call is quoted (`'+'`) or escaped
//!   (`\+`, `\,`); at top level `+` (and `,` outside a call) is literal text.
//! - `"…"` and `'…'` both delimit literal strings (the other quote is literal
//!   inside), so a template can dodge whatever quote its JSON/shell wrapper uses;
//!   spaces inside the quotes print verbatim.

use std::collections::{HashMap, HashSet};
use std::fmt;

/// Error returned by [`render`] when a template cannot be parsed.
///
/// Parse errors: unbalanced `(`/`)`, unterminated string, invalid or dangling
/// backslash escape, and malformed `sgr(…)` / `hex(…)` specs.
#[allow(dead_code)]
#[derive(Debug)]
pub struct ParseError {
    msg: String,
}

#[allow(dead_code)]
impl ParseError {
    pub fn new(msg: impl Into<String>) -> Self {
        ParseError { msg: msg.into() }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parse error: {}", self.msg)
    }
}

impl std::error::Error for ParseError {}

/// AST node produced by the parser.
enum Node {
    /// Literal text (already un-escaped; passes through verbatim).
    Literal(String),
    /// A variable reference, resolved against the vars map at render time.
    Var(String),
    /// An optional group `( … )`. Gates to empty when a gating variable is undefined.
    Group(Vec<Node>),
    /// A style call `name( … )`. Carries the SGR code string — a named style's
    /// static code, or the computed code of `sgr(…)` / `hex(…)` (owned `String`).
    Call(String, Vec<Node>),
    /// The `br(…)` builtin: emits an unstyled `\n`, then its children under the
    /// enclosing style.
    Br(Vec<Node>),
}

/// A flattened text run carrying the ordered active SGR codes (outermost first).
struct Run {
    codes: Vec<String>,
    text: String,
}

/// True for identifier-start characters `[A-Za-z_]`.
fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

/// True for identifier-continue characters `[A-Za-z0-9_]`.
fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Map a style name to its SGR code string, or `None` for an unknown style.
fn style_code(name: &str) -> Option<&'static str> {
    Some(match name {
        "bold" => "1",
        "dim" => "2",
        "italic" => "3",
        "underline" => "4",
        "gray" => "90",
        "orange" => "38;5;208",
        "red" => "31",
        "green" => "32",
        "yellow" => "33",
        "blue" => "34",
        "magenta" => "35",
        "cyan" => "36",
        _ => return None,
    })
}

/// A variable is defined iff present in the map with a non-empty value.
fn is_defined(name: &str, vars: &HashMap<String, String>) -> bool {
    vars.get(name).is_some_and(|v| !v.is_empty())
}

/// True if `name` immediately followed by `(` should parse as a style/builtin
/// call: a named style, or one of the `br` / `sgr` / `hex` builtins. Any other
/// identifier before `(` is plain text and the `(` opens a group.
fn is_call_name(name: &str) -> bool {
    matches!(name, "br" | "sgr" | "hex") || style_code(name).is_some()
}

/// Preprocess the raw template BEFORE parsing (v1 §1), tracking `"…"` strings so
/// their contents (newlines and `\x` alike) stay literal. Outside strings:
///
/// 1. leading whitespace (spaces, tabs, newlines) at the very start is stripped;
/// 2. `\` immediately followed by a newline is a line continuation — the `\`, the
///    newline, and the following run of spaces/tabs are dropped (joins with NO
///    boundary);
/// 3. a bare newline keeps exactly one `\n` and strips the following run of
///    spaces/tabs (indentation).
///
/// The `\`-before-`(`/`)`/`"`/`\` escapes are left untouched for the parser.
fn preprocess(template: &str) -> String {
    let chars: Vec<char> = template.chars().collect();
    let n = chars.len();
    let mut out = String::new();
    let mut i = 0;

    // Rule 1: strip leading whitespace at the very start of the template.
    while i < n && matches!(chars[i], ' ' | '\t' | '\n') {
        i += 1;
    }

    while i < n {
        let c = chars[i];
        if c == '"' || c == '\'' {
            // Copy the string verbatim, honoring `\x` so an escaped quote does not
            // close it. Either `"` or `'` opens a string; the matching quote closes.
            let quote = c;
            out.push(c);
            i += 1;
            while i < n {
                let ch = chars[i];
                out.push(ch);
                i += 1;
                if ch == '\\' {
                    if i < n {
                        out.push(chars[i]);
                        i += 1;
                    }
                } else if ch == quote {
                    break;
                }
            }
        } else if c == '\\' {
            if i + 1 < n && chars[i + 1] == '\n' {
                // Rule 2: line continuation — drop `\`, the newline, and the
                // following run of spaces/tabs.
                i += 2;
                while i < n && matches!(chars[i], ' ' | '\t') {
                    i += 1;
                }
            } else if i + 1 < n {
                // A `\`-escape for the parser: copy both chars verbatim.
                out.push('\\');
                out.push(chars[i + 1]);
                i += 2;
            } else {
                // Dangling `\` at end of input: leave it for the parser to reject.
                out.push('\\');
                i += 1;
            }
        } else if c == '\n' {
            // Rule 3: keep one `\n`, strip the following run of spaces/tabs.
            out.push('\n');
            i += 1;
            while i < n && matches!(chars[i], ' ' | '\t') {
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Build an `sgr(SPEC content…)` call. SPEC is the first child and MUST be a
/// literal giving raw SGR params (e.g. `1;31`); the rest is the wrapped content.
fn build_sgr(mut inner: Vec<Node>) -> Result<Node, ParseError> {
    let spec = match inner.first() {
        Some(Node::Literal(s)) => s.clone(),
        Some(_) => return Err(ParseError::new("sgr: spec must be a literal")),
        None => return Err(ParseError::new("sgr: missing spec")),
    };
    if spec.is_empty() {
        return Err(ParseError::new("sgr: empty spec"));
    }
    let content = inner.split_off(1);
    Ok(Node::Call(spec, content))
}

/// Build a `hex(SPEC content…)` call. SPEC is the first child and MUST be a
/// `#rrggbb` literal; it becomes a truecolor foreground `38;2;R;G;B` code.
fn build_hex(mut inner: Vec<Node>) -> Result<Node, ParseError> {
    let spec = match inner.first() {
        Some(Node::Literal(s)) => s.clone(),
        Some(_) => return Err(ParseError::new("hex: spec must be a literal")),
        None => return Err(ParseError::new("hex: missing spec")),
    };
    let sc: Vec<char> = spec.chars().collect();
    if sc.len() != 7 || sc[0] != '#' || !sc[1..].iter().all(|c| c.is_ascii_hexdigit()) {
        return Err(ParseError::new("hex: spec must be #rrggbb"));
    }
    let digits: String = sc[1..].iter().collect();
    let r = u8::from_str_radix(&digits[0..2], 16).unwrap();
    let g = u8::from_str_radix(&digits[2..4], 16).unwrap();
    let b = u8::from_str_radix(&digits[4..6], 16).unwrap();
    let content = inner.split_off(1);
    Ok(Node::Call(format!("38;2;{r};{g};{b}"), content))
}

/// Parse a sequence of nodes until end-of-input or a closing `)`.
///
/// On entry `*pos` points at the first char to consider. A `)` returns control
/// to the caller (with `*pos` left on the `)`) unless `top_level`, in which case
/// it is an unbalanced close. Errors on unterminated strings and dangling `\`.
///
/// `subst` selects the identifier semantics ("literal-default with registry"):
/// - `subst == false` (top level, outside all parens): a bare identifier is
///   literal text — no substitution.
/// - `subst == true` (inside any group or call): a bare identifier is a variable
///   iff it is in `registry`; otherwise it is literal text.
///
/// In both modes, `name(` is a call only when `name` is a style/builtin
/// ([`is_call_name`]); any other identifier before `(` is text and the `(` opens
/// a group. Groups and call bodies always parse with `subst == true`; a call body
/// additionally parses with `in_call == true`, which makes `,` an argument
/// separator (structural) rather than literal text.
fn parse_seq(
    chars: &[char],
    pos: &mut usize,
    top_level: bool,
    subst: bool,
    in_call: bool,
    registry: &HashSet<String>,
) -> Result<Vec<Node>, ParseError> {
    let mut nodes = Vec::new();
    while *pos < chars.len() {
        let c = chars[*pos];
        if c == ')' {
            if top_level {
                return Err(ParseError::new("unbalanced ')'"));
            }
            return Ok(nodes);
        } else if c == '(' {
            // '(' not immediately preceded by a call name -> group. A group is a
            // layout body, NOT a call argument list, so `,` is literal inside it.
            *pos += 1;
            let inner = parse_seq(chars, pos, false, true, false, registry)?;
            if *pos >= chars.len() || chars[*pos] != ')' {
                return Err(ParseError::new("unbalanced '('"));
            }
            *pos += 1; // consume ')'
            nodes.push(Node::Group(inner));
        } else if c == '"' || c == '\'' {
            // A string literal delimited by `"` or `'`; the other quote is a plain
            // character inside. Lets a template avoid escaping whichever quote the
            // surrounding JSON / shell already uses.
            let quote = c;
            *pos += 1; // consume opening quote
            let mut s = String::new();
            loop {
                if *pos >= chars.len() {
                    return Err(ParseError::new("unterminated string"));
                }
                let ch = chars[*pos];
                if ch == quote {
                    *pos += 1; // consume closing quote
                    break;
                } else if ch == '\\' {
                    *pos += 1;
                    if *pos >= chars.len() {
                        return Err(ParseError::new("unterminated string"));
                    }
                    s.push(chars[*pos]); // '\' any -> the escaped char, verbatim
                    *pos += 1;
                } else {
                    s.push(ch);
                    *pos += 1;
                }
            }
            nodes.push(Node::Literal(s));
        } else if is_ident_start(c) {
            let start = *pos;
            *pos += 1;
            while *pos < chars.len() && is_ident_continue(chars[*pos]) {
                *pos += 1;
            }
            let ident: String = chars[start..*pos].iter().collect();
            if *pos < chars.len() && chars[*pos] == '(' && is_call_name(&ident) {
                // Known style/builtin immediately followed by '(' -> call. A call
                // body is an argument list, so `,` separates arguments here.
                *pos += 1; // consume '('
                let inner = parse_seq(chars, pos, false, true, true, registry)?;
                if *pos >= chars.len() || chars[*pos] != ')' {
                    return Err(ParseError::new("unbalanced '('"));
                }
                *pos += 1; // consume ')'
                let node = match ident.as_str() {
                    "br" => Node::Br(inner),
                    "sgr" => build_sgr(inner)?,
                    "hex" => build_hex(inner)?,
                    _ => Node::Call(
                        style_code(&ident).expect("is_call_name checked").to_string(),
                        inner,
                    ),
                };
                nodes.push(node);
            } else if subst && registry.contains(ident.as_str()) {
                // In substitution context, a registry name -> variable. (Any '(' that
                // follows is not a call here — it opens a group on the next pass.)
                nodes.push(Node::Var(ident));
            } else {
                // Literal-default: an identifier that is neither a call nor a known
                // variable is plain text. A trailing '(' (non-call) opens a group next.
                nodes.push(Node::Literal(ident));
            }
        } else if (c == '+' && subst) || (c == ',' && in_call) {
            // Structural join / argument separator: emits nothing and swallows the
            // whitespace on BOTH sides, so `a + b` renders exactly like `a+b`. `+`
            // joins operands anywhere inside a group or call; `,` separates
            // arguments inside a call. A literal `+`/`,` is quoted or `\+` / `\,`;
            // at top level (and `,` outside a call) they are ordinary literal text.
            *pos += 1;
            while *pos < chars.len() && matches!(chars[*pos], ' ' | '\t') {
                *pos += 1;
            }
        } else {
            // Literal run: chars that are not ident-start / '(' / ')' / quote, nor a
            // structural `+` (in a group/call) or `,` (in a call). STRICT escapes.
            let mut s = String::new();
            while *pos < chars.len() {
                let ch = chars[*pos];
                if ch == '('
                    || ch == ')'
                    || ch == '"'
                    || ch == '\''
                    || (subst && ch == '+')
                    || (in_call && ch == ',')
                    || is_ident_start(ch)
                {
                    break;
                }
                if ch == '\\' {
                    *pos += 1;
                    if *pos >= chars.len() {
                        return Err(ParseError::new("dangling backslash"));
                    }
                    let e = chars[*pos];
                    match e {
                        '(' | ')' | '"' | '\'' | '\\' | '+' | ',' => s.push(e),
                        _ => {
                            return Err(ParseError::new(format!("invalid escape '{e}'")))
                        }
                    }
                    *pos += 1;
                } else {
                    s.push(ch);
                    *pos += 1;
                }
            }
            // Drop trailing whitespace that abuts a structural `+`/`,` so the join
            // is tight (`a + b` == `a+b`); the whitespace after it is skipped above.
            if *pos < chars.len()
                && ((chars[*pos] == '+' && subst) || (chars[*pos] == ',' && in_call))
            {
                while s.ends_with(' ') || s.ends_with('\t') {
                    s.pop();
                }
            }
            if !s.is_empty() {
                nodes.push(Node::Literal(s));
            }
        }
    }
    Ok(nodes)
}

/// Scan `nodes` for variable references, returning `(references_any_var,
/// any_referenced_var_is_defined)`. Reaches transitively through calls, `br`,
/// AND nested groups — a variable anywhere inside keeps its enclosing group
/// alive.
fn scan_vars(nodes: &[Node], vars: &HashMap<String, String>) -> (bool, bool) {
    let mut has = false;
    let mut any_defined = false;
    for n in nodes {
        match n {
            Node::Var(name) => {
                has = true;
                if is_defined(name, vars) {
                    any_defined = true;
                }
            }
            Node::Call(_, inner) | Node::Br(inner) | Node::Group(inner) => {
                let (h, d) = scan_vars(inner, vars);
                has |= h;
                any_defined |= d;
            }
            Node::Literal(_) => {}
        }
    }
    (has, any_defined)
}

/// Whether a group renders. It collapses ONLY when it references at least one
/// variable and ALL of them are undefined ("show if anything has content"). A
/// group that references no variable never collapses. Nested groups are
/// re-checked independently as `flatten` recurses, so an empty inner group still
/// drops out of an otherwise-live parent.
fn group_renders(nodes: &[Node], vars: &HashMap<String, String>) -> bool {
    let (has, any_defined) = scan_vars(nodes, vars);
    !has || any_defined
}

/// Flatten nodes into runs, carrying the ordered active style codes.
fn flatten(
    nodes: &[Node],
    active: &[String],
    vars: &HashMap<String, String>,
    out: &mut Vec<Run>,
) {
    for n in nodes {
        match n {
            Node::Literal(s) => out.push(Run {
                codes: active.to_vec(),
                text: s.clone(),
            }),
            Node::Var(name) => {
                let text = vars.get(name).cloned().unwrap_or_default();
                out.push(Run {
                    codes: active.to_vec(),
                    text,
                });
            }
            Node::Group(children) => {
                // Collapses only when EVERY variable it references is undefined
                // ("show if anything has content").
                if group_renders(children, vars) {
                    flatten(children, active, vars, out);
                }
            }
            Node::Call(code, inner) => {
                let mut new_active = active.to_vec();
                new_active.push(code.clone());
                flatten(inner, &new_active, vars, out);
            }
            Node::Br(children) => {
                // An UNSTYLED newline (empty active codes), then children under
                // the enclosing style.
                out.push(Run {
                    codes: Vec::new(),
                    text: "\n".to_string(),
                });
                flatten(children, active, vars, out);
            }
        }
    }
}

/// Merge adjacent runs sharing the identical active-style list, then serialize:
/// non-empty runs as `ESC[codes…m<text>ESC[0m` (or verbatim when unstyled),
/// empty runs as nothing.
fn serialize(runs: &[Run]) -> String {
    let mut merged: Vec<Run> = Vec::new();
    for run in runs {
        if let Some(last) = merged.last_mut() {
            if last.codes == run.codes {
                last.text.push_str(&run.text);
                continue;
            }
        }
        merged.push(Run {
            codes: run.codes.clone(),
            text: run.text.clone(),
        });
    }

    let mut out = String::new();
    for run in &merged {
        if run.text.is_empty() {
            continue;
        }
        if run.codes.is_empty() {
            out.push_str(&run.text);
        } else {
            out.push('\u{1b}');
            out.push('[');
            out.push_str(&run.codes.join(";"));
            out.push('m');
            out.push_str(&run.text);
            out.push_str("\u{1b}[0m");
        }
    }
    out
}

/// Render `template` against `vars`, returning the rendered status-line string
/// or a [`ParseError`]. `registry` is the set of known variable names — inside a
/// group or call, an identifier in `registry` substitutes its value (and gates
/// its group when undefined); any other identifier is literal text. Never panics
/// on well-formed UTF-8 input.
pub fn render(
    template: &str,
    vars: &HashMap<String, String>,
    registry: &HashSet<String>,
) -> Result<String, ParseError> {
    let pre = preprocess(template);
    let chars: Vec<char> = pre.chars().collect();
    let mut pos = 0;
    let nodes = parse_seq(&chars, &mut pos, true, false, false, registry)?;
    let mut runs = Vec::new();
    flatten(&nodes, &[], vars, &mut runs);
    Ok(serialize(&runs))
}

/// Collect `Node::Var` names into `set`, recursing through groups, calls, and
/// `br`. Style names, `br`, and the `sgr`/`hex` SPEC literal are NOT variables.
fn collect_vars(nodes: &[Node], set: &mut std::collections::BTreeSet<String>) {
    for n in nodes {
        match n {
            Node::Var(name) => {
                set.insert(name.clone());
            }
            Node::Group(inner) | Node::Call(_, inner) | Node::Br(inner) => {
                collect_vars(inner, set)
            }
            Node::Literal(_) => {}
        }
    }
}

/// Return the set of variable names referenced by `template` — `Node::Var` names
/// only, NOT style names, `br`, or the `sgr`/`hex` literal SPEC — or a
/// [`ParseError`] if the template does not parse. `registry` is the set of known
/// variable names (see [`render`]); an identifier outside it is literal text and
/// is not reported.
pub fn referenced_vars(
    template: &str,
    registry: &HashSet<String>,
) -> Result<std::collections::BTreeSet<String>, ParseError> {
    let pre = preprocess(template);
    let chars: Vec<char> = pre.chars().collect();
    let mut pos = 0;
    let nodes = parse_seq(&chars, &mut pos, true, false, false, registry)?;
    let mut set = std::collections::BTreeSet::new();
    collect_vars(&nodes, &mut set);
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashSet};

    // ============================ Test harness ============================
    // v3 grammar: literal-default at top level; variables substitute only inside
    // a group `(…)` or a style call `name(…)`, and only when the identifier is a
    // known registry name. `render`/`referenced_vars` now take a registry arg.

    /// Build a registry (set of known variable names).
    fn reg(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// A fixed registry covering every identifier any test treats as a variable.
    /// Names NOT in here (e.g. `foo`, `typo`, `Model`, `bogus`, `grey`, `session`)
    /// are literal text even inside a group/call.
    fn test_reg() -> HashSet<String> {
        reg(&[
            "dir", "branch", "model", "effort", "pct", "cpct", "x", "y", "z", "a", "b", "c",
            "c_ctx", "c_git", "c_pr", "c_session_pct", "c_weekly_pct", "session_reset_in",
            "weekly_reset_in", "c_vim", "cost", "val",
        ])
    }

    /// Visible form: replace the raw ESC byte (U+001B) with the two chars `\e`.
    fn esc(s: &str) -> String {
        s.replace('\u{1b}', "\\e")
    }

    fn map(vars: &[(&str, &str)]) -> HashMap<String, String> {
        vars.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Render against `test_reg()` and return the output in visible form. Panics
    /// if `render` errors (only used on value cases, which must be `Ok`).
    fn r(t: &str, vars: &[(&str, &str)]) -> String {
        esc(&render(t, &map(vars), &test_reg()).unwrap())
    }

    /// True if rendering the template errors (used for `expectErr` cases).
    fn is_err(t: &str, vars: &[(&str, &str)]) -> bool {
        render(t, &map(vars), &test_reg()).is_err()
    }

    /// `referenced_vars` against `test_reg()`, unwrapped.
    fn refs(t: &str) -> BTreeSet<String> {
        referenced_vars(t, &test_reg()).unwrap()
    }

    /// True if `referenced_vars` errors on the template.
    fn refs_err(t: &str) -> bool {
        referenced_vars(t, &test_reg()).is_err()
    }

    /// Build the expected `referenced_vars` result from a list of names.
    fn bset(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    // ================= ANCHOR CASES — spec ground truth (v3) =================
    // Registry = test_reg(); exact hand-derived expectations from the spec table.

    #[test]
    fn anchor_01_toplevel_ident_is_literal() {
        assert_eq!(r("dir", &[("dir", "D")]), "dir");
    }

    #[test]
    fn anchor_02_var_inside_group() {
        assert_eq!(r("(dir)", &[("dir", "D")]), "D");
    }

    #[test]
    fn anchor_03_group_collapses_when_undefined() {
        assert_eq!(r("(dir)", &[]), "");
    }

    #[test]
    fn anchor_04_literal_label_plus_group_var() {
        assert_eq!(r("Model: (pct)", &[("pct", "3%")]), "Model: 3%");
    }

    #[test]
    fn anchor_05_label_stays_group_drops() {
        assert_eq!(r("Model: (pct)", &[]), "Model: ");
    }

    #[test]
    fn anchor_06_group_literal_and_var() {
        assert_eq!(r("(Model: pct)", &[("pct", "3%")]), "Model: 3%");
    }

    #[test]
    fn anchor_07_group_collapses_on_pct() {
        assert_eq!(r("(Model: pct)", &[]), "");
    }

    #[test]
    fn anchor_08_group_no_var_never_collapses() {
        assert_eq!(r("(typo)", &[]), "typo");
    }

    #[test]
    fn anchor_09_call_substitutes_inside() {
        assert_eq!(r("magenta(model)", &[("model", "opus")]), "\\e[35mopus\\e[0m");
    }

    #[test]
    fn anchor_10_call_two_groups_tight() {
        assert_eq!(
            r("magenta((model)(effort))", &[("model", "opus"), ("effort", "xh")]),
            "\\e[35mopusxh\\e[0m"
        );
    }

    #[test]
    fn anchor_11_call_second_group_drops() {
        assert_eq!(
            r("magenta((model)(effort))", &[("model", "opus")]),
            "\\e[35mopus\\e[0m"
        );
    }

    #[test]
    fn anchor_12_quoted_label_plus_precolored_var() {
        assert_eq!(
            r("dim(\"ctx:\")(c_ctx)", &[("c_ctx", "\x1b[33m62%\x1b[0m")]),
            "\\e[2mctx:\\e[0m\\e[33m62%\\e[0m"
        );
    }

    #[test]
    fn anchor_13_single_quotes_identical() {
        assert_eq!(
            r("dim('ctx:')(c_ctx)", &[("c_ctx", "\x1b[33m62%\x1b[0m")]),
            "\\e[2mctx:\\e[0m\\e[33m62%\\e[0m"
        );
    }

    #[test]
    fn anchor_14_single_quote_literal_in_dq_string() {
        assert_eq!(r("green(\"it's\")", &[]), "\\e[32mit's\\e[0m");
    }

    #[test]
    fn anchor_15_double_quote_literal_in_sq_string() {
        assert_eq!(r("green('say \"hi\"')", &[]), "\\e[32msay \"hi\"\\e[0m");
    }

    #[test]
    fn anchor_16_name_not_style_literal_plus_group_var() {
        assert_eq!(r("foo(x)", &[("x", "X")]), "fooX");
    }

    #[test]
    fn anchor_17_name_not_style_group_collapses_literal_stays() {
        assert_eq!(r("foo(x)", &[]), "foo");
    }

    #[test]
    fn anchor_18_toplevel_registry_name_still_literal() {
        assert_eq!(r("x", &[("x", "X")]), "x");
    }

    #[test]
    fn anchor_19_known_var_undefined_collapses() {
        assert_eq!(r("(c_ctx)", &[]), "");
    }

    #[test]
    fn anchor_20_escaped_parens() {
        assert_eq!(r("\\(hi\\)", &[]), "(hi)");
    }

    #[test]
    fn anchor_21_escaped_single_quote() {
        assert_eq!(r("\\'", &[]), "'");
    }

    #[test]
    fn anchor_22_invalid_escape_errors() {
        assert!(is_err("a\\p", &[]));
    }

    #[test]
    fn anchor_23_unterminated_single_quote_errors() {
        assert!(is_err("'oops", &[]));
    }

    #[test]
    fn anchor_24_outer_gates_on_model_through_call() {
        assert_eq!(
            r("(magenta(model)magenta((effort)))", &[("model", "opus")]),
            "\\e[35mopus\\e[0m"
        );
    }

    #[test]
    fn anchor_25_all_vars_undefined_collapses_whole_group() {
        // OR-gating: pct is the group's only variable, so when it's undefined the
        // whole group — label included — collapses. (v3 boundary rule kept "S:".)
        assert_eq!(r("(\"S:\"(pct))", &[]), "");
    }

    #[test]
    fn anchor_26_pct_direct_in_group_label_collapses() {
        assert_eq!(r("(\"S:\"pct)", &[]), "");
    }

    // ============= referenced_vars ANCHORS — spec ground truth =============

    #[test]
    fn refs_anchor_toplevel_literals_not_vars() {
        assert_eq!(refs("dir (c_ctx) branch"), bset(&["c_ctx"]));
    }

    #[test]
    fn refs_anchor_group_and_call() {
        assert_eq!(refs("(dir) magenta(model)"), bset(&["dir", "model"]));
    }

    #[test]
    fn refs_anchor_label_plus_group() {
        assert_eq!(refs("Model: (pct)"), bset(&["pct"]));
    }

    #[test]
    fn refs_anchor_none_in_registry() {
        assert_eq!(refs("(typo)(foo)"), bset(&[]));
    }

    #[test]
    fn refs_anchor_quoted_literal_vs_bare_var() {
        assert_eq!(refs("dim(\"dir\")(dir)"), bset(&["dir"]));
    }

    // ================= NEW-BEHAVIOR coverage (v3 semantics) =================

    #[test]
    fn new_toplevel_is_all_literal_ignores_vars() {
        // Top level substitutes nothing: identifiers are literal even if defined.
        assert_eq!(r("model effort", &[("model", "opus"), ("effort", "xh")]), "model effort");
    }

    #[test]
    fn new_group_known_var_substitutes() {
        assert_eq!(r("(model)", &[("model", "opus")]), "opus");
    }

    #[test]
    fn new_group_typo_is_literal_var_still_gates() {
        // `typo` (not in registry) is literal and does not gate; `dir` does.
        assert_eq!(r("(dir typo)", &[("dir", "D")]), "D typo");
        assert_eq!(r("(dir typo)", &[]), "");
    }

    #[test]
    fn new_unknown_name_in_call_is_literal() {
        // Inside a call, an unknown name is literal text (not a var) and renders.
        assert_eq!(r("magenta(typo)", &[]), "\\e[35mtypo\\e[0m");
    }

    #[test]
    fn new_group_over_call_only_literal_never_collapses() {
        // The group references no variable (typo ∉ reg) -> it never collapses.
        assert_eq!(r("(magenta(typo))", &[]), "\\e[35mtypo\\e[0m");
    }

    #[test]
    fn new_name_not_style_opens_group() {
        // `session` is not a style: literal text, and `(pct)` opens a group.
        assert_eq!(r("session(pct)", &[("pct", "3%")]), "session3%");
        assert_eq!(r("session(pct)", &[]), "session");
    }

    #[test]
    fn new_single_quote_string_basic() {
        assert_eq!(r("'S:'", &[]), "S:");
    }

    #[test]
    fn new_single_quote_escaped_single_inside() {
        assert_eq!(r("'a\\'b'", &[]), "a'b");
    }

    #[test]
    fn new_double_quote_contains_single() {
        assert_eq!(r("\"it's\"", &[]), "it's");
    }

    #[test]
    fn new_single_quote_contains_double() {
        assert_eq!(r("'say \"hi\"'", &[]), "say \"hi\"");
    }

    #[test]
    fn new_escaped_single_quote_in_run() {
        assert_eq!(r("a\\'b", &[]), "a'b");
    }

    // ================= Seed cases (re-derived for v3) =================

    #[test]
    fn seed_01_group_var_renders_value() {
        // v0 `dir` was a top-level var; wrapped in a group to keep the intent.
        assert_eq!(r("(dir)", &[("dir", "statusline-rs")]), "statusline-rs");
    }

    #[test]
    fn seed_02_group_undefined_var_empty() {
        assert_eq!(r("(dir)", &[]), "");
    }

    #[test]
    fn seed_03_quoted_literal() {
        assert_eq!(r("\"S:\"", &[]), "S:");
    }

    #[test]
    fn seed_04_bare_punctuation() {
        assert_eq!(r("/↻", &[]), "/↻");
    }

    #[test]
    fn seed_05_space_between_groups_is_literal() {
        assert_eq!(
            r("(model) (effort)", &[("model", "opus-4-8[1m]"), ("effort", "xh")]),
            "opus-4-8[1m] xh"
        );
    }

    #[test]
    fn seed_05b_trailing_space_survives_dropped_group() {
        assert_eq!(r("(model) (effort)", &[("model", "opus-4-8[1m]")]), "opus-4-8[1m] ");
    }

    #[test]
    fn seed_06_abutment_tight() {
        assert_eq!(
            r("(model)(effort)", &[("model", "opus-4-8[1m]"), ("effort", "xh")]),
            "opus-4-8[1m]xh"
        );
    }

    #[test]
    fn seed_07_group_passes_value() {
        assert_eq!(r("(dir)", &[("dir", "statusline-rs")]), "statusline-rs");
    }

    #[test]
    fn seed_08_group_drops_undefined() {
        assert_eq!(r("(pct)", &[]), "");
    }

    #[test]
    fn seed_09_label_plus_var_tight() {
        assert_eq!(r("(\"S:\"pct)", &[("pct", "3%")]), "S:3%");
    }

    #[test]
    fn seed_09b_label_collapses() {
        assert_eq!(r("(\"S:\"pct)", &[]), "");
    }

    #[test]
    fn seed_10_leading_spaces_literal() {
        assert_eq!(r("(  pct)", &[("pct", "3%")]), "  3%");
    }

    #[test]
    fn seed_10b_leading_spaces_collapse() {
        assert_eq!(r("(  pct)", &[]), "");
    }

    #[test]
    fn seed_11_style_call() {
        assert_eq!(
            r("magenta(model)", &[("model", "opus-4-8[1m]")]),
            "\\e[35mopus-4-8[1m]\\e[0m"
        );
    }

    #[test]
    fn seed_12_space_inside_call() {
        assert_eq!(
            r("magenta(model effort)", &[("model", "opus-4-8[1m]"), ("effort", "xh")]),
            "\\e[35mopus-4-8[1m] xh\\e[0m"
        );
    }

    #[test]
    fn seed_13_tight_via_groups_in_call() {
        assert_eq!(
            r("magenta((model)(effort))", &[("model", "opus-4-8[1m]"), ("effort", "xh")]),
            "\\e[35mopus-4-8[1m]xh\\e[0m"
        );
    }

    #[test]
    fn seed_14_one_undefined_drops_tightly() {
        assert_eq!(
            r("magenta((model)(effort))", &[("model", "opus-4-8[1m]")]),
            "\\e[35mopus-4-8[1m]\\e[0m"
        );
    }

    #[test]
    fn seed_15_nested_styles_compose() {
        assert_eq!(r("red(bold(pct))", &[("pct", "3%")]), "\\e[31;1m3%\\e[0m");
    }

    #[test]
    fn seed_16_style_run_boundaries() {
        assert_eq!(r("red(\"a\"bold(\"b\"))", &[]), "\\e[31ma\\e[0m\\e[31;1mb\\e[0m");
    }

    #[test]
    fn seed_17_call_over_empty() {
        assert_eq!(r("magenta(pct)", &[]), "");
    }

    #[test]
    fn seed_18_precolored_passthrough() {
        assert_eq!(r("(cpct)", &[("cpct", "\x1b[33m3%\x1b[0m")]), "\\e[33m3%\\e[0m");
    }

    #[test]
    fn seed_19_nesting_noop() {
        assert_eq!(r("(((pct)))", &[("pct", "3%")]), "3%");
    }

    #[test]
    fn seed_19b_nesting_collapses() {
        assert_eq!(r("(((pct)))", &[]), "");
    }

    #[test]
    fn seed_20_escaped_parens() {
        assert_eq!(r("\\(\"hi\"\\)", &[]), "(hi)");
    }

    #[test]
    fn seed_21_all_vars_undefined_collapses_whole_group() {
        // OR-gating (was "S:" under the v3 boundary rule).
        assert_eq!(r("(\"S:\"(pct))", &[]), "");
    }

    #[test]
    fn seed_21b_nested_group_defined() {
        assert_eq!(r("(\"S:\"(pct))", &[("pct", "3%")]), "S:3%");
    }

    #[test]
    fn seed_22_call_transparent_drops_label() {
        assert_eq!(r("(\"S:\"magenta(pct))", &[]), "");
    }

    #[test]
    fn seed_22b_call_transparent_defined() {
        assert_eq!(
            r("(\"S:\"magenta(pct))", &[("pct", "3%")]),
            "S:\\e[35m3%\\e[0m"
        );
    }

    #[test]
    fn seed_e1_unbalanced_open() {
        assert!(is_err("(x", &[]));
    }

    #[test]
    fn seed_e2_unterminated_string() {
        assert!(is_err("\"abc", &[]));
    }

    #[test]
    fn unknown_name_before_paren_is_literal_group() {
        // v3: there is NO "unknown style" error. `bogus` is literal; `(x)` a group.
        assert_eq!(r("bogus(x)", &[("x", "1")]), "bogus1");
        assert_eq!(r("bogus(x)", &[]), "bogus");
        assert!(!is_err("bogus(x)", &[("x", "1")]));
    }

    #[test]
    fn seed_e4_dangling_backslash() {
        assert!(is_err("a\\", &[]));
    }

    // ================= Expanded cases (re-derived for v3) =================

    #[test]
    fn same_group_two_vars_shows_present_partial() {
        // OR-gating: model present, effort empty -> renders "model " with the gap.
        assert_eq!(r("(model effort)", &[("model", "opus-4-8[1m]")]), "opus-4-8[1m] ");
    }

    #[test]
    fn nested_groups_undef_first_survivor_second() {
        assert_eq!(
            r("((effort)(model))", &[("model", "opus-4-8[1m]")]),
            "opus-4-8[1m]"
        );
    }

    #[test]
    fn empty_string_is_undefined_at_boundary() {
        assert_eq!(
            r("((dir)(branch))", &[("dir", "statusline-rs"), ("branch", "")]),
            "statusline-rs"
        );
    }

    #[test]
    fn sibling_defined_var_keeps_group_alive() {
        // OR-gating: `dir` is defined, so the group renders even though `effort` is
        // not; the empty magenta(effort) contributes nothing. (v3 AND-rule: "".)
        assert_eq!(r("(magenta(effort)dir)", &[("dir", "statusline-rs")]), "statusline-rs");
    }

    #[test]
    fn nested_group_boundary_blocks_gate() {
        assert_eq!(r("((effort)dir)", &[("dir", "statusline-rs")]), "statusline-rs");
    }

    #[test]
    fn call_and_sibling_var_both_defined() {
        assert_eq!(
            r("(magenta(effort)dir)", &[("dir", "statusline-rs"), ("effort", "xh")]),
            "\\e[35mxh\\e[0mstatusline-rs"
        );
    }

    #[test]
    fn three_vars_gate_group_all_defined() {
        assert_eq!(
            r(
                "(dir branch pct)",
                &[("dir", "statusline-rs"), ("branch", "main"), ("pct", "3%")]
            ),
            "statusline-rs main 3%"
        );
    }

    #[test]
    fn three_vars_group_shows_present_with_gap() {
        // OR-gating: dir+pct present, branch empty -> the group renders the present
        // parts, leaving a gap where branch was (write `(dir)( branch)( pct)` for a
        // tidy result). Under the v3 AND-rule this whole group was "".
        assert_eq!(
            r("(dir branch pct)", &[("dir", "statusline-rs"), ("pct", "3%")]),
            "statusline-rs  3%"
        );
    }

    #[test]
    fn toplevel_call_no_group_literal_survives() {
        assert_eq!(r("magenta(\"S:\"pct)", &[]), "\\e[35mS:\\e[0m");
    }

    #[test]
    fn nested_group_in_call_drops_label() {
        assert_eq!(r("magenta((\"S:\"pct))", &[]), "");
    }

    #[test]
    fn literals_between_boundaries_survive() {
        assert_eq!(
            r("(\"A:\"(dir)\"B:\"(pct))", &[("dir", "statusline-rs")]),
            "A:statusline-rsB:"
        );
    }

    #[test]
    fn deep_nesting_boundary_noop() {
        assert_eq!(r("(((pct)dir))", &[("dir", "statusline-rs")]), "statusline-rs");
    }

    #[test]
    fn deep_nesting_call_transparent_styles() {
        assert_eq!(r("(((magenta(pct))))", &[("pct", "3%")]), "\\e[35m3%\\e[0m");
    }

    #[test]
    fn ident_before_paren_is_var_then_group() {
        // v3: `dir(` is NOT a call (dir isn't a style): dir is a var, `(` a group.
        assert!(!is_err("(dir(pct))", &[("dir", "statusline-rs"), ("pct", "3%")]));
        assert_eq!(
            r("(dir(pct))", &[("dir", "statusline-rs"), ("pct", "3%")]),
            "statusline-rs3%"
        );
        assert_eq!(r("(dir(pct))", &[]), "");
    }

    #[test]
    fn every_named_style_once() {
        assert_eq!(
            r(
                "bold(\"b\")dim(\"d\")italic(\"i\")underline(\"u\")gray(\"g\")orange(\"o\")red(\"r\")green(\"n\")yellow(\"y\")blue(\"l\")magenta(\"m\")cyan(\"c\")",
                &[]
            ),
            "\\e[1mb\\e[0m\\e[2md\\e[0m\\e[3mi\\e[0m\\e[4mu\\e[0m\\e[90mg\\e[0m\\e[38;5;208mo\\e[0m\\e[31mr\\e[0m\\e[32mn\\e[0m\\e[33my\\e[0m\\e[34ml\\e[0m\\e[35mm\\e[0m\\e[36mc\\e[0m"
        );
    }

    #[test]
    fn three_level_nesting() {
        assert_eq!(r("bold(underline(red(pct)))", &[("pct", "3%")]), "\\e[1;4;31m3%\\e[0m");
    }

    #[test]
    fn orange_nested_multicode_join() {
        assert_eq!(r("orange(bold(pct))", &[("pct", "3%")]), "\\e[38;5;208;1m3%\\e[0m");
    }

    #[test]
    fn style_run_boundaries_back_to_outer() {
        assert_eq!(
            r("red(\"a\"bold(\"b\")\"c\")", &[]),
            "\\e[31ma\\e[0m\\e[31;1mb\\e[0m\\e[31mc\\e[0m"
        );
    }

    #[test]
    fn adjacent_calls_literal_between() {
        assert_eq!(r("red(\"x\")\"|\"blue(\"y\")", &[]), "\\e[31mx\\e[0m|\\e[34my\\e[0m");
    }

    #[test]
    fn style_over_literal_and_var() {
        assert_eq!(
            r("green(\"branch:\"branch)", &[("branch", "main")]),
            "\\e[32mbranch:main\\e[0m"
        );
    }

    #[test]
    fn style_literal_survives_undefined_var() {
        assert_eq!(r("green(\"branch:\"branch)", &[]), "\\e[32mbranch:\\e[0m");
    }

    #[test]
    fn call_undefined_vars_literal_space_survives() {
        assert_eq!(r("red(effort pct)", &[]), "\\e[31m \\e[0m");
    }

    #[test]
    fn nested_call_all_undefined_emits_nothing() {
        assert_eq!(r("bold(dim(pct))", &[]), "");
    }

    #[test]
    fn style_group_drops_literals_merge() {
        assert_eq!(r("red(\"a\"(pct)\"b\")", &[]), "\\e[31mab\\e[0m");
    }

    #[test]
    fn style_literal_survives_gated_inner_group() {
        assert_eq!(r("blue(\"A\"(pct))", &[]), "\\e[34mA\\e[0m");
    }

    #[test]
    fn style_over_precolored_value() {
        assert_eq!(
            r("red(cpct)", &[("cpct", "\x1b[33m3%\x1b[0m")]),
            "\\e[31m\\e[33m3%\\e[0m\\e[0m"
        );
    }

    #[test]
    fn adjacent_same_style_calls_merge() {
        assert_eq!(r("red(\"a\")red(\"b\")", &[]), "\\e[31mab\\e[0m");
    }

    #[test]
    fn style_run_boundary_var_literal_mix() {
        assert_eq!(
            r(
                "magenta(model bold(effort) pct)",
                &[("model", "opus-4-8[1m]"), ("effort", "xh"), ("pct", "3%")]
            ),
            "\\e[35mopus-4-8[1m] \\e[0m\\e[35;1mxh\\e[0m\\e[35m 3%\\e[0m"
        );
    }

    #[test]
    fn group_trailing_space_survives() {
        assert_eq!(r("(pct )", &[("pct", "3%")]), "3% ");
    }

    #[test]
    fn group_trailing_space_collapses() {
        assert_eq!(r("(pct )", &[]), "");
    }

    #[test]
    fn group_inner_space_shows_when_one_var_present() {
        // OR-gating: partial content keeps the group (and its inner space) alive.
        assert_eq!(r("(model effort)", &[("model", "opus-4-8[1m]")]), "opus-4-8[1m] ");
    }

    #[test]
    fn top_level_two_spaces_dangle() {
        assert_eq!(r("(dir)  (branch)", &[("dir", "statusline-rs")]), "statusline-rs  ");
    }

    #[test]
    fn group_space_separator_dangles() {
        assert_eq!(r("(dir) (branch)", &[("dir", "statusline-rs")]), "statusline-rs ");
    }

    #[test]
    fn adjacent_groups_leading_space_collapses() {
        assert_eq!(r("(dir)( branch)", &[("dir", "statusline-rs")]), "statusline-rs");
    }

    #[test]
    fn adjacent_groups_space_from_inside_group() {
        assert_eq!(
            r("(dir)( branch)", &[("dir", "statusline-rs"), ("branch", "main")]),
            "statusline-rs main"
        );
    }

    #[test]
    fn call_literal_space_survives_undefined_var() {
        assert_eq!(r("magenta(  pct)", &[]), "\\e[35m  \\e[0m");
    }

    #[test]
    fn group_over_call_literal_space_collapses() {
        assert_eq!(r("(magenta(  pct))", &[]), "");
    }

    #[test]
    fn group_over_call_literal_space_defined() {
        assert_eq!(r("(magenta(  pct))", &[("pct", "3%")]), "\\e[35m  3%\\e[0m");
    }

    #[test]
    fn tab_literal_survives_in_group() {
        assert_eq!(r("(\tpct)", &[("pct", "3%")]), "\t3%");
    }

    #[test]
    fn tab_dangles_top_level() {
        assert_eq!(r("(dir)\t(branch)", &[("dir", "statusline-rs")]), "statusline-rs\t");
    }

    #[test]
    fn literal_spaces_around_opaque_value() {
        assert_eq!(
            r("( cpct )", &[("cpct", "\x1b[33m3%\x1b[0m")]),
            " \\e[33m3%\\e[0m "
        );
    }

    #[test]
    fn leading_space_outside_vs_inside_call() {
        assert_eq!(r("( green( branch))", &[("branch", "main")]), " \\e[32m main\\e[0m");
    }

    #[test]
    fn str_punctuation() {
        assert_eq!(r("\"a/b:c%\"", &[]), "a/b:c%");
    }

    #[test]
    fn str_parens_and_letter_inside_are_literal() {
        assert_eq!(r("\"(x)\"", &[]), "(x)");
    }

    #[test]
    fn str_escaped_quote() {
        assert_eq!(r("\"a\\\"b\"", &[]), "a\"b");
    }

    #[test]
    fn digits_and_percent_are_literal() {
        assert_eq!(r("100%", &[]), "100%");
    }

    #[test]
    fn square_brackets_are_literal_around_group_var() {
        assert_eq!(r("[(pct)]", &[("pct", "3%")]), "[3%]");
    }

    #[test]
    fn all_four_escapes() {
        assert_eq!(r("\\(\\)\\\"\\\\", &[]), "()\"\\");
    }

    #[test]
    fn escaped_parens_are_literal_not_group_defined() {
        // Escaped `\(` `\)` are literal chars; the real `(pct)` group gates.
        assert_eq!(r("\\((pct)\\)", &[("pct", "3%")]), "(3%)");
    }

    #[test]
    fn escaped_parens_are_literal_not_group_undefined() {
        assert_eq!(r("\\((pct)\\)", &[]), "()");
    }

    #[test]
    fn err_unbalanced_open_extra() {
        assert!(is_err("((pct)", &[]));
    }

    #[test]
    fn err_unbalanced_close_extra() {
        assert!(is_err("(pct))", &[]));
    }

    #[test]
    fn err_unterminated_string_via_escaped_quote() {
        assert!(is_err("\"a\\\"", &[]));
    }

    #[test]
    fn unknown_style_name_grey_is_literal() {
        // v3: `grey` is not a known style -> literal text, `(pct)` opens a group.
        assert_eq!(r("grey(pct)", &[("pct", "3%")]), "grey3%");
        assert_eq!(r("grey(pct)", &[]), "grey");
    }

    #[test]
    fn err_dangling_backslash() {
        assert!(is_err("pct\\", &[]));
    }

    #[test]
    fn empty_template_renders_empty() {
        assert_eq!(r("", &[]), "");
    }

    // ================= v1 §3 seed cases (engine extensions) =================
    // Newlines/layout preprocessing, `br()`, strict literal escapes, `sgr()`,
    // `hex()`.

    #[test]
    fn v1_seed_br_between_groups() {
        assert_eq!(r("(x)br()(y)", &[("x", "X"), ("y", "Y")]), "X\nY");
    }

    #[test]
    fn v1_seed_bare_break() {
        assert_eq!(r("br()", &[]), "\n");
    }

    #[test]
    fn v1_seed_literal_newline_breaks() {
        assert_eq!(r("(a)\n(b)", &[("a", "1"), ("b", "2")]), "1\n2");
    }

    #[test]
    fn v1_seed_continuation_joins_indent_strip() {
        assert_eq!(
            r("cyan(dir)\\\n  (  x)", &[("dir", "D"), ("x", "P")]),
            "\\e[36mD\\e[0m  P"
        );
    }

    #[test]
    fn v1_seed_valid_literal_escapes() {
        assert_eq!(r("\\(\\)", &[]), "()");
    }

    #[test]
    fn v1_seed_invalid_escape_p_errors() {
        assert!(is_err("a\\p", &[]));
    }

    #[test]
    fn v1_seed_dangling_backslash_errors() {
        assert!(is_err("x\\", &[]));
    }

    #[test]
    fn v1_seed_sgr_basic() {
        assert_eq!(r("sgr(\"38;5;208\"x)", &[("x", "P")]), "\\e[38;5;208mP\\e[0m");
    }

    #[test]
    fn v1_seed_sgr_tight_multicode() {
        assert_eq!(
            r("sgr(\"1;31\"(a)(b))", &[("a", "A"), ("b", "B")]),
            "\\e[1;31mAB\\e[0m"
        );
    }

    #[test]
    fn v1_seed_hex_truecolor() {
        assert_eq!(r("hex(\"#5f8700\"x)", &[("x", "P")]), "\\e[38;2;95;135;0mP\\e[0m");
    }

    #[test]
    fn v1_seed_hex_bad_digits_errors() {
        assert!(is_err("hex(\"#zz0000\"x)", &[("x", "P")]));
    }

    #[test]
    fn v1_seed_sgr_spec_not_literal_errors() {
        assert!(is_err("sgr(a b)", &[("a", "1"), ("b", "2")]));
    }

    #[test]
    fn v1_seed_sgr_nests_named_style() {
        assert_eq!(r("red(sgr(\"1\"x))", &[("x", "P")]), "\\e[31;1mP\\e[0m");
    }

    // ============ v1 expanded grammar cases (validated vs spec) ============

    #[test]
    fn v1_br_between_groups_one_undefined() {
        assert_eq!(r("(x)br()(y)", &[("x", "X")]), "X\n");
    }

    #[test]
    fn v1_br_with_content_prepends_newline() {
        assert_eq!(r("br(x)", &[("x", "P")]), "\nP");
    }

    #[test]
    fn v1_br_noarg_inside_style_splits_runs() {
        assert_eq!(r("red(\"a\"br()\"b\")", &[]), "\\e[31ma\\e[0m\n\\e[31mb\\e[0m");
    }

    #[test]
    fn v1_bare_newline_strips_following_indent() {
        assert_eq!(r("(a)\n   (b)", &[("a", "1"), ("b", "2")]), "1\n2");
    }

    #[test]
    fn v1_leading_newline_and_spaces_stripped() {
        assert_eq!(r("\n  (a)", &[("a", "1")]), "1");
    }

    #[test]
    fn v1_consecutive_newlines_preserved() {
        assert_eq!(r("(a)\n\n(b)", &[("a", "1"), ("b", "2")]), "1\n\n2");
    }

    #[test]
    fn v1_continuation_joins_string_and_var() {
        assert_eq!(r("\"A:\"\\\n    (x)", &[("x", "P")]), "A:P");
    }

    #[test]
    fn v1_backslash_newline_at_end_is_continuation() {
        assert_eq!(r("(a)\\\n", &[("a", "1")]), "1");
    }

    #[test]
    fn v1_escaped_backslash_between_vars() {
        assert_eq!(r("(a)\\\\(b)", &[("a", "1"), ("b", "2")]), "1\\2");
    }

    #[test]
    fn v1_invalid_escape_n_errors() {
        assert!(is_err("a\\nb", &[("a", "A")]));
    }

    #[test]
    fn v1_sgr_bare_spec_abuts_var() {
        assert_eq!(r("sgr(1;31x)", &[("x", "P")]), "\\e[1;31mP\\e[0m");
    }

    #[test]
    fn v1_sgr_empty_content_emits_nothing() {
        assert_eq!(r("sgr(\"1;31\"x)", &[]), "");
    }

    #[test]
    fn v1_hex_mixed_digits_letters() {
        assert_eq!(r("hex(\"#1a2b3c\"x)", &[("x", "P")]), "\\e[38;2;26;43;60mP\\e[0m");
    }

    #[test]
    fn v1_hex_missing_hash_errors() {
        assert!(is_err("hex(\"123456\"x)", &[("x", "P")]));
    }

    #[test]
    fn v1_bold_wraps_hex_truecolor() {
        assert_eq!(
            r("bold(hex(\"#5f8700\"x))", &[("x", "P")]),
            "\\e[1;38;2;95;135;0mP\\e[0m"
        );
    }

    #[test]
    fn v1_gating_through_sgr_drops_label() {
        assert_eq!(r("(\"S:\"sgr(\"1;31\"x))", &[]), "");
    }

    // ================= v1 §4 referenced_vars() (re-derived for v3) =================

    #[test]
    fn v1_refs_cyan_bare_group() {
        // v3: bare `c_ctx` at top level is literal, NOT a referenced var.
        assert_eq!(refs("cyan(dir) c_ctx (  c_pr)"), bset(&["c_pr", "dir"]));
    }

    #[test]
    fn v1_refs_sgr_br_dim() {
        assert_eq!(refs("sgr(\"1;31\"x) br() dim(\"S:\")"), bset(&["x"]));
    }

    #[test]
    fn v1_refs_magenta_two_groups() {
        assert_eq!(refs("magenta((model)(effort))"), bset(&["effort", "model"]));
    }

    #[test]
    fn v1_refs_red_bold_pct() {
        assert_eq!(refs("red(bold(pct))"), bset(&["pct"]));
    }

    #[test]
    fn v1_refs_no_vars_empty_set() {
        assert_eq!(refs("red(\"a\"bold(\"b\"))"), bset(&[]));
    }

    #[test]
    fn v1_refs_hex_sgr_group_var() {
        // v3: the third `z` must be grouped to count as a var (top-level = literal).
        assert_eq!(
            refs("hex(\"#5f8700\"x) sgr(\"38;5;208\"y) (z)"),
            bset(&["x", "y", "z"])
        );
    }

    #[test]
    fn v1_refs_br_var_vs_builtin() {
        // v3: bare `br` at top level is literal (and not in registry); `br()` is the
        // builtin — neither is a variable.
        assert_eq!(refs("br dim(br())"), bset(&[]));
    }

    #[test]
    fn v1_refs_sgr_bare_spec_group() {
        assert_eq!(
            refs("sgr(38;5;208(model)) cyan(branch)"),
            bset(&["branch", "model"])
        );
    }

    #[test]
    fn v1_refs_escaped_parens_literal() {
        // Escaped `\(` `\)` are literal; `(dir)` is a real group so `dir` counts.
        assert_eq!(
            refs("\\(\"S:\"(dir)\\)(  c_git)"),
            bset(&["c_git", "dir"])
        );
    }

    #[test]
    fn v1_refs_default_like_template() {
        assert_eq!(
            refs("cyan(dir)(  green( branch)( c_git))(  c_pr)  magenta((model)(effort))"),
            bset(&["branch", "c_git", "c_pr", "dir", "effort", "model"])
        );
    }

    #[test]
    fn v1_refs_invalid_escape_errors() {
        assert!(refs_err("a\\p"));
    }

    #[test]
    fn v1_refs_unbalanced_open_errors() {
        assert!(refs_err("(dir"));
    }

    // ================= v4: '+' concatenation join =================
    // Inside a group/call, `+` is a no-op token joining its neighbors tightly —
    // identical output to adjacency, but explicit. Whitespace around it prints
    // (not swallowed). Literal `+` in a group/call is `'+'` or `\+`; at top level
    // `+` is ordinary literal text.

    #[test]
    fn plus_join_equals_adjacency() {
        assert_eq!(
            r("(magenta(a)+magenta(b))", &[("a", "A"), ("b", "B")]),
            r("(magenta(a)magenta(b))", &[("a", "A"), ("b", "B")])
        );
    }

    #[test]
    fn plus_join_label_and_var_tight() {
        assert_eq!(
            r("(dim('S:')+c_session_pct)", &[("c_session_pct", "\x1b[33m72%\x1b[0m")]),
            "\\e[2mS:\\e[0m\\e[33m72%\\e[0m"
        );
    }

    #[test]
    fn plus_is_transparent_for_gating() {
        // An undefined operand collapses the enclosing group, exactly like a bare
        // var would — `+` does NOT introduce a boundary (unlike `(value)`).
        assert_eq!(r("(dim('S:')+c_session_pct)", &[]), "");
    }

    #[test]
    fn plus_literal_at_top_level() {
        // Top level is not a substitution context: `+` (and the idents) are literal.
        assert_eq!(r("a+b", &[("a", "1"), ("b", "2")]), "a+b");
    }

    #[test]
    fn plus_literal_via_escape_or_quote_in_group() {
        assert_eq!(r("(a\\+b)", &[("a", "1"), ("b", "2")]), "1+2");
        assert_eq!(r("('a+b')", &[]), "a+b");
    }

    #[test]
    fn plus_swallows_surrounding_whitespace() {
        // JS-style: `a + b` == `a+b`; `+` ignores the whitespace on both sides.
        assert_eq!(r("(a + b)", &[("a", "1"), ("b", "2")]), "12");
        assert_eq!(r("(a+b)", &[("a", "1"), ("b", "2")]), "12");
    }

    #[test]
    fn plus_refs_counts_operands_only() {
        // `+` itself is nothing; its variable operands are referenced as usual.
        assert_eq!(refs("(dim('S:')+c_session_pct)"), bset(&["c_session_pct"]));
    }

    // ================= v4: comma arguments + OR-gating =================
    // Call arguments are comma-separated (structural, whitespace-insensitive); `,`
    // is literal outside a call. A group collapses only when ALL its variables are
    // undefined, so `(var)` and `+var` behave identically.

    #[test]
    fn comma_separates_call_arguments() {
        // sgr's first arg is the SGR spec, the rest is content — tight, no space.
        assert_eq!(r("sgr('1;32', x)", &[("x", "P")]), "\\e[1;32mP\\e[0m");
        assert_eq!(r("hex('#5f8700', x)", &[("x", "P")]), "\\e[38;2;95;135;0mP\\e[0m");
    }

    #[test]
    fn comma_is_whitespace_insensitive_in_calls() {
        assert_eq!(
            r("sgr('1;32',x)", &[("x", "P")]),
            r("sgr('1;32',     x)", &[("x", "P")])
        );
        assert_eq!(r("sgr('1;32',   x)", &[("x", "P")]), "\\e[1;32mP\\e[0m");
    }

    #[test]
    fn comma_is_literal_outside_a_call() {
        // In a group `,` is ordinary text; at top level too.
        assert_eq!(r("(x, y)", &[("x", "X"), ("y", "Y")]), "X, Y");
        assert_eq!(r("a,b", &[("a", "1"), ("b", "2")]), "a,b");
    }

    #[test]
    fn comma_structural_in_call_vs_literal_in_group() {
        // Same text, different context: tight in a call, literal in a group.
        assert_eq!(r("magenta(x, y)", &[("x", "X"), ("y", "Y")]), "\\e[35mXY\\e[0m");
        assert_eq!(r("(magenta(x), y)", &[("x", "X"), ("y", "Y")]), "\\e[35mX\\e[0m, Y");
    }

    #[test]
    fn comma_literal_via_escape_or_quote_in_call() {
        assert_eq!(r("magenta(x\\,y)", &[("x", "X"), ("y", "Y")]), "\\e[35mX,Y\\e[0m");
        assert_eq!(r("magenta('x,y')", &[]), "\\e[35mx,y\\e[0m");
    }

    #[test]
    fn default_branch_pattern_keeps_leading_space() {
        // The DEFAULT's git label: spec, then ' ' concatenated to the branch.
        assert_eq!(
            r("sgr('1;32', ' '+branch)", &[("branch", "main")]),
            "\\e[1;32m main\\e[0m"
        );
    }

    #[test]
    fn paren_var_equals_plus_var() {
        // OR-gating makes `(c_ctx)` and `+c_ctx` interchangeable after a label.
        let present: &[(&str, &str)] = &[("c_ctx", "\x1b[33m62%\x1b[0m")];
        assert_eq!(
            r("(dim('ctx:')(c_ctx))", present),
            r("(dim('ctx:')+c_ctx)", present)
        );
        assert_eq!(r("(dim('ctx:')(c_ctx))", present), "\\e[2mctx:\\e[0m\\e[33m62%\\e[0m");
        // Absent: the only variable is empty, so the whole label collapses.
        assert_eq!(r("(dim('ctx:')(c_ctx))", &[]), "");
        assert_eq!(r("(dim('ctx:')+c_ctx)", &[]), "");
    }

    #[test]
    fn or_gating_keeps_segment_when_one_var_present() {
        // Clean-tree git case: branch present, c_git empty -> branch survives and
        // the badge group collapses. No repo (both empty) -> whole segment gone.
        assert_eq!(
            r("(sgr('1;32', ' '+branch)( c_git))", &[("branch", "main")]),
            "\\e[1;32m main\\e[0m"
        );
        assert_eq!(r("(sgr('1;32', ' '+branch)( c_git))", &[]), "");
    }
}
