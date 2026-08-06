use std::io::Write;

use camino::Utf8PathBuf;
use serde_json::Value;

use crate::error::error;
use crate::ops::registry;

use super::cli::{ListArgs, ListKind};
use super::dispatch::EXIT_OK;
use super::host::Host;

/// Implements `list`.
pub(super) fn list<H: Host>(host: &mut H, args: &ListArgs) -> crate::Result<i32> {
    match args.what {
        ListKind::Ops => list_ops(host, args),
        ListKind::Files => list_files(host, args),
        ListKind::Mutants => list_mutants(host, args),
    }
}

/// Lists the mutator registry.
fn list_ops<H: Host>(host: &mut H, args: &ListArgs) -> crate::Result<i32> {
    let selection = args.select.selection()?;
    let mut stream = host.output();

    if args.json {
        let entries: Vec<Value> = registry::REGISTRY
            .iter()
            .map(|mutator| {
                serde_json::json!({
                    "name": mutator.name,
                    "description": mutator.description,
                    "default": mutator.default_on,
                    "enabled": selection.contains(mutator.name),
                    "aliases": mutator.aliases,
                })
            })
            .collect();

        writeln!(stream, "{}", serde_json::to_string_pretty(&entries).map_err(|cause| {
            error!("could not serialize the mutator registry").caused_by(cause)
        })?)?;

        return Ok(EXIT_OK);
    }

    let width = registry::REGISTRY.iter().map(|m| m.name.len()).max().unwrap_or(0);

    for mutator in registry::REGISTRY {
        let mark = if selection.contains(mutator.name) { "*" } else { " " };

        writeln!(stream, "{mark} {:width$}  {}", mutator.name, mutator.description)?;
    }

    writeln!(stream)?;
    writeln!(stream, "* = enabled by the current selection")?;

    Ok(EXIT_OK)
}

/// Lists the files that would be analyzed.
fn list_files<H: Host>(host: &mut H, args: &ListArgs) -> crate::Result<i32> {
    let selection = args.select.selection()?;
    let plan = crate::discover::plan(&args.select, &selection, args.select.shard()?, &mut |_| {})?;
    let mut stream = host.output();

    if args.json {
        let paths: Vec<&Utf8PathBuf> = plan.files.iter().map(|file| &file.path).collect();

        writeln!(
            stream,
            "{}",
            serde_json::to_string_pretty(&paths)
                .map_err(|cause| error!("could not serialize the file list").caused_by(cause))?
        )?;

        return Ok(EXIT_OK);
    }

    for file in &plan.files {
        writeln!(stream, "{}", file.path)?;
    }

    Ok(EXIT_OK)
}

/// Lists the mutants that would be generated.
fn list_mutants<H: Host>(host: &mut H, args: &ListArgs) -> crate::Result<i32> {
    let selection = args.select.selection()?;
    let shard = args.select.shard()?;
    let plan = crate::discover::plan(&args.select, &selection, shard, &mut |_| {})?;

    if let Some(path) = args.json_report.as_ref() {
        write_population(host, &plan, shard, path)?;
    }

    let mut stream = host.output();

    if args.json {
        writeln!(
            stream,
            "{}",
            serde_json::to_string_pretty(&plan.mutants)
                .map_err(|cause| error!("could not serialize the mutant list").caused_by(cause))?
        )?;

        return Ok(EXIT_OK);
    }

    for mutant in &plan.mutants {
        writeln!(stream, "{}", mutant.describe())?;
    }

    Ok(EXIT_OK)
}

