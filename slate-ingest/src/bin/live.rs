use slate_ingest::capture::Capturer;
use slate_ingest::yellowstone::{self, IngestConfig};
use slate_store::ClickHouseClient;

// SPL Token program. Scoping to one program keeps the account stream sane on a shared endpoint.
const SPL_TOKEN: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = IngestConfig {
        endpoint: "https://laserstream-devnet-ewr.helius-rpc.com".into(),
        x_token: std::env::var("GRPC_TOKEN").ok(),
        owners: vec![SPL_TOKEN.into()],
    };
    let store = ClickHouseClient::new("http://localhost:8123");
    let mut capturer = Capturer::new(store);
    yellowstone::run(&cfg, &mut capturer).await
}
