//! The documentation of the settings stays true. The table at the top of
//! `src/config.rs` lists every setting the server reads. Each one is
//! described in the settings tables of the three READMEs and in
//! `nuage.env.example`; none of them, nor `app.json`, names a setting the
//! server does not read; and the three READMEs keep the same outline.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// The READMEs and the heading of their settings section.
const READMES: [(&str, &str); 3] = [
    ("README.md", "## Environment variables"),
    ("README.fr.md", "## Variables d'environnement"),
    ("README.es.md", "## Variables de entorno"),
];

/// Read by the website (crates/nuage-site), not by `src/config.rs`.
const READ_BY_THE_SITE: &str = "SITE_URL";

/// Names a settings file itself: the example file does not list it.
const NOT_IN_A_FILE: &str = "NUAGE_ENV_FILE";

/// A file of the repository.
fn read(path: &str) -> String {
    let file = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(path);
    std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("cannot read {}: {e}", file.display()))
}

/// Written like the name of a setting: capital letters, digits and `_`.
fn is_name(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_uppercase())
        && s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// The names between backticks in `cells`.
fn names(cells: &[String]) -> BTreeSet<String> {
    cells.iter().flat_map(|cell| cell.split('`').skip(1).step_by(2)).filter(|s| is_name(s)).map(str::to_owned).collect()
}

/// The first cell of each row of the Markdown tables in `lines` whose first
/// cell is code (the header rows are not).
fn first_cells<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<String> {
    lines
        .filter_map(|l| l.strip_prefix("| ")?.split_once(" | ").map(|(cell, _)| cell.to_owned()))
        .filter(|cell| cell.starts_with('`'))
        .collect()
}

/// The section of `text` that starts at the line `heading`, up to the next
/// heading of the same level.
fn section<'a>(text: &'a str, heading: &str) -> &'a str {
    let start = text.find(&format!("\n{heading}\n")).unwrap_or_else(|| panic!("no `{heading}`")) + 1;
    let end = text[start + heading.len()..].find("\n## ").map_or(text.len(), |i| start + heading.len() + i);
    &text[start..end]
}

/// The settings the server reads: the names in the first column of the
/// table at the top of `src/config.rs`.
fn settings() -> BTreeSet<String> {
    let config = read("crates/nuage/src/config.rs");
    let table = names(&first_cells(config.lines().map_while(|l| l.strip_prefix("//!")).map(str::trim_start)));
    for name in ["PORT", "NUAGE_ENCRYPTION_KEY", "NUAGE_KV_STORAGE", "NUAGE_RESTORE_REQUIRED", "NUAGE_BACKUP2_DIR"] {
        assert!(table.contains(name), "the table of src/config.rs was not read: {name} is not in {table:?}");
    }
    table
}

/// What is in `a` and not in `b`.
fn missing<'a>(a: &'a BTreeSet<String>, b: &BTreeSet<String>) -> Vec<&'a str> {
    a.iter().filter(|name| !b.contains(*name)).map(String::as_str).collect()
}

