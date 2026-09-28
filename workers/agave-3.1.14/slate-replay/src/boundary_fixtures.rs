//! Fetches the boundary fixtures, which are too big for git and ship as release assets.
//!
//! Only compiled under the `boundary-fixtures` feature, so the default `cargo test` stays
//! offline. A download is trusted only after its sha256 matches the committed checksum.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

const BASE_URL: &str = "https://github.com/Mctursh/slate/releases/download/fixtures-v1/";
const CHECKSUMS: &str = include_str!("../../fixtures/boundary/checksums.txt");

/// `(filename, sha256)` for every published boundary fixture.
pub fn published() -> Vec<(String, String)> {
    CHECKSUMS
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.to_string(), it.next()?.to_string()))
        })
        .map(|(sha, name)| (name, sha))
        .collect()
}

fn cache_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/boundary-fixtures")
}

/// Path to the decompressed fixture, downloading and verifying it on first use.
pub fn ensure(name: &str, sha256: &str) -> Result<PathBuf> {
    let dir = cache_dir();
    let out = dir.join(name.trim_end_matches(".zst"));
    if out.exists() {
        return Ok(out);
    }
    std::fs::create_dir_all(&dir)?;

    let url = format!("{BASE_URL}{name}");
    let bytes = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(600))
        .build()?
        .get(&url)
        .send()
        .with_context(|| format!("downloading {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?
        .bytes()?;

    let got = hex(&Sha256::digest(&bytes));
    if got != sha256 {
        bail!("checksum mismatch for {name}: expected {sha256}, got {got}");
    }

    let decoded = zstd::decode_all(&bytes[..]).with_context(|| format!("decompressing {name}"))?;
    write_atomically(&out, &decoded)?;
    Ok(out)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("partial");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
