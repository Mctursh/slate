use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    /// ClickHouse connection. Section optional — omitted fields fall back to localhost/`slate`.
    #[serde(default)]
    pub clickhouse: Clickhouse,
    /// Live ingest settings. Required by the `live` binary, ignored by the RPC server.
    #[serde(default)]
    pub ingest: Option<Ingest>,
    /// RPC server settings. Section optional — bind address defaults to 127.0.0.1:8899.
    #[serde(default)]
    pub rpc: Rpc,
}

#[derive(Debug, Deserialize)]
pub struct Clickhouse {
    #[serde(default = "default_ch_url")]
    pub url: String,
    #[serde(default = "default_slate")]
    pub database: String,
    #[serde(default = "default_slate")]
    pub user: String,
    #[serde(default = "default_slate")]
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct Ingest {
    /// Yellowstone gRPC (Dragon's Mouth) endpoint for the live stream.
    #[serde(rename = "grpc-endpoint", default = "default_grpc_endpoint")]
    pub grpc_endpoint: String,
    /// The program whose accounts to capture (owner scope). Required.
    pub program: String,
    /// gRPC (and default baseline-RPC) auth token. Lives in the gitignored `slate.toml`; the
    /// committed example has a placeholder. Overridden by the `GRPC_TOKEN` env var when set.
    #[serde(rename = "x-token", default)]
    pub x_token: Option<String>,
    /// JSON-RPC endpoint for the getProgramAccounts baseline. If omitted, defaults to the Helius
    /// devnet RPC built from the token.
    #[serde(rename = "baseline-rpc", default)]
    pub baseline_rpc: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Rpc {
    /// Address the JSON-RPC server binds to.
    #[serde(default = "default_bind")]
    pub bind: String,
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {path} (copy slate.example.toml to slate.toml)"))?;
        toml::from_str(&text).with_context(|| format!("parsing config file {path}"))
    }
}

impl Default for Clickhouse {
    fn default() -> Self {
        Self {
            url: default_ch_url(),
            database: default_slate(),
            user: default_slate(),
            password: default_slate(),
        }
    }
}

impl Default for Rpc {
    fn default() -> Self {
        Self { bind: default_bind() }
    }
}

fn default_ch_url() -> String {
    "http://localhost:8123".into()
}
fn default_slate() -> String {
    "slate".into()
}
fn default_grpc_endpoint() -> String {
    "https://laserstream-devnet-ewr.helius-rpc.com".into()
}
fn default_bind() -> String {
    "127.0.0.1:8899".into()
}
