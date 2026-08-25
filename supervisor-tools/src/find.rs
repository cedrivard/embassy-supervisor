//! Finding and parsing supervisor graph declarations in Rust source files.

use embassy_supervisor_syntax::{GraphSpec, normalize_fragment_crate, substitute_dollar_crate};
use proc_macro2::{TokenStream, TokenTree};
use quote::quote;
use std::collections::BTreeMap;
use std::str::FromStr;

/// Placeholder crate name used while normalising fragment paths.
pub const UNRESOLVED_CRATE: &str = "__sv_unresolved_crate";

/// Which of the three supervisor declaration macros produced a declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclKind {
    /// A complete `supervisor_graph!` declaration.
    Graph,
    /// A `supervisor_fragment!` declaration.
    Fragment,
    /// A `compose_graph!` declaration.
    Compose,
}

impl DeclKind {
    /// Return the macro name as written in source, including the trailing `!`.
    pub fn macro_name(self) -> &'static str {
        match self {
            DeclKind::Graph => "supervisor_graph!",
            DeclKind::Fragment => "supervisor_fragment!",
            DeclKind::Compose => "compose_graph!",
        }
    }
}

/// One declaration site, with its items already parsed.
pub struct Decl {
    /// Which macro introduced this declaration.
    pub kind: DeclKind,
    /// The parsed graph specification.
    pub spec: GraphSpec,
    /// The declaration's items as written, still holding `$crate`.
    ///
    /// This is kept so that fragments can be re-resolved at a compose site.
    pub body: TokenStream,
    /// Fragment names named by a `compose_graph!` declaration.
    pub fragments: Vec<String>,
    /// Map from fragment name to the file that declared it.
    pub fragment_origins: BTreeMap<String, String>,
    /// Source file path containing the declaration.
    pub origin: String,
    /// Line number of the macro invocation.
    pub line: usize,
}

impl Decl {
    /// Return the graph name, if the declaration has a `name:` clause.
    pub fn name(&self) -> Option<String> {
        self.spec.name.as_ref().map(|i| i.to_string())
    }
}

/// Result type alias for declaration parsing.
pub type Result<T> = std::result::Result<T, Error>;

/// An error encountered while scanning or parsing a declaration.
#[derive(Debug)]
pub struct Error {
    /// Source file path where the error occurred.
    pub file: String,
    /// Line number where the error occurred.
    pub line: usize,
    /// Human-readable error message.
    pub message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.file, self.line, self.message)
    }
}

impl std::error::Error for Error {}

const MACRO_NAMES: &[(&str, DeclKind)] = &[
    ("supervisor_graph", DeclKind::Graph),
    ("supervisor_fragment", DeclKind::Fragment),
    ("compose_graph", DeclKind::Compose),
];

/// Parse every supervisor declaration in one source file.
///
/// This descends into module blocks so declarations inside `mod` bodies are
/// found too. It ignores `macro_rules!` relays by detecting `$` metavariables.
pub fn parse_source(src: &str, file: &str) -> Result<Vec<Decl>> {
    let stream = TokenStream::from_str(src).map_err(|e| Error {
        file: file.to_string(),
        line: e.span().start().line,
        message: format!("could not tokenize the file: {e}"),
    })?;
    let mut out = Vec::new();
    walk(stream, file, &mut out)?;
    Ok(out)
}

/// Scan for `NAME ! { .. }`, descending into every group so a declaration
/// inside a module block is found too.
fn walk(stream: TokenStream, file: &str, out: &mut Vec<Decl>) -> Result<()> {
    let tt: Vec<TokenTree> = stream.into_iter().collect();
    let mut i = 0;
    while i < tt.len() {
        if let TokenTree::Ident(id) = &tt[i]
            && let Some((_, kind)) = MACRO_NAMES.iter().find(|(n, _)| id == n)
            && matches!(tt.get(i + 1), Some(TokenTree::Punct(p)) if p.as_char() == '!')
            && let Some(TokenTree::Group(body)) = tt.get(i + 2)
        {
            let inner = body.stream();
            // A `macro_rules!` arm forwarding to one of these macros is not a
            // declaration: its body opens with an internal `@rule` marker, or
            // carries `$` metavariables (`$($acc)*`, `$g`). `$crate` alone is
            // NOT a relay tell — fragment declarations may spell their own
            // paths with it.
            let internal = matches!(inner.clone().into_iter().next(), Some(TokenTree::Punct(p)) if p.as_char() == '@')
                || has_metavariable(inner.clone());
            if !internal {
                let line = id.span().start().line;
                out.push(parse_decl(*kind, inner, file, line)?);
                i += 3;
                continue;
            }
        }
        if let TokenTree::Group(g) = &tt[i] {
            walk(g.stream(), file, out)?;
        }
        i += 1;
    }
    Ok(())
}

