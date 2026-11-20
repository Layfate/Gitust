//! Lightweight syntax highlighting and word-level diff emphasis for diff rows.
//!
//! A small table-driven lexer instead of a grammar engine: it knows comments,
//! strings, numbers, keywords, types and attributes for the common languages,
//! which is what makes a diff readable, at a tiny cost in binary size and time.
//! Everything works on the *displayed* text (tabs already expanded), so
//! character offsets map straight onto what the user sees.

use std::collections::HashSet;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Tok {
    Plain = 0,
    Keyword = 1,
    Type = 2,
    String = 3,
    Number = 4,
    Comment = 5,
    Attr = 6,
    Func = 7,
}

/// A run of characters with one token class, optionally emphasised as changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seg {
    pub text: String,
    pub tok: Tok,
    pub emph: bool,
}

#[derive(Debug)]
pub struct Lang {
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    quotes: &'static [char],
    keywords: &'static [&'static str],
    /// `#[...]` (Rust) / `@name` (Java, Python, TS…) attributes.
    attr_prefix: Option<&'static str>,
    /// Lowercase, keywords only: SQL.
    case_insensitive: bool,
    /// Markdown, YAML, TOML: keys and headings instead of code.
    kind: Kind,
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Code,
    Markdown,
    Config,
}

#[rustfmt::skip]
const C_LIKE: &[&str] = &[
    "if", "else", "for", "while", "do", "switch", "case", "default", "break", "continue", "return", "goto",
    "struct", "union", "enum", "typedef", "static", "const", "extern", "volatile", "inline", "sizeof", "void",
    "int", "char", "short", "long", "float", "double", "unsigned", "signed", "bool", "true", "false", "nullptr",
    "NULL", "class", "public", "private", "protected", "virtual", "override", "template", "typename", "namespace",
    "using", "new", "delete", "this", "throw", "try", "catch", "auto", "constexpr", "final", "abstract",
    "interface", "extends", "implements", "import", "package", "super", "null", "instanceof", "synchronized",
    "boolean", "byte", "var", "let", "function", "async", "await", "yield", "export", "from", "of", "in", "typeof",
    "undefined", "readonly", "type", "keyof", "as", "is", "declare", "module", "get", "set", "internal", "sealed",
    "base", "lock", "foreach", "func", "go", "chan", "select", "defer", "map", "range", "fallthrough", "fun", "val",
    "when", "object", "companion", "data", "suspend", "guard", "let", "where", "protocol", "extension", "self",
    "Self", "init", "deinit", "weak", "lazy", "mut", "pub", "fn", "impl", "trait", "mod", "crate", "use", "match",
    "loop", "move", "ref", "dyn", "unsafe", "where", "macro_rules", "in-out", "property", "callback", "component",
    "inherits", "export", "out", "root", "parent", "global", "animate", "states", "transitions",
];
#[rustfmt::skip]
const PYTHON: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except",
    "False", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "None", "nonlocal", "not",
    "or", "pass", "raise", "return", "True", "try", "while", "with", "yield", "match", "case", "self",
];
#[rustfmt::skip]
const RUBY: &[&str] = &[
    "def", "end", "if", "elsif", "else", "unless", "while", "until", "for", "in", "do", "return", "class", "module",
    "self", "nil", "true", "false", "and", "or", "not", "then", "yield", "begin", "rescue", "ensure", "raise",
    "case", "when", "require", "attr_accessor", "lambda", "proc",
];
#[rustfmt::skip]
const SHELL: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac", "in", "function",
    "return", "local", "export", "readonly", "set", "unset", "echo", "exit", "source", "param", "foreach", "true",
    "false", "null",
];
#[rustfmt::skip]
const LUA: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in", "local", "nil",
    "not", "or", "repeat", "return", "then", "true", "until", "while", "self",
];
#[rustfmt::skip]
const SQL: &[&str] = &[
    "select", "from", "where", "insert", "into", "values", "update", "set", "delete", "create", "table", "drop",
    "alter", "add", "index", "primary", "key", "foreign", "references", "join", "left", "right", "inner", "outer",
    "on", "and", "or", "not", "null", "is", "in", "as", "order", "by", "group", "having", "limit", "offset",
    "distinct", "union", "all", "exists", "case", "when", "then", "else", "end", "default", "unique", "integer",
    "text", "begin", "commit", "transaction", "if",
];
const CSS: &[&str] = &["important", "media", "import", "keyframes", "from", "to", "supports", "font-face"];
const NONE: &[&str] = &[];

