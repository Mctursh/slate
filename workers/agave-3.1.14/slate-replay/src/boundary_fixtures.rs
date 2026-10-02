//! Fetches the boundary fixtures, which are too big for git and ship as release assets.
//!
//! Only compiled under the `boundary-fixtures` feature, so the default `cargo test` stays
//! offline. A download is trusted only after its sha256 matches the committed checksum.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

// cargo test captures the print macros; go straight to the fd or a long download looks hung.
fn note(msg: &str) {
    let mut e = std::io::stderr();
    let _ = writeln!(e, "{msg}");
    let _ = e.flush();
}

// Rewrites the same line; pad so a shorter update cannot leave the tail of a longer one behind.
fn progress(msg: &str) {
    let mut e = std::io::stderr();
    let _ = write!(e, "\r{msg:<76}");
    let _ = e.flush();
}

fn progress_done() {
    let mut e = std::io::stderr();
    let _ = writeln!(e);
    let _ = e.flush();
}

const BASE_URL: &str = concat!(
    env!("CARGO_PKG_REPOSITORY"),
    "/releases/download/fixtures-v1/"
);
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
        note(&format!("  {name}: cached"));
        return Ok(out);
    }
    std::fs::create_dir_all(&dir)?;

    let url = format!("{BASE_URL}{name}");
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(300))
        .build()?;
    let tmp = dir.join(format!("{name}.part"));

    let mut last = None;
    for attempt in 1..=12u32 {
        let have = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
        if have > 0 {
            note(&format!(
                "  {name}: resuming from {:.1} MB (attempt {attempt})",
                have as f64 / 1048576.0
            ));
        } else {
            note(&format!(
                "  {name}: fetching from GitHub (attempt {attempt})"
            ));
        }
        match fetch_from(&client, &url, &tmp, have, name) {
            Ok(total) => {
                note(&format!(
                    "  {name}: downloaded {:.1} MB",
                    total as f64 / 1048576.0
                ));
                last = None;
                break;
            }
            Err(e) => {
                let now = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
                note(&format!(
                    "  {name}: interrupted at {:.1} MB ({e}); retrying",
                    now as f64 / 1048576.0
                ));
                last = Some(e);
                std::thread::sleep(Duration::from_secs(3));
            }
        }
    }
    if let Some(e) = last {
        return Err(e).with_context(|| format!("downloading {url}"));
    }

    note(&format!("  {name}: verifying checksum"));
    let bytes = std::fs::read(&tmp)?;
    let got = hex(&Sha256::digest(&bytes));
    if got != sha256 {
        let _ = std::fs::remove_file(&tmp);
        bail!("checksum mismatch for {name}: expected {sha256}, got {got}");
    }

    let decoded = zstd::decode_all(&bytes[..]).with_context(|| format!("decompressing {name}"))?;
    write_atomically(&out, &decoded)?;
    let _ = std::fs::remove_file(&tmp);
    note(&format!(
        "  {name}: ready ({:.0} MB)",
        decoded.len() as f64 / 1048576.0
    ));
    Ok(out)
}

// Resume by byte range: release-asset bandwidth throttles, so restarting from zero never finishes.
fn fetch_from(
    client: &reqwest::blocking::Client,
    url: &str,
    tmp: &Path,
    have: u64,
    name: &str,
) -> Result<u64> {
    let mut req = client.get(url);
    if have > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let mut resp = req.send()?.error_for_status()?;
    let resumed = resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    let total = resp
        .content_length()
        .map(|l| l + if resumed { have } else { 0 });
    let mut f = if resumed {
        std::fs::OpenOptions::new().append(true).open(tmp)?
    } else {
        std::fs::File::create(tmp)?
    };

    let mut buf = vec![0u8; 256 * 1024];
    let mut done = if resumed { have } else { 0 };
    let start = Instant::now();
    let mut reported = Instant::now();
    let mut reported_any = false;
    loop {
        let n = resp.read(&mut buf)?;
        if n == 0 {
            break;
        }
        f.write_all(&buf[..n])?;
        done += n as u64;
        if reported.elapsed() >= Duration::from_secs(10) {
            let rate = (done.saturating_sub(have)) as f64 / start.elapsed().as_secs_f64();
            match total {
                Some(t) => progress(&format!(
                    "  {name}: {:.0}/{:.0} MB ({:.0}%) at {:.2} MB/s",
                    done as f64 / 1048576.0,
                    t as f64 / 1048576.0,
                    done as f64 / t as f64 * 100.0,
                    rate / 1048576.0
                )),
                None => progress(&format!(
                    "  {name}: {:.0} MB at {:.2} MB/s",
                    done as f64 / 1048576.0,
                    rate / 1048576.0
                )),
            }
            reported = Instant::now();
            reported_any = true;
        }
    }
    f.flush()?;
    if reported_any {
        progress_done();
    }
    Ok(done)
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
