#![cfg(feature = "boundary-fixtures")]

use slate_replay::{boundary_fixtures, fixture_capture::replay_fixture_file};

#[test]
fn every_published_boundary_fixture_replays_to_its_recorded_hash() {
    let published = boundary_fixtures::published();
    assert!(
        !published.is_empty(),
        "boundary-fixtures is on but fixtures/boundary/checksums.txt lists none; \
         publish the release assets or run without the feature"
    );
    for (name, sha256) in published {
        let path =
            boundary_fixtures::ensure(&name, &sha256).unwrap_or_else(|e| panic!("{name}: {e}"));
        let (got, expected) = replay_fixture_file(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            got.to_bytes(),
            expected,
            "{name} replayed to a different hash"
        );
    }
}
