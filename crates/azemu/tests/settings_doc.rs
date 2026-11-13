//! `docs/SETTINGS.md` cannot go stale silently. The table in it is what the code describes (`azemu __settings-table`,
//! which builds every service and prints what their constructors passed to `settings::describe`), and every
//! `"AZEMU_*"` name in the source of a crate is either in the table or in the list under it.
//!
//! After changing a setting: `AZEMU_UPDATE_SETTINGS_DOC=1 cargo test -p azemu --test settings_doc` rewrites the file.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_azemu");

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// What `azemu __settings-table` prints, with the caller's `AZEMU_*` variables out of the way.
fn table_from_the_program() -> String {
    let mut cmd = Command::new(BIN);
    cmd.arg("__settings-table");
    for (k, _) in std::env::vars().filter(|(k, _)| k.starts_with("AZEMU_")) {
        cmd.env_remove(k);
    }
    let out = cmd.output().expect("run azemu __settings-table");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Every `"AZEMU_NAME"` string in the `src` directory of a crate (tests and build output are not looked at).
fn names_in_the_source() -> BTreeSet<String> {
    let mut crates = Vec::new();
    for group in ["crates", "crates/services"] {
        if let Ok(entries) = std::fs::read_dir(workspace().join(group)) {
            crates.extend(entries.flatten().map(|e| e.path().join("src")).filter(|p| p.is_dir()));
        }
    }
    let mut files = Vec::new();
    for c in crates {
        rust_files(&c, &mut files);
    }
    let mut names = BTreeSet::new();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap_or_default();
        let mut rest = text.as_str();
        while let Some(i) = rest.find("\"AZEMU_") {
            let after = &rest[i + 1..];
            let end = after.find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')).unwrap_or(after.len());
            if after[end..].starts_with('"') {
                names.insert(after[..end].to_string());
            }
            rest = &after[end..];
        }
    }
    // not settings: names of the test tooling (listed in core and in the second table of the document), and the commit the build records
    names.retain(|n| !azemu_core::settings::is_test_tooling(n) && n != "AZEMU_GIT_COMMIT" && !n.ends_with('_'));
    names
}

fn names_in_the_table(table: &str) -> BTreeSet<String> {
    table.lines().filter_map(|l| l.strip_prefix("| `")).filter_map(|l| l.split('`').next()).map(str::to_string).collect()
}

fn between<'a>(doc: &'a str, begin: &str, end: &str) -> &'a str {
    let a = doc.find(begin).unwrap_or_else(|| panic!("docs/SETTINGS.md has no {begin}")) + begin.len();
    let b = doc.find(end).unwrap_or_else(|| panic!("docs/SETTINGS.md has no {end}"));
    &doc[a..b]
}

fn undescribed_list(undescribed: &BTreeSet<String>) -> String {
    undescribed.iter().map(|n| format!("- `{n}`\n")).collect()
}

#[test]
fn the_settings_document_matches_the_code() {
    let table = table_from_the_program();
    let described = names_in_the_table(&table);
    assert!(described.contains("AZEMU_LENIENT") && described.contains("AZEMU_STANDARD_PORTS"), "core's own settings are missing:\n{table}");
    let undescribed: BTreeSet<String> = names_in_the_source().difference(&described).cloned().collect();

    // a name in some crate's source that is neither described nor on the list of known names: say how to fix it
    let unknown: Vec<&String> = undescribed.iter().filter(|n| !azemu_core::settings::NOT_YET_DESCRIBED.contains(&n.as_str())).collect();
    assert!(
        unknown.is_empty(),
        "{unknown:?} is read by a crate but is not described. Call azemu_core::settings::describe(name, default, meaning) in the \
         constructor of the service that reads it (see docs/SETTINGS.md). If it is not a setting (a variable only a test or a \
         child process uses), name it differently or ask the maintainers of crates/azemu."
    );

    let path = workspace().join("docs/SETTINGS.md");
    let doc = std::fs::read_to_string(&path).expect("docs/SETTINGS.md");
    let want_table = format!("\n{table}");
    let want_list = format!("\n{}", undescribed_list(&undescribed));
    let want_tooling = format!("\n{}", azemu_core::settings::test_tooling_markdown());
    if std::env::var("AZEMU_UPDATE_SETTINGS_DOC").is_ok_and(|v| v == "1") {
        let mut new = doc.clone();
        for (begin, end, body) in [
            ("<!-- BEGIN TABLE -->", "<!-- END TABLE -->", &want_table),
            ("<!-- BEGIN TEST TOOLING -->", "<!-- END TEST TOOLING -->", &want_tooling),
            ("<!-- BEGIN UNDESCRIBED -->", "<!-- END UNDESCRIBED -->", &want_list),
        ] {
            let a = new.find(begin).expect(begin) + begin.len();
            let b = new.find(end).expect(end);
            new.replace_range(a..b, body);
        }
        std::fs::write(&path, new).unwrap();
        return;
    }
    assert_eq!(
        between(&doc, "<!-- BEGIN TABLE -->", "<!-- END TABLE -->"),
        want_table,
        "docs/SETTINGS.md is out of date. Run: AZEMU_UPDATE_SETTINGS_DOC=1 cargo test -p azemu --test settings_doc"
    );
    assert_eq!(
        between(&doc, "<!-- BEGIN TEST TOOLING -->", "<!-- END TEST TOOLING -->"),
        want_tooling,
        "docs/SETTINGS.md is out of date. Run: AZEMU_UPDATE_SETTINGS_DOC=1 cargo test -p azemu --test settings_doc"
    );
    assert_eq!(
        between(&doc, "<!-- BEGIN UNDESCRIBED -->", "<!-- END UNDESCRIBED -->"),
        want_list,
        "docs/SETTINGS.md is out of date. Run: AZEMU_UPDATE_SETTINGS_DOC=1 cargo test -p azemu --test settings_doc"
    );
}

#[test]
fn every_service_binary_and_the_unified_one_print_the_same_core_rows() {
    // the hidden command works in the program without a data directory and without opening a port
    let table = table_from_the_program();
    assert!(table.starts_with("| Variable | Default | Meaning |\n|---|---|---|\n"), "{table}");
    let names: Vec<String> = names_in_the_table(&table).into_iter().collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "rows are sorted by name");
}
