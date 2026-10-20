//! Regression guard: auto-airplay must never change the speaker volume.
//!
//! This is a source scan rather than a behavioural test because the streaming
//! path needs a real AirPlay receiver (there is no mock RTSP server to assert
//! on), and the failure mode is silent: a stray `set_volume(` call makes the
//! speaker jump to a new volume on connect, re-acquire or reconnect. The scan
//! covers our sources and the stream-start functions of the vendored (patched)
//! `airplay-client`, which upstream used to send a 100 % volume from.

use std::fs;
use std::path::{Path, PathBuf};

const VENDORED_SRC: &str = "vendor/airplay-client/src";

fn root(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn rust_files(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

/// Source with `//` comments removed, so explanations may mention the call.
fn code_only(source: &str) -> String {
    source
        .lines()
        .map(|line| line.find("//").map_or(line, |index| &line[..index]))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Body of the first `fn <name>(` in `source`, found by matching braces.
fn function_body(source: &str, name: &str) -> String {
    let marker = format!("fn {name}(");
    let start = source
        .find(&marker)
        .unwrap_or_else(|| panic!("`{name}` not found"));
    let open = start + source[start..].find('{').expect("function body");
    let mut depth = 0usize;
    for (offset, character) in source[open..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return source[open..=open + offset].to_owned();
                }
            }
            _ => {}
        }
    }
    panic!("unterminated `{name}`");
}

#[test]
fn application_sources_never_set_the_speaker_volume() {
    let mut files = Vec::new();
    rust_files(&root("src"), &mut files);
    assert!(!files.is_empty());

    for file in files {
        let code = code_only(&fs::read_to_string(&file).unwrap());
        assert!(
            !code.contains("set_volume("),
            "{} calls set_volume(); auto-airplay must not change the speaker volume",
            file.display()
        );
    }
}

#[test]
fn vendored_stream_start_does_not_send_a_volume() {
    let connection =
        code_only(&fs::read_to_string(root(VENDORED_SRC).join("connection.rs")).unwrap());
    let raop =
        code_only(&fs::read_to_string(root(VENDORED_SRC).join("raop_connection.rs")).unwrap());

    let functions = [
        ("connection.rs", &connection, "start_streaming"),
        ("connection.rs", &connection, "start_streaming_live"),
        ("raop_connection.rs", &raop, "start_streaming"),
    ];
    for (file, source, name) in functions {
        let body = function_body(source, name);
        assert!(body.len() > 200, "{file}::{name} body looks truncated");
        assert!(
            !body.contains("set_volume("),
            "{file}::{name} sends an implicit volume"
        );
    }
}

#[test]
fn vendored_client_keeps_the_explicit_set_volume_api() {
    // The patch removes only the implicit call; callers can still opt in.
    let connection = fs::read_to_string(root(VENDORED_SRC).join("connection.rs")).unwrap();
    assert!(connection.contains("pub async fn set_volume("));
}
