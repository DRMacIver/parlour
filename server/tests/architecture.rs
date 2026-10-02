//! The access layer's boundary (DESIGN.md §6): only the modules in
//! `src/db/` run SQL. The rest of the app gets opaque `Db` and `Tx`
//! handles, which the type system already keeps it from querying; this
//! test also catches SQL written straight into a handler.

use std::fs;
use std::path::Path;

/// Signs of code running SQL itself.
const SQL: &[&str] = &[
    "sqlx::query",
    "sqlx::raw_sql",
    "query!(",
    "query_scalar!(",
    "query_as!(",
    ".fetch_one(",
    ".fetch_all(",
    ".fetch_optional(",
    "PgConnection",
    "Transaction<",
];

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn only_the_db_modules_run_sql() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    assert!(files.len() > 10, "found only {files:?}");
    let mut offences = Vec::new();
    for file in files {
        let relative = file.strip_prefix(&src).unwrap();
        // The binary's entry point owns the pool and runs migrations.
        if relative.starts_with("db") || relative == Path::new("main.rs") {
            continue;
        }
        let text = fs::read_to_string(&file).unwrap();
        for (n, line) in text.lines().enumerate() {
            for sign in SQL {
                if line.contains(sign) {
                    offences.push(format!("{}:{}: {}", relative.display(), n + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        offences.is_empty(),
        "SQL outside src/db:\n{}",
        offences.join("\n")
    );
}
