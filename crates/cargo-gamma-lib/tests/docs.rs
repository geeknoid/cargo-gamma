//! Keeps the generated reference tables in `docs/` honest.
//!
//! The operator catalog is the tool's published vocabulary: the same names appear on `--ops`, in
//! every suppression directive, in the report, in SARIF rule identifiers and in configuration. A
//! reference that has drifted is worse than none, because a reader who copies a name out of it
//! gets a usage error with nothing to suggest the document was at fault.
//!
//! Run with `GAMMA_BLESS_DOCS=1` to rewrite the files instead of failing.

use camino::Utf8PathBuf;
use cargo_gamma_lib::docs;
use std::fs;

/// The documentation files carrying generated blocks.
const FILES: &[&str] = &["OPERATORS.md", "PROFILES.md"];

/// Returns the repository's `docs` directory.
fn docs_dir() -> Utf8PathBuf {
    Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs")
}

/// Rewrites every generated block in `text`, returning the result.
///
/// A block is delimited rather than owning the file so that the prose explaining what a family is
/// *for* can live beside the table listing what it contains. A reference that is only a table says
/// what exists without saying when to reach for it.
fn regenerate(text: &str, path: &str) -> String {
    let mut out = String::new();
    let mut rest = text;

    while let Some((before, after_marker)) = rest.split_once(docs::BEGIN) {
        out.push_str(before);

        assert!(
            after_marker.contains(" -->"),
            "{path}: a `{}` marker is never terminated",
            docs::BEGIN
        );
        let (name, after_name) = after_marker.split_once(" -->").expect("the marker terminator is present");

        assert!(docs::block(name).is_some(), "{path}: there is no generated block named `{name}`");
        let body = docs::block(name).expect("the block name is known");

        assert!(
            after_name.contains(docs::END),
            "{path}: block `{name}` is never closed with `{}`",
            docs::END
        );
        let (_, after_end) = after_name.split_once(docs::END).expect("the closing marker is present");

        out.push_str(docs::BEGIN);
        out.push_str(name);
        out.push_str(" -->\n\n");
        out.push_str(&body);
        out.push_str("\n\n");
        out.push_str(docs::END);

        rest = after_end;
    }

    out.push_str(rest);
    out
}

#[test]
fn the_generated_reference_tables_match_the_registry() {
    let dir = docs_dir();
    let blessing = std::env::var_os("GAMMA_BLESS_DOCS").is_some();
    let mut stale = Vec::new();

    for name in FILES {
        let path = dir.join(name);
        let text = fs::read_to_string(path.as_std_path()).unwrap_or_else(|_| panic!("could not read {path}"));
        let expected = regenerate(&text, name);

        if text == expected {
            continue;
        }

        if blessing {
            fs::write(path.as_std_path(), &expected).unwrap_or_else(|_| panic!("could not write {path}"));
        } else {
            stale.push((*name).to_owned());
        }
    }

    assert!(
        stale.is_empty(),
        "{} is out of date with the mutator registry. Run `GAMMA_BLESS_DOCS=1 cargo test --all-features --test docs` to regenerate.",
        stale.join(", ")
    );
}

#[test]
fn every_documentation_file_the_readme_points_at_exists() {
    // A reference split out of the README is only useful if the link works. A broken one sends a
    // reader looking for the catalog to a 404 on the crate's own front page.
    let root = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let readme = fs::read_to_string(root.join("README.md").as_std_path()).expect("could not read README.md");

    for name in FILES.iter().copied().chain(["CONFIGURATION.md", "SUPPRESSION.md", "DESIGN.md"]) {
        let link = format!("docs/{name}");

        assert!(readme.contains(&link), "README.md never links to {link}");
        assert!(docs_dir().join(name).as_std_path().exists(), "docs/{name} is linked but missing");
    }
}

/// Kebab-cases a Rust field name the way serde's `rename_all` does.
fn kebab(name: &str) -> String {
    name.replace('_', "-")
}

/// Returns the field names declared in one `pub struct` in `config.rs`.
///
/// The struct is found by name and read to its closing brace. Parsing the source is crude, but it
/// is the only source of truth available: serde's `deny_unknown_fields` is generated at compile
/// time and leaves no runtime list of accepted keys behind to compare against.
fn fields(source: &str, name: &str) -> Vec<String> {
    let header = format!("pub struct {name} {{");

    assert!(source.contains(&header), "config.rs declares no struct named {name}");
    let (_, body) = source.split_once(header.as_str()).expect("the struct header is present");

    body.lines()
        .take_while(|line| !line.starts_with('}'))
        .filter_map(|line| line.strip_prefix("    pub "))
        .filter_map(|rest| rest.split(':').next())
        .map(kebab)
        .collect()
}

#[test]
fn every_configuration_key_is_documented() {
    // An undocumented key is a key nobody can use: `deny_unknown_fields` means a reader cannot
    // discover one by guessing, and there is no `--help` for a configuration file. Adding a field
    // without adding a row is therefore a silent feature.
    let source = include_str!("../src/config.rs");
    let path = docs_dir().join("CONFIGURATION.md");
    let text = fs::read_to_string(path.as_std_path()).expect("could not read docs/CONFIGURATION.md");

    let keys = ["Config", "Shard", "Reporters"].iter().flat_map(|name| fields(source, name));

    for key in keys {
        // A nested table is documented by its heading rather than as a row of its parent's table,
        // so `[shard]` counts as documenting the `shard` key.
        assert!(
            text.contains(&format!("`{key}`")) || text.contains(&format!("[{key}]")),
            "docs/CONFIGURATION.md never mentions the `{key}` key"
        );
    }
}
