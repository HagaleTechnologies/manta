//! MAN-548: `manta-cli`'s module layout. `main.rs` holds the clap tree and
//! dispatch; each module split out of it owns its items, and unit tests live
//! in a sibling `tests.rs` file instead of an inline `mod tests { .. }`.
//! A sibling module names the crate root only for what `main.rs` itself
//! declares, never for a name `main.rs` merely imports.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use proc_macro2::{TokenStream, TokenTree};
use syn::visit::{self, Visit};

fn src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn parse(path: &Path) -> syn::File {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    syn::parse_file(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The `mod`s `main.rs` declares and the items it defines (not imports).
fn root_names() -> BTreeSet<String> {
    parse(&src().join("main.rs"))
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Mod(x) => Some(x.ident.to_string()),
            syn::Item::Fn(x) => Some(x.sig.ident.to_string()),
            syn::Item::Struct(x) => Some(x.ident.to_string()),
            syn::Item::Enum(x) => Some(x.ident.to_string()),
            syn::Item::Const(x) => Some(x.ident.to_string()),
            syn::Item::Static(x) => Some(x.ident.to_string()),
            syn::Item::Type(x) => Some(x.ident.to_string()),
            syn::Item::Trait(x) => Some(x.ident.to_string()),
            syn::Item::Macro(x) => x.ident.as_ref().map(|i| i.to_string()),
            _ => None,
        })
        .collect()
}

/// Every name that follows a leading `crate::`, with its line.
#[derive(Default)]
struct CrateRefs(Vec<(String, usize)>);

impl CrateRefs {
    fn use_tree(&mut self, tree: &syn::UseTree) {
        let (name, span) = match tree {
            syn::UseTree::Path(x) => (x.ident.to_string(), x.ident.span()),
            syn::UseTree::Name(x) => (x.ident.to_string(), x.ident.span()),
            syn::UseTree::Rename(x) => (x.ident.to_string(), x.ident.span()),
            syn::UseTree::Glob(x) => ("*".to_string(), x.star_token.span),
            syn::UseTree::Group(x) => {
                x.items.iter().for_each(|t| self.use_tree(t));
                return;
            }
        };
        self.0.push((name, span.start().line));
    }
    fn tokens(&mut self, tokens: TokenStream) {
        let tokens: Vec<TokenTree> = tokens.into_iter().collect();
        for (i, t) in tokens.iter().enumerate() {
            match t {
                TokenTree::Group(g) => self.tokens(g.stream()),
                TokenTree::Ident(id) if id == "crate" => {
                    if let (
                        Some(TokenTree::Punct(a)),
                        Some(TokenTree::Punct(b)),
                        Some(TokenTree::Ident(name)),
                    ) = (tokens.get(i + 1), tokens.get(i + 2), tokens.get(i + 3))
                    {
                        if a.as_char() == ':' && b.as_char() == ':' {
                            self.0.push((name.to_string(), name.span().start().line));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for CrateRefs {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        let mut segments = path.segments.iter();
        if let (Some(first), Some(second)) = (segments.next(), segments.next()) {
            if first.ident == "crate" {
                self.0
                    .push((second.ident.to_string(), second.ident.span().start().line));
            }
        }
        visit::visit_path(self, path);
    }
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        if let syn::UseTree::Path(p) = &item.tree {
            if p.ident == "crate" {
                self.use_tree(&p.tree);
            }
        }
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.tokens(mac.tokens.clone());
    }
}

#[test]
fn sibling_modules_reach_the_crate_root_only_for_what_main_rs_defines() {
    let root = root_names();
    let mut files = Vec::new();
    rust_files(&src(), &mut files);
    files.sort();
    let mut hits = Vec::new();
    for path in files.iter().filter(|p| !p.ends_with("src/main.rs")) {
        let mut refs = CrateRefs::default();
        refs.visit_file(&parse(path));
        for (name, line) in refs.0 {
            if name != "self" && !root.contains(&name) {
                hits.push(format!("{}:{line}: crate::{name}", path.display()));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "these name an item main.rs only imports; name it by its own module instead:\n{}",
        hits.join("\n")
    );
}

#[test]
fn main_rs_keeps_its_unit_tests_in_src_tests_rs() {
    let main = parse(&src().join("main.rs"));
    let tests = main
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Mod(m) if m.ident == "tests" => Some(m),
            _ => None,
        })
        .expect("main.rs declares `mod tests`");
    assert!(
        tests.content.is_none(),
        "main.rs's `mod tests` is inline; move it to src/tests.rs"
    );
    assert!(src().join("tests.rs").is_file(), "src/tests.rs is missing");
}
