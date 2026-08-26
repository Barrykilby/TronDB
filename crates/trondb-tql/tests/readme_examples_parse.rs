//! Every TQL statement shown in README.md must parse.
//!
//! Documented syntax that does not parse is worse than undocumented syntax: it
//! sends a reader down a path the engine rejects. This extracts the statements
//! from the README's fenced `sql` blocks and parses each one, so the docs
//! cannot drift from the grammar without failing the build.

use std::path::PathBuf;

fn readme() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("README.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Statements from every ```sql fenced block, with comments stripped.
fn tql_statements(md: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_sql = false;
    let mut buf = String::new();

    for line in md.lines() {
        let t = line.trim();
        if t.starts_with("```") {
            if in_sql {
                in_sql = false;
                for stmt in buf.split(';') {
                    let s = stmt.trim();
                    if !s.is_empty() {
                        out.push(format!("{s};"));
                    }
                }
                buf.clear();
            } else if t == "```sql" {
                in_sql = true;
            }
            continue;
        }
        if in_sql {
            let code = match line.find("--") {
                Some(i) => &line[..i],
                None => line,
            };
            buf.push_str(code);
            buf.push('\n');
        }
    }
    out
}

#[test]
fn every_documented_statement_parses() {
    let md = readme();
    let statements = tql_statements(&md);
    assert!(
        statements.len() > 20,
        "extracted only {} statements; the extractor is probably broken",
        statements.len()
    );

    let mut failures = Vec::new();
    for stmt in &statements {
        // Vector literals in the README are elided with `...`, which is not
        // valid TQL. Drop the ellipsis so the rest of the statement is checked.
        // Vector literals are elided in prose as `...`; give them a value so
        // the surrounding statement is still checked rather than skipped.
        let concrete = stmt
            .replace("[0.1, 0.2, 0.3, ...]", "[0.1, 0.2, 0.3]")
            .replace("[0.1, 0.2, ...]", "[0.1, 0.2]")
            .replace("[...]", "[0.1, 0.2]")
            .replace(", ...", "")
            .replace("...", "");
        if let Err(e) = trondb_tql::parse(&concrete) {
            failures.push(format!("  {}\n      -> {e}", concrete.replace('\n', " ")));
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} documented TQL statements do not parse:\n{}",
        failures.len(),
        statements.len(),
        failures.join("\n")
    );
}