fn lang(ext: &str) -> Option<&'static Lang> {
    macro_rules! l {
        ($lc:expr, $bc:expr, $q:expr, $kw:expr, $attr:expr) => {{
            static L: Lang = Lang {
                line_comments: $lc,
                block_comment: $bc,
                quotes: $q,
                keywords: $kw,
                attr_prefix: $attr,
                case_insensitive: false,
                kind: Kind::Code,
            };
            &L
        }};
    }
    static SQL_L: Lang = Lang {
        line_comments: &["--"],
        block_comment: Some(("/*", "*/")),
        quotes: &['\'', '"'],
        keywords: SQL,
        attr_prefix: None,
        case_insensitive: true,
        kind: Kind::Code,
    };
    static MD: Lang = Lang {
        line_comments: &[],
        block_comment: Some(("<!--", "-->")),
        quotes: &['`'],
        keywords: NONE,
        attr_prefix: None,
        case_insensitive: false,
        kind: Kind::Markdown,
    };
    static CONF_HASH: Lang = Lang {
        line_comments: &["#"],
        block_comment: None,
        quotes: &['"', '\''],
        keywords: &["true", "false", "null", "yes", "no", "on", "off"],
        attr_prefix: None,
        case_insensitive: false,
        kind: Kind::Config,
    };
    static CONF_INI: Lang = Lang {
        line_comments: &[";", "#"],
        block_comment: None,
        quotes: &['"'],
        keywords: &["true", "false"],
        attr_prefix: None,
        case_insensitive: false,
        kind: Kind::Config,
    };
    const C_BLOCK: Option<(&str, &str)> = Some(("/*", "*/"));
    Some(match ext {
        "rs" => l!(&["//"], C_BLOCK, &['"'], C_LIKE, Some("#")),
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "m" | "mm" | "cs" | "java" | "kt" | "kts" | "scala"
        | "swift" | "dart" | "go" | "zig" | "slint" | "proto" | "gradle" | "groovy" => {
            l!(&["//"], C_BLOCK, &['"', '\''], C_LIKE, Some("@"))
        }
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "mts" | "cts" | "tsx" | "vue" | "svelte" => {
            l!(&["//"], C_BLOCK, &['"', '\'', '`'], C_LIKE, Some("@"))
        }
        "py" | "pyi" | "pyw" => l!(&["#"], None, &['"', '\''], PYTHON, Some("@")),
        "rb" | "rake" | "gemspec" => l!(&["#"], None, &['"', '\''], RUBY, None),
        "sh" | "bash" | "zsh" | "fish" | "ps1" | "psm1" => l!(&["#"], None, &['"', '\''], SHELL, None),
        "lua" | "luau" => l!(&["--"], Some(("--[[", "]]")), &['"', '\''], LUA, None),
        "css" | "scss" | "sass" | "less" => l!(&["//"], C_BLOCK, &['"', '\''], CSS, Some("@")),
        "html" | "htm" | "xml" | "svg" | "xaml" | "csproj" | "plist" | "wxs" => {
            l!(&[], Some(("<!--", "-->")), &['"', '\''], NONE, None)
        }
        "json" | "jsonc" | "json5" => l!(&["//"], C_BLOCK, &['"'], &["true", "false", "null"], None),
        "sql" => &SQL_L,
        "md" | "markdown" | "mdx" => &MD,
        "toml" | "yaml" | "yml" | "env" | "conf" | "properties" | "gitignore" | "gitattributes" | "dockerignore"
        | "editorconfig" => &CONF_HASH,
        "ini" | "cfg" => &CONF_INI,
        _ => return None,
    })
}

