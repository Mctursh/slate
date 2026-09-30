#![cfg(feature = "boundary-fixtures")]

use std::io::Write;

use slate_replay::{boundary_fixtures, fixture_capture::replay_fixture_file};

// cargo test captures the print macros; go straight to the fd so a long download is visible.
fn note(msg: &str) {
    let mut e = std::io::stderr();
    let _ = writeln!(e, "{msg}");
    let _ = e.flush();
}

#[test]
fn every_published_boundary_fixture_replays_to_its_recorded_hash() {
    let published = boundary_fixtures::published();
    assert!(
        !published.is_empty(),
        "boundary-fixtures is on but fixtures/boundary/checksums.txt lists none; \
         publish the release assets or run without the feature"
    );
    note(&format!(
        "\nboundary fixtures: {} published, resolving cache then downloading from GitHub",
        published.len()
    ));
    for (i, (name, sha256)) in published.iter().enumerate() {
        note(&format!("[{}/{}] {name}", i + 1, published.len()));
        let path =
            boundary_fixtures::ensure(name, sha256).unwrap_or_else(|e| panic!("{name}: {e}"));
        note(&format!("  {name}: replaying"));
        let (got, expected) = replay_fixture_file(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            got.to_bytes(),
            expected,
            "{name} replayed to a different hash"
        );
    }
}
