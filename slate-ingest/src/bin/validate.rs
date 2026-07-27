//! validate — run the differential correctness harness against a running Slate.
//!
//! Prereq: `live` is already ingesting the SAME program into the configured ClickHouse. This tool
//! reads an independent reference RPC and Slate's store, waits for Slate to capture through the
//! reference's finalized slot, and diffs Slate's now-historical answer against it.
//!
//! Usage:
//!   REFERENCE_RPC=<url> cargo run -p slate-ingest --bin validate -- <program_pubkey> [--config slate.toml]
//!
//! Use a REFERENCE_RPC that did NOT seed Slate's baseline, or the unchanged accounts match
//! trivially and prove nothing. Exit code is non-zero on any mismatch, so it's CI-able.

use std::str::FromStr;
use std::time::Duration;

use clap::Parser;
use slate_common::config::Config;
use slate_ingest::validate::{Diff, diff, fetch_oracle, fetch_slate};
use slate_store::{ClickHouseClient, Fidelity};
use solana_pubkey::Pubkey;

const COVERAGE_WAIT_SECS: u64 = 120;

#[derive(Parser)]
struct Args {
    /// The program (owner) pubkey to validate, base58.
    program: String,
    /// Path to the config file.
    #[arg(long, default_value = "slate.toml")]
    config: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let cfg = Config::load(&args.config)?;
    let rpc_url = std::env::var("REFERENCE_RPC")
        .expect("set REFERENCE_RPC to an RPC that did NOT seed Slate's baseline");
    let program_bytes = Pubkey::from_str(&args.program)?.to_bytes();

    let store = ClickHouseClient::with_config(
        &cfg.clickhouse.url,
        &cfg.clickhouse.database,
        &cfg.clickhouse.user,
        &cfg.clickhouse.password,
    );

    // 1. Oracle: the reference RPC's view of the program at its current finalized slot.
    let (s1, oracle) = fetch_oracle(&rpc_url, &args.program).await?;
    println!("oracle: {} accounts at finalized slot {s1}", oracle.len());

    // 2. Wait until Slate has captured through S1, so we're asking about a slot in its past.
    println!("waiting for Slate to cover slot {s1} ...");
    let mut waited = 0u64;
    while !store.is_covered(s1, s1).await? {
        if waited >= COVERAGE_WAIT_SECS {
            anyhow::bail!(
                "Slate has not covered slot {s1} after {COVERAGE_WAIT_SECS}s — is `live` running and caught up for this program?"
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        waited += 2;
    }
    if let Some(tip) = store.latest_covered().await? {
        println!(
            "Slate covered slot {s1}; it is now {} slots behind Slate's tip ({tip}) — a genuinely historical read",
            tip.saturating_sub(s1)
        );
    }

    // 3. Slate's answer for the same program, as-of the now-historical slot S1.
    let (fidelity, slate) = fetch_slate(&store, &program_bytes, s1).await?;

    // Respect Slate's own honesty flag: Uncertain means a coverage gap straddles S1, so a mismatch
    // there would be Slate correctly refusing to vouch, not a data bug. Don't score it.
    if fidelity != Fidelity::Exact {
        println!("\nSlate reports fidelity Uncertain for slot {s1} (a coverage gap straddles it).");
        println!(
            "That's Slate being honest, not a data bug. Re-run once coverage is contiguous through {s1}. Skipping diff."
        );
        return Ok(());
    }

    // 4. Diff.
    let report = diff(&oracle, &slate);
    println!(
        "\ncompared {} oracle accounts against Slate as-of {s1}",
        report.compared
    );
    if report.rent_epoch_diffs > 0 {
        println!(
            "note: {} accounts differ on rent_epoch (vestigial field, not scored — informational)",
            report.rent_epoch_diffs
        );
    }

    if report.diffs.is_empty() {
        println!("PASS — Slate's reconstruction of slot {s1} matches the reference RPC exactly.");
        return Ok(());
    }

    println!("FAIL — {} mismatches:", report.diffs.len());
    for d in report.diffs.iter().take(50) {
        match d {
            Diff::MissingInSlate(p) => println!("  missing in Slate: {}", Pubkey::from(*p)),
            Diff::ExtraInSlate(p) => println!("  extra in Slate:   {}", Pubkey::from(*p)),
            Diff::Field {
                pubkey,
                field,
                oracle,
                slate,
            } => println!("  {}  {field}: oracle={oracle}  slate={slate}", Pubkey::from(*pubkey)),
        }
    }
    if report.diffs.len() > 50 {
        println!("  ... and {} more", report.diffs.len() - 50);
    }
    std::process::exit(1);
}