/// The language for a path, by extension or a few well-known file names.
pub fn lang_for(path: &str) -> Option<&'static Lang> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "dockerfile" | "makefile" | "cmakelists.txt" | ".bashrc" | ".zshrc" | ".profile" => return lang("sh"),
        "cargo.lock" | ".gitignore" | ".gitattributes" | ".dockerignore" | ".editorconfig" | ".env" => {
            return lang("gitignore");
        }
        _ => {}
    }
    let ext = lower.rsplit_once('.').map(|x| x.1)?;
    lang(ext)
}

fn keyword_set(l: &'static Lang) -> &'static HashSet<&'static str> {
    // One lazily built set per keyword table (keyed by its address).
    type Sets = std::sync::Mutex<Vec<(usize, &'static HashSet<&'static str>)>>;
    static SETS: OnceLock<Sets> = OnceLock::new();
    let key = l.keywords.as_ptr() as usize;
    let mut sets = SETS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, s)) = sets.iter().find(|(k, _)| *k == key) {
        return s;
    }
    let set: &'static HashSet<&'static str> = Box::leak(Box::new(l.keywords.iter().copied().collect()));
    sets.push((key, set));
    set
}

/// Highlights one side of a diff line by line, carrying block-comment state.
pub struct Highlighter {
    lang: &'static Lang,
    in_block: bool,
}

impl Highlighter {
    pub fn new(lang: &'static Lang) -> Self {
        Self { lang, in_block: false }
    }

    pub fn reset(&mut self) {
        self.in_block = false;
    }

    /// Token runs for `line` as `(start char, char count, token)`, covering the whole line.
    pub fn line(&mut self, line: &str) -> Vec<(usize, usize, Tok)> {
        let chars: Vec<char> = line.chars().collect();
        let l = self.lang;
        let mut out: Vec<(usize, usize, Tok)> = Vec::new();
        let mut push = |start: usize, end: usize, tok: Tok| {
            if end > start {
                match out.last_mut() {
                    Some(last) if last.2 == tok && last.0 + last.1 == start => last.1 += end - start,
                    _ => out.push((start, end - start, tok)),
                }
            }
        };
        let starts_with = |i: usize, pat: &str| pat.chars().enumerate().all(|(o, pc)| chars.get(i + o) == Some(&pc));
        let find = |from: usize, pat: &str| (from..chars.len()).find(|&k| starts_with(k, pat));

        let mut i = 0;
        if self.in_block {
            let (_, close) = l.block_comment.expect("in_block implies a block comment");
            match find(0, close) {
                Some(k) => {
                    let end = k + close.chars().count();
                    push(0, end, Tok::Comment);
                    i = end;
                    self.in_block = false;
                }
                None => {
                    push(0, chars.len(), Tok::Comment);
                    return out;
                }
            }
        }

        if l.kind == Kind::Markdown {
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                push(0, chars.len(), Tok::Keyword);
                return out;
            }
            if trimmed.starts_with("```") {
                push(0, chars.len(), Tok::String);
                return out;
            }
        }
        // `key = value` / `key: value` / `[section]` in config files.
        let mut config_key_end = None;
        if l.kind == Kind::Config && i == 0 {
            let lead = chars.iter().take_while(|c| c.is_whitespace() || **c == '-').count();
            if chars.get(lead) == Some(&'[') {
                push(0, chars.len(), Tok::Type);
                return out;
            }
            if let Some(k) = (lead..chars.len()).find(|&k| chars[k] == '=' || chars[k] == ':')
                && k > lead
                && !chars[lead..k].iter().any(|c| matches!(c, '"' | '\'' | '#'))
            {
                config_key_end = Some(k);
            }
        }

