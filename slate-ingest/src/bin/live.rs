use slate_ingest::capture::Capturer;
use slate_ingest::yellowstone::{self, IngestConfig};
use slate_store::ClickHouseClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = IngestConfig {
        endpoint: "http://127.0.0.1:10000".into(), // your plugin's gRPC port
        x_token: None,
        owners: vec![], // empty = every account (fine on a quiet validator)
    };
    let store = ClickHouseClient::new("http://localhost:8123");
    let mut capturer = Capturer::new(store); // no baseline yet — just stream + commit
    yellowstone::run(&cfg, &mut capturer).await
}
