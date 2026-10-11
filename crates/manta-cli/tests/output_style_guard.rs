//! Parse Rust syntax and macro token trees, so comments, raw strings and nested
//! formatting cannot hide an operator-facing Debug rendering.
use proc_macro2::{TokenStream, TokenTree};
use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("test")
            || (a.path().is_ident("cfg")
                && a.parse_args::<syn::Path>()
                    .is_ok_and(|p| p.is_ident("test")))
    })
}

fn debug_spec(text: &str) -> bool {
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            continue;
        }
        if chars.peek() == Some(&'{') {
            chars.next();
            continue;
        }
        let field: String = chars.by_ref().take_while(|c| *c != '}').collect();
        if field
            .split_once(':')
            .is_some_and(|(_, spec)| spec.ends_with('?'))
        {
            return true;
        }
    }
    false
}

struct Guard<'a> {
    path: &'a str,
    hits: Vec<String>,
}

impl Guard<'_> {
    fn tokens(&mut self, name: &str, tokens: TokenStream) {
        if matches!(
            name,
            "assert"
                | "assert_eq"
                | "assert_ne"
                | "debug_assert"
                | "debug_assert_eq"
                | "debug_assert_ne"
                | "panic"
                | "unreachable"
                | "todo"
                | "unimplemented"
        ) {
            return;
        }
        let formats = matches!(
            name,
            "format"
                | "format_args"
                | "print"
                | "println"
                | "eprint"
                | "eprintln"
                | "write"
                | "writeln"
                | "anyhow"
                | "bail"
                | "ensure"
                | "error"
                | "warn"
                | "info"
                | "debug"
                | "trace"
                | "diagnostic"
        );
        let tokens: Vec<_> = tokens.into_iter().collect();
        for (i, token) in tokens.iter().enumerate() {
            match token {
                TokenTree::Literal(lit) if formats => {
                    if let Ok(lit) = syn::parse_str::<syn::LitStr>(&lit.to_string()) {
                        if debug_spec(&lit.value()) {
                            let at = token.span().start();
                            self.hits.push(format!(
                                "{}:{}:{}: Debug format in {name}!",
                                self.path,
                                at.line,
                                at.column + 1
                            ));
                        }
                    }
                }
                TokenTree::Punct(p)
                    if matches!(name, "info" | "warn" | "error" | "debug" | "trace")
                        && p.as_char() == '?' =>
                {
                    // tracing's `field = ?value` is Debug even without a format string.
                    if i == 0
                        || matches!(&tokens[i - 1], TokenTree::Punct(p) if p.as_char() == '=' || p.as_char() == ',')
                    {
                        let at = p.span().start();
                        self.hits.push(format!(
                            "{}:{}:{}: Debug tracing field",
                            self.path,
                            at.line,
                            at.column + 1
                        ));
                    }
                }
                TokenTree::Group(group) => {
                    let nested = if i >= 2
                        && matches!(&tokens[i-1], TokenTree::Punct(p) if p.as_char() == '!')
                    {
                        match &tokens[i - 2] {
                            TokenTree::Ident(id) => id.to_string(),
                            _ => String::new(),
                        }
                    } else {
                        String::new()
                    };
                    self.tokens(&nested, group.stream());
                }
                _ => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for Guard<'_> {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let attrs = match item {
            syn::Item::Const(x) => &x.attrs,
            syn::Item::Enum(x) => &x.attrs,
            syn::Item::ExternCrate(x) => &x.attrs,
            syn::Item::Fn(x) => &x.attrs,
            syn::Item::ForeignMod(x) => &x.attrs,
            syn::Item::Impl(x) => &x.attrs,
            syn::Item::Macro(x) => &x.attrs,
            syn::Item::Mod(x) => &x.attrs,
            syn::Item::Static(x) => &x.attrs,
            syn::Item::Struct(x) => &x.attrs,
            syn::Item::Trait(x) => &x.attrs,
            syn::Item::TraitAlias(x) => &x.attrs,
            syn::Item::Type(x) => &x.attrs,
            syn::Item::Union(x) => &x.attrs,
            syn::Item::Use(x) => &x.attrs,
            _ => &[] as &[syn::Attribute],
        };
        if !test_only(attrs) {
            visit::visit_item(self, item);
        }
    }
    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        let attrs = match item {
            syn::ImplItem::Const(x) => &x.attrs,
            syn::ImplItem::Fn(x) => &x.attrs,
            syn::ImplItem::Type(x) => &x.attrs,
            syn::ImplItem::Macro(x) => &x.attrs,
            _ => &[] as &[syn::Attribute],
        };
        if !test_only(attrs) {
            visit::visit_impl_item(self, item);
        }
    }
    fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
        let attrs = match item {
            syn::TraitItem::Const(x) => &x.attrs,
            syn::TraitItem::Fn(x) => &x.attrs,
            syn::TraitItem::Type(x) => &x.attrs,
            syn::TraitItem::Macro(x) => &x.attrs,
            _ => &[] as &[syn::Attribute],
        };
        if !test_only(attrs) {
            visit::visit_trait_item(self, item);
        }
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.tokens(
            &mac.path.segments.last().unwrap().ident.to_string(),
            mac.tokens.clone(),
        );
    }
    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        if attr.path().is_ident("error") {
            if let syn::Meta::List(list) = &attr.meta {
                self.tokens("error", list.tokens.clone());
            }
        }
    }
}