        while i < chars.len() {
            let c = chars[i];
            if let Some(end) = config_key_end
                && i < end
            {
                push(i, end, Tok::Attr);
                i = end;
                continue;
            }
            if let Some(lc) = l.line_comments.iter().find(|p| starts_with(i, p)) {
                // `#` in shell-like languages only starts a comment at a word boundary.
                let boundary = i == 0 || chars[i - 1].is_whitespace() || *lc != "#";
                if boundary {
                    push(i, chars.len(), Tok::Comment);
                    return out;
                }
            }
            if let Some((open, close)) = l.block_comment
                && starts_with(i, open)
            {
                let from = i + open.chars().count();
                match find(from, close) {
                    Some(k) => {
                        let end = k + close.chars().count();
                        push(i, end, Tok::Comment);
                        i = end;
                    }
                    None => {
                        push(i, chars.len(), Tok::Comment);
                        self.in_block = true;
                        return out;
                    }
                }
                continue;
            }
            if l.quotes.contains(&c) {
                let mut k = i + 1;
                while k < chars.len() && chars[k] != c {
                    if chars[k] == '\\' {
                        k += 1;
                    }
                    k += 1;
                }
                // Rust lifetimes ('a) aren't strings.
                let lifetime = c == '\'' && k >= chars.len() && l.attr_prefix == Some("#");
                if !lifetime {
                    let end = (k + 1).min(chars.len());
                    push(i, end, Tok::String);
                    i = end;
                    continue;
                }
            }
            if let Some(prefix) = l.attr_prefix
                && starts_with(i, prefix)
                && (prefix == "@" || chars.get(i + 1).is_some_and(|n| *n == '[' || *n == '!'))
                && (i == 0 || !chars[i - 1].is_alphanumeric())
            {
                let mut k = i + 1;
                if prefix == "#" {
                    // #[...] up to the matching bracket on this line.
                    let mut depth = 0;
                    while k < chars.len() {
                        match chars[k] {
                            '[' => depth += 1,
                            ']' => {
                                depth -= 1;
                                if depth == 0 {
                                    k += 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                        k += 1;
                    }
                } else {
                    while k < chars.len() && (chars[k].is_alphanumeric() || matches!(chars[k], '_' | '.')) {
                        k += 1;
                    }
                }
                if k > i + 1 {
                    push(i, k, Tok::Attr);
                    i = k;
                    continue;
                }
            }
            if c.is_ascii_digit() && (i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_')) {
                let mut k = i + 1;
                while k < chars.len() && (chars[k].is_ascii_alphanumeric() || matches!(chars[k], '.' | '_')) {
                    k += 1;
                }
                push(i, k, Tok::Number);
                i = k;
                continue;
            }
            if c.is_alphabetic() || c == '_' || c == '$' {
                // CSS identifiers (`font-size`) may contain dashes; elsewhere `-` is an operator.
                let css = std::ptr::eq(l.keywords, CSS);
                let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$' || (css && c == '-');
                let mut k = i + 1;
                while k < chars.len() && ident(chars[k]) {
                    k += 1;
                }
                let word: String = chars[i..k].iter().collect();
                let is_kw = if l.case_insensitive {
                    keyword_set(l).contains(word.to_ascii_lowercase().as_str())
                } else {
                    keyword_set(l).contains(word.as_str())
                };
                let next = chars[k..].iter().find(|c| !c.is_whitespace()).copied();
                // Calls `name(` and Rust macros `name!`.
                let call = next == Some('(') || (next == Some('!') && l.attr_prefix == Some("#"));
                let tok = if is_kw {
                    Tok::Keyword
                } else if l.kind == Kind::Code && call {
                    Tok::Func
                } else if l.kind == Kind::Code && c.is_uppercase() && word.chars().any(|c| c.is_lowercase()) {
                    Tok::Type
                } else {
                    Tok::Plain
                };
                push(i, k, tok);
                i = k;
                continue;
            }
            push(i, i + 1, Tok::Plain);
            i += 1;
        }
        out
    }
}

// ───────────────────────────── word diff ─────────────────────────────

/// Splits into words, runs of whitespace, and single punctuation characters,
/// as `(start char, char count)`.
fn words(s: &[char]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let class = |c: char| {
            if c.is_alphanumeric() || c == '_' {
                0
            } else if c.is_whitespace() {
                1
            } else {
                2
            }
        };
        let k0 = class(s[i]);
        let mut k = i + 1;
        if k0 != 2 {
            while k < s.len() && class(s[k]) == k0 {
                k += 1;
            }
        }
        out.push((i, k - i));
        i = k;
    }
    out
}

/// Character ranges that differ between `old` and `new`, word by word.
/// Returns `None` when the lines have too little in common for emphasis to
/// help (then the whole line is the change).
/// Character ranges as `(start, count)`.
pub type Ranges = Vec<(usize, usize)>;

pub fn changed_ranges(old: &str, new: &str) -> Option<(Ranges, Ranges)> {
    let (a, b): (Vec<char>, Vec<char>) = (old.chars().collect(), new.chars().collect());
    let (wa, wb) = (words(&a), words(&b));
    if wa.len() * wb.len() > 250_000 {
        return None; // pathological line; skip
    }
    let tok = |s: &[char], w: (usize, usize)| -> String { s[w.0..w.0 + w.1].iter().collect() };
    let ta: Vec<String> = wa.iter().map(|w| tok(&a, *w)).collect();
    let tb: Vec<String> = wb.iter().map(|w| tok(&b, *w)).collect();
    // LCS table over tokens.
    let (n, m) = (ta.len(), tb.len());
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * (m + 1) + j] = if ta[i] == tb[j] {
                dp[(i + 1) * (m + 1) + j + 1] + 1
            } else {
                dp[(i + 1) * (m + 1) + j].max(dp[i * (m + 1) + j + 1])
            };
        }
    }
    let (mut keep_a, mut keep_b) = (vec![false; n], vec![false; m]);
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if ta[i] == tb[j] {
            keep_a[i] = true;
            keep_b[j] = true;
            i += 1;
            j += 1;
        } else if dp[(i + 1) * (m + 1) + j] >= dp[i * (m + 1) + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    // Only meaningful (non-whitespace) shared content counts towards similarity.
    let shared: usize = wa.iter().zip(&keep_a).filter(|(w, k)| **k && !a[w.0].is_whitespace()).map(|(w, _)| w.1).sum();
    let total = a.iter().filter(|c| !c.is_whitespace()).count().max(b.iter().filter(|c| !c.is_whitespace()).count());
    if total == 0 || shared * 10 < total * 4 {
        return None;
    }
    let ranges = |ws: &[(usize, usize)], keep: &[bool]| {
        let mut out: Vec<(usize, usize)> = Vec::new();
        for (w, k) in ws.iter().zip(keep) {
            if *k {
                continue;
            }
            match out.last_mut() {
                Some(last) if last.0 + last.1 == w.0 => last.1 += w.1,
                _ => out.push(*w),
            }
        }
        out
    };
    Some((ranges(&wa, &keep_a), ranges(&wb, &keep_b)))
}

