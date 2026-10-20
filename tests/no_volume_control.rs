//! Regression guard: auto-airplay must never change the speaker volume.
//!
//! This is a source scan rather than a behavioural test because the streaming
//! path needs a real AirPlay receiver (there is no mock RTSP server to assert
//! on), and the failure mode is silent: a stray `set_volume(` call makes the
//! speaker jump to a new volume on connect, re-acquire or reconnect. The scan
//! covers our sources and all production code of the vendored (patched)
//! `airplay-client`, which upstream used to send a 100 % volume from while
//! setting up a stream: any `set_volume(` there must sit inside a function
//! literally named `set_volume` (the explicit, opt-in API and its delegations).

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

/// Production code of a vendored file: everything from the first
/// `#[cfg(test)]` onward is dropped, then `//` comments are removed.
fn production_code(source: &str) -> String {
    let production = source
        .find("#[cfg(test)]")
        .map_or(source, |index| &source[..index]);
    code_only(production)
}

/// Byte ranges of the bodies of every `fn set_volume(`, found by matching braces.
fn set_volume_bodies(source: &str) -> Vec<std::ops::Range<usize>> {
    let marker = "fn set_volume(";
    let mut bodies = Vec::new();
    let mut from = 0;
    while let Some(found) = source[from..].find(marker) {
        let start = from + found;
        from = start + marker.len();
        let Some(open) = source[start..].find(['{', ';']).map(|index| start + index) else {
            continue;
        };
        if source[open..].starts_with(';') {
            continue; // declaration without a body
        }
        let mut depth = 0usize;
        for (offset, character) in source[open..].char_indices() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        bodies.push(open..open + offset + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    bodies
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
fn vendored_client_only_sends_a_volume_from_explicit_set_volume_functions() {
    let mut files = Vec::new();
    rust_files(&root(VENDORED_SRC), &mut files);
    assert!(!files.is_empty());

    let mut definitions = 0;
    for file in files {
        let code = production_code(&fs::read_to_string(&file).unwrap());
        let bodies = set_volume_bodies(&code);
        definitions += bodies.len();

        for (index, _) in code.match_indices("set_volume(") {
            let is_definition = code[..index].ends_with("fn ");
            let in_set_volume = bodies.iter().any(|body| body.contains(&index));
            assert!(
                is_definition || in_set_volume,
                "{} sends a volume outside an explicit `set_volume` function: ...{}",
                file.display(),
                &code[index.saturating_sub(60)..(index + 40).min(code.len())]
            );
        }
    }
    // The scan must have seen the explicit API (connection, RAOP, client).
    assert!(
        definitions >= 3,
        "found only {definitions} set_volume bodies"
    );
}

#[test]
fn vendored_client_keeps_the_explicit_set_volume_api() {
    // The patch removes only the implicit call; callers can still opt in.
    let connection = fs::read_to_string(root(VENDORED_SRC).join("connection.rs")).unwrap();
    assert!(connection.contains("pub async fn set_volume("));
}

#[test]
fn events_channel_responder_never_mentions_volume_or_set_parameter() {
    // The responder only answers `200 OK` to the receiver's requests; it must
    // not contain any code path that could name, let alone send, a volume.
    let source = fs::read_to_string(root(VENDORED_SRC).join("receiver_events.rs")).unwrap();
    let code = production_code(&source).to_ascii_lowercase();
    for forbidden in ["volume", "set_parameter", "setparameter"] {
        assert!(
            !code.contains(forbidden),
            "receiver_events.rs production code mentions `{forbidden}`"
        );
    }
}
