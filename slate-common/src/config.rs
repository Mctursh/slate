use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub clickhouse: Clickhouse,
    #[serde(default)]
    pub ingest: Option<Ingest>,
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
    #[serde(rename = "grpc-endpoint")]
    pub grpc_endpoint: String,
    pub program: String,
    // GRPC_TOKEN env overrides this
    #[serde(rename = "x-token", default)]
    pub x_token: Option<String>,
    #[serde(rename = "baseline-rpc")]
    pub baseline_rpc: String,
}

#[derive(Debug, Deserialize)]
pub struct Rpc {
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
fn default_bind() -> String {
    "127.0.0.1:8899".into()
}
