//! What slots does a block cache actually hold? Usage: probe_block_cache <cache.redb>

use anyhow::{Context, Result, bail};
use redb::{Database, ReadableTable, TableDefinition};

const BLOCKS: TableDefinition<u64, &[u8]> = TableDefinition::new("blocks");

fn main() -> Result<()> {
    let path = std::env::args().nth(1).context("usage: <cache.redb>")?;
    let db = Database::builder()
        .open(&path)
        .context("opening the cache")?;
    let txn = db.begin_read()?;
    let table = match txn.open_table(BLOCKS) {
        Ok(t) => t,
        Err(e) => bail!("no `blocks` table in {path}: {e}"),
    };
    let mut count = 0u64;
    let mut min = u64::MAX;
    let mut max = 0u64;
    let mut bytes = 0u64;
    for row in table.iter()?.flatten() {
        let slot = row.0.value();
        count += 1;
        min = min.min(slot);
        max = max.max(slot);
        bytes += row.1.value().len() as u64;
    }
    if count == 0 {
        println!("{path}: table exists but is EMPTY");
        return Ok(());
    }
    println!("{path}");
    println!("  blocks: {count}");
    println!("  slots:  {min} .. {max}");
    println!("  data:   {:.2} GB", bytes as f64 / 1e9);
    Ok(())
}