/// Writes the listing as a report document.
///
/// `merge` withdraws a mutant only when a newer unsharded input states the whole population of its
/// file, and producing that from a run means paying for a run. Listing is the cheap way to say what
/// exists now, so it is the one a nightly rotation can afford beside its shard.
fn write_population<H: Host>(
    host: &mut H,
    plan: &crate::discover::Plan,
    shard: Option<(u32, u32)>,
    path: &Utf8PathBuf,
) -> crate::Result<()> {
    let info = crate::elements::RunInfo {
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs()),
        shard: shard.map(|(count, index)| crate::elements::ShardInfo { index, count }),
    };

    let report = crate::elements::build(plan, crate::elements::Thresholds::default(), Some(info))?;

    crate::elements::write(path, &crate::elements::to_json(&report)?)?;
    writeln!(host.error(), "Wrote {path}")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{BrokenHost, Sink, workdir};

    fn crate_dir(name: &str) -> tempfile::TempDir {
        let dir = workdir(name);
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");

        std::fs::create_dir(root.join("src")).expect("src");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"subject\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
        )
        .expect("manifest");
        std::fs::write(root.join("src/lib.rs"), "pub fn less(a: i32, b: i32) -> bool { a < b }\n").expect("lib");

        dir
    }

    fn args(dir: Utf8PathBuf, what: ListKind, json: bool) -> ListArgs {
        ListArgs {
            what,
            select: crate::commands::SelectArgs {
                dir,
                ..crate::commands::SelectArgs::default()
            },
            json,
            json_report: None,
        }
    }

    #[test]
    fn ops_can_be_listed_as_json() {
        let mut host = Sink::default();

        let code = list(&mut host, &args(Utf8PathBuf::from("."), ListKind::Ops, true)).expect("list");
        let text = String::from_utf8(host.out).expect("utf-8");
        let value: Value = serde_json::from_str(&text).expect("json");

        assert_eq!(code, EXIT_OK);
        assert!(value.as_array().is_some_and(|entries| !entries.is_empty()), "{text}");
        assert!(text.contains("\"enabled\""), "{text}");
    }

    #[test]
    fn files_can_be_listed_as_json() {
        let dir = crate_dir("list-files-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut host = Sink::default();

        let code = list(&mut host, &args(root, ListKind::Files, true)).expect("list files");
        let text = String::from_utf8(host.out).expect("utf-8");

        assert_eq!(code, EXIT_OK);
        assert!(text.contains("src/lib.rs"), "{text}");
    }

    #[test]
    fn mutants_can_be_listed_as_json() {
        let dir = crate_dir("list-mutants-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut host = Sink::default();

        let code = list(&mut host, &args(root, ListKind::Mutants, true)).expect("list mutants");
        let text = String::from_utf8(host.out).expect("utf-8");

        assert_eq!(code, EXIT_OK);
        assert!(text.contains("relational.lt_to_le"), "{text}");
    }

    /// The plain listing marks the mutators the current selection turns on.
    #[test]
    fn ops_can_be_listed_as_text_with_the_selection_marked() {
        let mut host = Sink::default();

        let code = list(&mut host, &args(Utf8PathBuf::from("."), ListKind::Ops, false)).expect("list");

        assert_eq!(code, EXIT_OK);
        assert!(host.out().contains("* = enabled by the current selection"), "{}", host.out());
        assert!(host.out().lines().any(|line| line.starts_with("* ")), "{}", host.out());
    }

    /// The plain file listing is one path per line, so it can be piped into `xargs`.
    #[test]
    fn files_can_be_listed_as_text() {
        let dir = crate_dir("list-files-text-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut host = Sink::default();

        let code = list(&mut host, &args(root, ListKind::Files, false)).expect("list files");

        assert_eq!(code, EXIT_OK);
        assert_eq!(host.out().trim(), "src/lib.rs");
    }

    /// The plain mutant listing describes each mutant on its own line.
    #[test]
    fn mutants_can_be_listed_as_text() {
        let dir = crate_dir("list-mutants-text-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let mut host = Sink::default();

        let code = list(&mut host, &args(root, ListKind::Mutants, false)).expect("list mutants");

        assert_eq!(code, EXIT_OK);
        assert!(host.out().contains("relational.lt_to_le"), "{}", host.out());
    }

    /// Every listing shape has to surface a closed pipe rather than stop half-written.
    #[test]
    fn a_closed_output_stream_is_reported_for_every_listing() {
        let dir = crate_dir("list-broken-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");

        for (what, json) in [
            (ListKind::Ops, true),
            (ListKind::Ops, false),
            (ListKind::Files, true),
            (ListKind::Files, false),
            (ListKind::Mutants, true),
            (ListKind::Mutants, false),
        ] {
            let error = list(&mut BrokenHost, &args(root.clone(), what, json)).expect_err("closed pipe");

            assert!(error.to_string().contains("broken pipe"), "{what:?} json={json}: {error}");
        }
    }

    #[test]
    fn the_population_can_be_written_as_a_report() {
        // `merge` withdraws a retired mutant only against an unsharded population, and a rotation
        // that could afford a full run would not be sharding in the first place.
        let dir = crate_dir("list-population-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("population.json");
        let mut host = Sink::default();
        let mut listing = args(root, ListKind::Mutants, false);

        listing.json_report = Some(path.clone());

        let code = list(&mut host, &listing).expect("list");
        let text = std::fs::read_to_string(&path).expect("report");
        let report: crate::elements::Report = serde_json::from_str(&text).expect("json");

        assert_eq!(code, EXIT_OK);
        assert!(report.config.as_ref().is_some_and(|run| run.shard.is_none()), "{text}");
        assert!(
            report.files.values().any(|file| !file.mutants.is_empty()),
            "the population is empty: {text}"
        );
        assert!(String::from_utf8(host.err).expect("utf-8").contains("Wrote"), "the path was not echoed");
    }

    #[test]
    fn a_sharded_population_says_which_shard_it_is() {
        // A shard's silence about a mutant is not evidence that the mutant is gone, so the merge
        // has to be able to tell the two kinds of listing apart.
        let dir = crate_dir("list-population-shard-");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let path = root.join("population.json");
        let mut host = Sink::default();
        let mut listing = args(root, ListKind::Mutants, false);

        listing.json_report = Some(path.clone());
        listing.select.shard_count = Some(4);
        listing.select.shard_index = Some(2);

        let code = list(&mut host, &listing).expect("list");
        let text = std::fs::read_to_string(&path).expect("report");
        let report: crate::elements::Report = serde_json::from_str(&text).expect("json");
        let shard = report.config.as_ref().and_then(|run| run.shard.as_ref()).expect("shard");

        assert_eq!(code, EXIT_OK);
        assert_eq!((shard.index, shard.count), (2, 4));
    }
}
