use std::collections::BTreeMap;

const CHECKSUMS: &str = include_str!("../../fixtures/boundary/checksums.txt");
const EXPECTED: &str = include_str!("../../fixtures/boundary/expected.txt");

fn entries(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
}

fn declared() -> BTreeMap<u64, String> {
    entries(EXPECTED)
        .map(|l| {
            let (slot, note) = l.split_once(char::is_whitespace).unwrap_or_else(|| {
                panic!("fixtures/boundary/expected.txt: `{l}` has no description")
            });
            let slot = slot
                .parse()
                .unwrap_or_else(|_| panic!("fixtures/boundary/expected.txt: bad slot in `{l}`"));
            (slot, note.trim().to_string())
        })
        .collect()
}

fn slot_of(name: &str) -> u64 {
    name.strip_prefix("slot-")
        .and_then(|s| s.strip_suffix(".slfix.zst"))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("checksums.txt: `{name}` is not slot-<n>.slfix.zst"))
}

fn published_slots() -> Vec<u64> {
    entries(CHECKSUMS)
        .map(|l| {
            let name = l
                .split_whitespace()
                .nth(1)
                .unwrap_or_else(|| panic!("checksums.txt: `{l}` has no filename"));
            slot_of(name)
        })
        .collect()
}

#[test]
fn every_published_boundary_fixture_says_what_it_exercises() {
    let declared = declared();
    let published = published_slots();
    assert!(
        !published.is_empty(),
        "checksums.txt publishes no boundary fixtures"
    );
    for slot in &published {
        assert!(
            declared.contains_key(slot),
            "checksums.txt publishes slot {slot} but fixtures/boundary/expected.txt doesn't describe it"
        );
    }
    for slot in declared.keys() {
        assert!(
            published.contains(slot),
            "fixtures/boundary/expected.txt describes slot {slot} but checksums.txt doesn't publish it"
        );
    }
}

#[cfg(feature = "boundary-fixtures")]
#[test]
fn every_published_boundary_fixture_replays_to_its_recorded_hash() {
    use slate_replay::{boundary_fixtures, fixture_capture::replay_fixture_file};
    use std::io::Write;

    let note = |msg: &str| {
        let mut e = std::io::stderr();
        let _ = writeln!(e, "{msg}");
        let _ = e.flush();
    };

    let declared = declared();
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
        let what = declared
            .get(&slot_of(name))
            .map_or("undescribed", String::as_str);
        note(&format!("[{}/{}] {name} ({what})", i + 1, published.len()));
        let path = boundary_fixtures::ensure(name, sha256)
            .unwrap_or_else(|e| panic!("{name} ({what}): {e}"));
        note(&format!("  {name}: replaying"));
        let (got, expected) =
            replay_fixture_file(&path).unwrap_or_else(|e| panic!("{name} ({what}): {e}"));
        assert_eq!(
            got.to_bytes(),
            expected,
            "{name} ({what}) replayed to a different hash"
        );
    }
}