/// Every setting is described in the settings table of each README and in
/// `nuage.env.example`; they, and `app.json`, name nothing else; and the
/// three tables have the same first column, row by row.
#[test]
fn every_setting_is_documented_in_the_three_languages() {
    let settings = settings();
    let known: BTreeSet<String> = settings.iter().cloned().chain([READ_BY_THE_SITE.to_owned()]).collect();
    let mut first_columns = Vec::new();
    for (file, heading) in READMES {
        let text = read(file);
        let rows = first_cells(section(&text, heading).lines());
        let documented = names(&rows);
        assert_eq!(missing(&settings, &documented), Vec::<&str>::new(), "settings missing from the table of {file}");
        assert_eq!(missing(&documented, &known), Vec::<&str>::new(), "not settings, in the table of {file}");
        first_columns.push((file, rows));
    }
    let (english, rows) = &first_columns[0];
    for (file, other) in &first_columns[1..] {
        assert_eq!(other, rows, "the first column of the table of {file} is not the one of {english}");
    }

    // `NAME=value` lines, commented out or not.
    let example: BTreeSet<String> = read("nuage.env.example")
        .lines()
        .filter_map(|l| l.trim_start_matches('#').trim_start().split_once('='))
        .map(|(name, _)| name.to_owned())
        .filter(|name| is_name(name))
        .collect();
    let mut expected = settings.clone();
    expected.remove(NOT_IN_A_FILE);
    assert_eq!(missing(&expected, &example), Vec::<&str>::new(), "settings missing from nuage.env.example");
    assert_eq!(missing(&example, &known), Vec::<&str>::new(), "not settings, in nuage.env.example");

    let app: serde_json::Value = serde_json::from_str(&read("app.json")).expect("app.json is JSON");
    let offered: BTreeSet<String> = app["env"].as_object().expect("app.json has `env`").keys().cloned().collect();
    assert!(offered.contains("NUAGE_RESTORE_REQUIRED") && offered.contains("NUAGE_KV_STORAGE"), "{offered:?}");
    assert_eq!(missing(&offered, &known), Vec::<&str>::new(), "not settings, in app.json");
    // A first deployment has nothing to restore: an empty start is refused
    // only once someone sets it.
    assert_eq!(app["env"]["NUAGE_RESTORE_REQUIRED"].get("value"), None, "app.json must not preset it");
    assert_eq!(app["env"]["NUAGE_RESTORE_REQUIRED"]["required"], false);
}

/// What `src/config.rs` reads is in its table: each `"NUAGE_..."` it names
/// in its code, and the variables of both backup destinations, whose names
/// it puts together.
#[test]
fn the_table_of_config_lists_what_it_reads() {
    let settings = settings();
    let config = read("crates/nuage/src/config.rs");
    let code = config.split("\nmod tests {").next().unwrap();
    // Each `"NAME"` of the code.
    let mut read_there: BTreeSet<String> = code
        .match_indices('"')
        .filter_map(|(i, _)| {
            let rest = &code[i + 1..];
            let name = &rest[..rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len())];
            let quoted = rest[name.len()..].starts_with('"');
            (quoted && is_name(name) && (name.starts_with("NUAGE_") || name == "PORT")).then(|| name.to_owned())
        })
        .collect();
    // The start of the names of each destination's variables.
    for base in ["NUAGE_BACKUP", "NUAGE_BACKUP2"] {
        assert!(read_there.remove(base), "{base} is no longer the start of a destination's names");
        for end in
            ["DIR", "S3_BUCKET", "S3_REGION", "S3_ENDPOINT", "S3_PREFIX", "S3_ACCESS_KEY_ID", "S3_SECRET_ACCESS_KEY"]
        {
            read_there.insert(format!("{base}_{end}"));
        }
    }
    assert!(read_there.contains("NUAGE_KV_STORAGE") && read_there.contains("NUAGE_RESTORE_REQUIRED"));
    assert_eq!(missing(&read_there, &settings), Vec::<&str>::new(), "read by src/config.rs, not in its table");
}

/// The `##` and `###` headings of a Markdown text, outside code blocks.
fn outline(text: &str) -> Vec<(usize, &str)> {
    let mut in_code = false;
    let mut headings = Vec::new();
    for line in text.lines() {
        if line.starts_with("```") {
            in_code = !in_code;
        } else if !in_code && (line.starts_with("## ") || line.starts_with("### ")) {
            headings.push((line.find(' ').unwrap(), line));
        }
    }
    headings
}

/// The three READMEs are translations of one another: the same sections, in
/// the same order, at the same levels.
#[test]
fn the_three_readmes_have_the_same_headings() {
    let english = read(READMES[0].0);
    let levels = |text: &str| outline(text).into_iter().map(|(level, _)| level).collect::<Vec<_>>();
    assert!(levels(&english).len() > 20, "{:?}", outline(&english));
    for (file, _) in &READMES[1..] {
        let text = read(file);
        assert_eq!(
            levels(&text),
            levels(&english),
            "{file} and README.md have different headings:\n{:#?}\n{:#?}",
            outline(&text),
            outline(&english)
        );
    }
}
