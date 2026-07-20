use slate_rpc::{Rpc, SlateRpcServer};
use slate_store::ClickHouseClient;
use tokio::main;


#[main]
async fn main() -> anyhow::Result<()> {
    let store = ClickHouseClient::new("http://localhost:8123");
    // 8899 = Solana's standard RPC port. (Not 9000 — that's ClickHouse's native TCP port,
    // which docker-compose maps to the host, so binding 9000 collides with ClickHouse.)
    let server = jsonrpsee::server::ServerBuilder::default().build("127.0.0.1:8899").await?;
    let handle = server.start(Rpc { store }.into_rpc());

    handle.stopped().await;
    Ok(())
}