/// Merges token runs and emphasis ranges into display segments.
pub fn segments(line: &str, toks: &[(usize, usize, Tok)], emph: &[(usize, usize)]) -> Vec<Seg> {
    let chars: Vec<char> = line.chars().collect();
    let mut cuts: Vec<usize> = vec![0, chars.len()];
    for t in toks {
        cuts.extend([t.0, t.0 + t.1]);
    }
    for e in emph {
        cuts.extend([e.0, e.0 + e.1]);
    }
    cuts.retain(|&c| c <= chars.len());
    cuts.sort_unstable();
    cuts.dedup();
    let tok_at = |p: usize| toks.iter().find(|t| p >= t.0 && p < t.0 + t.1).map(|t| t.2).unwrap_or(Tok::Plain);
    let emph_at = |p: usize| emph.iter().any(|e| p >= e.0 && p < e.0 + e.1);
    let mut out: Vec<Seg> = Vec::new();
    for w in cuts.windows(2) {
        let (s, e) = (w[0], w[1]);
        if s == e {
            continue;
        }
        let (tok, em) = (tok_at(s), emph_at(s));
        let text: String = chars[s..e].iter().collect();
        match out.last_mut() {
            Some(last) if last.tok == tok && last.emph == em => last.text.push_str(&text),
            _ => out.push(Seg { text, tok, emph: em }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(path: &str, lines: &[&str]) -> Vec<Vec<(String, Tok)>> {
        let mut h = Highlighter::new(lang_for(path).unwrap());
        lines
            .iter()
            .map(|l| {
                let chars: Vec<char> = l.chars().collect();
                h.line(l)
                    .into_iter()
                    .filter(|t| t.2 != Tok::Plain)
                    .map(|t| (chars[t.0..t.0 + t.1].iter().collect(), t.2))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn rust_tokens() {
        let t = toks("src/a.rs", &[r#"pub fn main() { let s = "hi // not a comment"; } // done"#]);
        let t = &t[0];
        assert!(t.contains(&("pub".into(), Tok::Keyword)));
        assert!(t.contains(&("main".into(), Tok::Func)));
        assert!(t.contains(&("\"hi // not a comment\"".into(), Tok::String)));
        assert!(t.contains(&("// done".into(), Tok::Comment)));
        let t = toks("a.rs", &["#[derive(Debug)] struct Foo<'a>(&'a str, 42u8);"]);
        assert!(t[0].contains(&("#[derive(Debug)]".into(), Tok::Attr)));
        assert!(t[0].contains(&("Foo".into(), Tok::Type)));
        assert!(t[0].contains(&("42u8".into(), Tok::Number)));
        assert!(!t[0].iter().any(|x| x.1 == Tok::String), "lifetimes aren't strings: {:?}", t[0]);
    }

    #[test]
    fn block_comments_span_lines() {
        let t = toks("x.ts", &["const a = 1; /* start", "still comment", "end */ let b = 2;"]);
        assert_eq!(t[0].last().unwrap(), &("/* start".to_string(), Tok::Comment));
        assert_eq!(t[1], vec![("still comment".to_string(), Tok::Comment)]);
        assert_eq!(t[2][0], ("end */".to_string(), Tok::Comment));
        assert!(t[2].contains(&("let".into(), Tok::Keyword)));
    }

    #[test]
    fn other_languages() {
        let py = toks("a.py", &["@dataclass", "def f(x): return None  # note"]);
        assert_eq!(py[0], vec![("@dataclass".to_string(), Tok::Attr)]);
        assert!(py[1].contains(&("# note".into(), Tok::Comment)));
        let sh = toks("run.sh", &["echo \"$HOME\" # c", "x=a#b"]);
        assert!(sh[0].contains(&("# c".into(), Tok::Comment)));
        assert!(!sh[1].iter().any(|x| x.1 == Tok::Comment), "# inside a word isn't a comment");
        let toml = toks("Cargo.toml", &["[package]", "name = \"gitust\" # c"]);
        assert_eq!(toml[0], vec![("[package]".to_string(), Tok::Type)]);
        assert_eq!(toml[1][0], ("name ".to_string(), Tok::Attr));
        let sql = toks("q.sql", &["SELECT * FROM t -- all"]);
        assert!(sql[0].contains(&("SELECT".into(), Tok::Keyword)));
        let md = toks("README.md", &["## Title"]);
        assert_eq!(md[0], vec![("## Title".to_string(), Tok::Keyword)]);
        assert!(lang_for("photo.png").is_none());
        assert!(lang_for("Dockerfile").is_some());
    }

    #[test]
    fn runs_cover_the_line_without_gaps() {
        let line = "  let x = foo(\"a\\\"b\", 3.5) /* c */ + y;";
        let mut h = Highlighter::new(lang_for("a.rs").unwrap());
        let runs = h.line(line);
        let mut pos = 0;
        for r in &runs {
            assert_eq!(r.0, pos);
            pos += r.1;
        }
        assert_eq!(pos, line.chars().count());
    }

    #[test]
    fn word_diff() {
        let (a, b) = changed_ranges("let total = price * 2;", "let total = price * 3;").unwrap();
        assert_eq!(a, vec![(20, 1)]);
        assert_eq!(b, vec![(20, 1)]);
        // Unrelated lines get no emphasis.
        assert!(changed_ranges("fn alpha() {}", "struct Beta;").is_none());
        let (_, b) = changed_ranges("call(a, b)", "call(a, b, c)").unwrap();
        assert_eq!(b, vec![(9, 3)]);
    }

    #[test]
    fn segments_merge_tokens_and_emphasis() {
        let line = "let x = 1;";
        let mut h = Highlighter::new(lang_for("a.rs").unwrap());
        let segs = segments(line, &h.line(line), &[(8, 1)]);
        let text: String = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(text, line);
        assert!(segs.iter().any(|s| s.text == "1" && s.emph && s.tok == Tok::Number));
        assert_eq!(segs[0], Seg { text: "let".into(), tok: Tok::Keyword, emph: false });
    }
}
