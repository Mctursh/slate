//! slate-rpc — HTTP JSON-RPC server exposing the historical account store in Agave's shape.
//!
//! jsonrpsee is the transport (server + `{jsonrpc,id,result/error}` envelope + error type).
//! The Solana crates produce the exact payloads: `encode_ui_account` -> `UiAccount` (the
//! `value` object, base64 data, camelCase, `space`, etc.), and `Response`/`RpcResponseContext`
//! give the `{context, value}` wrapper. Reference: cloudbreak crates/api/src/methods.

use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
    types::ErrorObject,
};
use slate_store::{AccountUpdate, ClickHouseClient};
use solana_account::Account;
use solana_account_decoder::{UiAccountEncoding, encode_ui_account};
use solana_account_decoder_client_types::UiAccount;
use solana_pubkey::Pubkey;
use solana_rpc_client_api::response::{
    Response as RpcResponse, RpcKeyedAccount, RpcResponseContext,
};

#[rpc(server)]
pub trait SlateRpc {
    #[method(name = "getAccountInfo")]
    async fn get_account_info(
        &self,
        pubkey: String,
        as_of_slot: u64,
    ) -> RpcResult<serde_json::Value>;

    #[method(name = "getProgramAccounts")]
    async fn get_program_accounts(
        &self,
        owner: String,
        as_of_slot: u64,
    ) -> RpcResult<serde_json::Value>;
}

pub struct Rpc {
    pub store: ClickHouseClient,
}

#[async_trait]
impl SlateRpcServer for Rpc {
    async fn get_account_info(
        &self,
        pubkey: String,
        as_of_slot: u64,
    ) -> RpcResult<serde_json::Value> {
        let key = decode_pubkey(pubkey)?;

        let account = self
            .store
            .get_account_info(&key, as_of_slot)
            .await
            .map_err(|_| ErrorObject::owned(-32603, "failed to query account store", None::<()>))?;

        // Agave getAccountInfo: { context: { slot }, value: UiAccount | null }
        let response = RpcResponse {
            context: RpcResponseContext {
                slot: as_of_slot,
                api_version: None,
            },
            value: account.map(|a| encode(&a)),
        };
        to_value(response)
    }

    async fn get_program_accounts(
        &self,
        owner: String,
        as_of_slot: u64,
    ) -> RpcResult<serde_json::Value> {
        let key = decode_pubkey(owner)?;

        let rows = self
            .store
            .get_program_accounts(&key, as_of_slot)
            .await
            .map_err(|_| {
                ErrorObject::owned(-32603, "failed to query program accounts", None::<()>)
            })?;

        // Agave getProgramAccounts (default): [ { pubkey, account: UiAccount } ]
        let keyed: Vec<RpcKeyedAccount> = rows
            .into_iter()
            .map(|a| RpcKeyedAccount {
                pubkey: Pubkey::from(a.pubkey).to_string(),
                account: encode(&a),
            })
            .collect();
        to_value(keyed)
    }
}

/// Map a stored account to Agave's `UiAccount` (base64 encoding for now).
fn encode(a: &AccountUpdate) -> UiAccount {
    let account = Account {
        lamports: a.lamports,
        data: a.data.clone(),
        owner: Pubkey::from(a.owner),
        executable: a.executable != 0,
        rent_epoch: a.rent_epoch,
    };
    encode_ui_account(
        &Pubkey::from(a.pubkey),
        &account,
        UiAccountEncoding::Base64,
        None,
        None,
    )
}

/// base58 string -> 32-byte key, validated by solana-pubkey.
fn decode_pubkey(s: String) -> RpcResult<[u8; 32]> {
    let pk: Pubkey = s
        .parse()
        .map_err(|_| ErrorObject::owned(-32602, "invalid base58 pubkey", None::<()>))?;
    Ok(pk.to_bytes())
}

fn to_value<T: serde::Serialize>(v: T) -> RpcResult<serde_json::Value> {
    serde_json::to_value(v)
        .map_err(|_| ErrorObject::owned(-32603, "failed to serialize response", None::<()>))
}
