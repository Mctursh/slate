use std::path::PathBuf;

use slate_replay::fixture_capture::replay_fixture_file;

const EXPECTED: &str = include_str!("../../fixtures/slots/expected.txt");

fn declared() -> Vec<(u64, String)> {
    EXPECTED
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (slot, note) = l.split_once(char::is_whitespace)?;
            Some((slot.parse().ok()?, note.trim().to_string()))
        })
        .collect()
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/slots")
}

#[test]
fn every_declared_slot_fixture_is_present_and_replays_to_its_recorded_hash() {
    for (slot, note) in declared() {
        let path = dir().join(format!("slot-{slot}.slfix"));
        assert!(
            path.exists(),
            "fixtures/slots/expected.txt declares slot {slot} ({note}) but {} is missing",
            path.display()
        );
        let (got, expected) =
            replay_fixture_file(&path).unwrap_or_else(|e| panic!("slot {slot} ({note}): {e}"));
        assert_eq!(
            got.to_bytes(),
            expected,
            "slot {slot} ({note}) replayed to a different bank hash"
        );
    }
}

#[test]
fn no_fixture_sits_in_the_directory_undeclared() {
    let declared: Vec<u64> = declared().into_iter().map(|(s, _)| s).collect();
    let Ok(entries) = std::fs::read_dir(dir()) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(slot) = name
            .strip_prefix("slot-")
            .and_then(|s| s.strip_suffix(".slfix"))
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        assert!(
            declared.contains(&slot),
            "{name} is committed but not listed in expected.txt, so nothing would notice if it rotted"
        );
    }
}