/// Does the stream carry, at any depth, a `$` followed by anything other than
/// the ident `crate`? That is a `macro_rules!` metavariable — a relay body —
/// while `$crate` is an ordinary path prefix a declaration may use.
fn has_metavariable(stream: TokenStream) -> bool {
    let tt: Vec<TokenTree> = stream.into_iter().collect();
    for (i, t) in tt.iter().enumerate() {
        match t {
            TokenTree::Punct(p) if p.as_char() == '$' => match tt.get(i + 1) {
                Some(TokenTree::Ident(id)) if id == "crate" => {}
                _ => return true,
            },
            TokenTree::Group(g) if has_metavariable(g.stream()) => return true,
            _ => {}
        }
    }
    false
}

/// Parse a declaration's item body into a [`GraphSpec`].
///
/// `items` must have had `$crate` already substituted. `kind` and `err` are
/// used to produce accurate diagnostics.
pub fn parse_items(
    items: TokenStream,
    kind: DeclKind,
    err: &dyn Fn(String) -> Error,
) -> Result<GraphSpec> {
    syn::parse2(items).map_err(|e| err(format!("{}: {e}", kind.macro_name().trim_end_matches('!'))))
}

fn parse_decl(kind: DeclKind, body: TokenStream, file: &str, line: usize) -> Result<Decl> {
    let err = |message: String| Error {
        file: file.to_string(),
        line,
        message,
    };
    let (items, fragments) = match kind {
        DeclKind::Compose => split_compose(body.clone(), &err)?,
        DeclKind::Fragment => (
            substitute_dollar_crate(
                normalize_fragment_crate(body.clone()),
                &placeholder(UNRESOLVED_CRATE),
            ),
            Vec::new(),
        ),
        DeclKind::Graph => (
            substitute_dollar_crate(body.clone(), &placeholder(UNRESOLVED_CRATE)),
            Vec::new(),
        ),
    };
    let spec = parse_items(items.clone(), kind, &err)?;
    Ok(Decl {
        kind,
        spec,
        body,
        fragments,
        fragment_origins: BTreeMap::new(),
        origin: file.to_string(),
        line,
    })
}

/// Create a single-ident token stream from `name`.
pub fn placeholder(name: &str) -> TokenStream {
    let id = proc_macro2::Ident::new(name, proc_macro2::Span::call_site());
    quote!(#id)
}

fn split_compose(
    body: TokenStream,
    err: &dyn Fn(String) -> Error,
) -> Result<(TokenStream, Vec<String>)> {
    let tt: Vec<TokenTree> = body.into_iter().collect();
    let mut items = TokenStream::new();
    let mut fragments = Vec::new();
    let mut name = TokenStream::new();
    for clause in split_top(&tt, ',') {
        let Some(TokenTree::Ident(key)) = clause.first() else {
            continue;
        };
        let Some(value) = clause.get(2..) else {
            return Err(err(format!(
                "`compose_graph!` clause `{key}` is missing its `:` and value"
            )));
        };
        match key.to_string().as_str() {
            "name" => {
                let v: TokenStream = value.iter().cloned().collect();
                name = quote!(name: #v;);
            }
            "fragments" => {
                let Some(TokenTree::Group(g)) = value.first() else {
                    return Err(err("expected a list after `fragments:`".into()));
                };
                fragments = split_top(&g.stream().into_iter().collect::<Vec<_>>(), ',')
                    .into_iter()
                    .filter(|e| !e.is_empty())
                    .map(|e| e.iter().map(|t| t.to_string()).collect::<String>())
                    .collect();
            }
            "graph" => {
                let Some(TokenTree::Group(g)) = value.first() else {
                    return Err(err("expected a block after `graph:`".into()));
                };
                items = g.stream();
            }
            other => {
                return Err(err(format!(
                    "unknown `compose_graph!` clause `{other}:`; it takes \
                     `name:`, `fragments:` and `graph:`"
                )));
            }
        }
    }
    Ok((quote!(#name #items), fragments))
}

fn split_top(tt: &[TokenTree], sep: char) -> Vec<Vec<TokenTree>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    let mut angle = 0i32;
    for t in tt {
        if let TokenTree::Punct(p) = t {
            match p.as_char() {
                '<' => angle += 1,
                '>' if angle > 0 => angle -= 1,
                c if c == sep && angle == 0 => {
                    out.push(std::mem::take(&mut cur));
                    continue;
                }
                _ => {}
            }
        }
        cur.push(t.clone());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