fn check(path: &str, source: &str) -> Vec<String> {
    let parsed = syn::parse_file(source).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut guard = Guard {
        path,
        hits: Vec::new(),
    };
    guard.visit_file(&parsed);
    guard.hits
}

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn guard_detects_nested_captured_hex_and_raw_debug_formats() {
    let location = regex::Regex::new(r"^snippet\.rs:[1-9][0-9]*:[1-9][0-9]*:").unwrap();
    for source in [
        r#"fn f() { println!("{:?}", x); }"#,
        r#"fn f() { operation().with_context(|| format!("bad {x:?}")); }"#,
        r#"fn f() { return Err(format!("bad {x:#?}")); }"#,
        r#"fn f() { x.parse().map_err(|e| format!("bad {e:?}")); }"#,
        r#"fn f() { bail!("bad {sync:02x?}"); }"#,
        "fn f() { println!(r#\"https://rx/x\n {sync:>8X?}\"#); }",
        r#"fn f() { outer!(format!("{:?}", x)); }"#,
        r#"fn f() { info!(command = ?command, "received"); }"#,
        r#"fn f() { info!(?command, "received"); }"#,
        r#"#[derive(Error)] #[error("bad {0:?}")] struct E;"#,
        r#"#[cfg(test)] mod tests { fn f() { println!("{:?}", x); } } fn prod() { println!("{:?}", x); }"#,
    ] {
        let hits = check("snippet.rs", source);
        assert_eq!(hits.len(), 1, "{source}: {hits:?}");
        assert!(location.is_match(&hits[0]), "{hits:?}");
    }
}

#[test]
fn guard_ignores_comments_literal_braces_and_real_test_items() {
    for source in [
        r#"fn f() { println!("{{sync:02x?}} https://rx/x {}", x); /* println!("{:?}", x); */ }"#,
        "// println!(\"{:?}\", x);\nfn f() { println!(r#\"https://rx/x {}\"#, x); }",
        r#"fn f() { assert_eq!(a, b, "{:?}", x); panic!("{:?}", x); }"#,
        r#"mod m { #[cfg(test)] fn f() { println!("{:?}", x); } fn prod() { println!("{}", x); } }"#,
        r#"#[cfg(test)] mod tests { fn f() { println!("{:?}", x); } }"#,
        r#"struct X; impl X { #[cfg(test)] fn f() { println!("{:?}", x); } }"#,
        r#"fn f() { serde_json::to_string(&x); toml::to_string(&x); }"#,
    ] {
        assert!(check("negative.rs", source).is_empty(), "{source}");
    }
}

#[test]
fn shipped_sources_never_use_debug_output() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(root.join("crates")).unwrap() {
        let src = entry.unwrap().path().join("src");
        if src.is_dir() {
            sources(&src, &mut files);
        }
    }
    files.sort();
    for expected in [
        "manta-cli/src/main.rs",
        "manta-cli/src/config_cmd.rs",
        "manta-input/src/lib.rs",
        "manta-input/src/hpsdr.rs",
        "manta-input/src/soapy.rs",
        "manta-server/src/lib.rs",
        "manta-server/src/telnet.rs",
        "manta-testkit/src/oracle.rs",
    ] {
        assert!(
            files.iter().any(|p| p.ends_with(expected)),
            "source discovery missed {expected}"
        );
    }
    let mut hits = Vec::new();
    for path in files {
        // Separate internal soak executable, never linked into the shipped manta command.
        if path.ends_with("manta-soak-harness/src/main.rs") {
            continue;
        }
        hits.extend(check(
            &path.display().to_string(),
            &std::fs::read_to_string(&path).unwrap(),
        ));
    }
    assert!(hits.is_empty(), "{}", hits.join("\n"));
}
