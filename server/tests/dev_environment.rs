//! The development setup's Postgres port must be one the operating system
//! never hands out as a client's own (ephemeral) port.
//!
//! A client connecting to 127.0.0.1:P from an ephemeral port that happens
//! to be P itself is connected to itself by TCP's simultaneous open: it
//! reads back its own startup message and sqlx fails with "unknown message
//! type: '\0'". macOS chooses ephemeral ports from 49152-65535 and Linux
//! from 32768-60999, so a port in either range failed about one connection
//! in sixteen thousand, which made long test runs fail now and then.

use std::fs;
use std::path::{Path, PathBuf};

/// The lowest ephemeral port of the systems we develop on (Linux's).
const FIRST_EPHEMERAL_PORT: u32 = 32768;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// The ports of every `@localhost:PORT` URL in `text`.
fn localhost_ports(text: &str) -> Vec<u32> {
    text.match_indices("@localhost:")
        .map(|(at, needle)| {
            text[at + needle.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap()
        })
        .collect()
}

/// Files that say where the dev Postgres is.
const FILES: &[&str] = &[
    "README.md",
    "dev/compose.yaml",
    "dev/check_builds.py",
    "e2e/server.mjs",
    "e2e/tests/accounts.spec.ts",
];

#[test]
fn the_dev_postgres_port_is_never_an_ephemeral_port() {
    let compose = fs::read_to_string(root().join("dev/compose.yaml")).unwrap();
    for file in FILES {
        let text = fs::read_to_string(root().join(file)).unwrap();
        let ports = localhost_ports(&text);
        for &port in &ports {
            assert!(
                port < FIRST_EPHEMERAL_PORT,
                "{file} connects to Postgres on port {port}, which is an ephemeral port"
            );
        }
        // Some files also give CI's address as an example; the dev one
        // must be among them.
        assert!(
            ports
                .iter()
                .any(|port| compose.contains(&format!("\"127.0.0.1:{port}:5432\""))),
            "{file} names none of the ports dev/compose.yaml publishes: {ports:?}"
        );
    }
}

#[test]
fn localhost_ports_are_read_from_urls() {
    assert_eq!(
        localhost_ports("postgres://a:b@localhost:25432/x and p://c@localhost:1/"),
        vec![25432, 1]
    );
}
