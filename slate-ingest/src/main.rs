use slate_ingest::read_snapshot_accounts;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    read_snapshot_accounts(
        "/Users/mctursh/Documents/blockchain/open-source/slate/dev-snapshot/extracted/accounts/",
    )
    .await?;
    Ok(())
}
