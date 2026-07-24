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
use slate_store::{
    AccountAtSlot, AccountUpdate, ClickHouseClient, Fidelity, ProgramAccountAtSlot,
    ProgramAccountsPage,
};
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
        limit: Option<u64>, 
        cursor: Option<String>
    ) -> RpcResult<serde_json::Value>;

    #[method(name = "getBalance")]
    async fn get_balance(&self, pubkey: String, as_of_slot: u64) -> RpcResult<serde_json::Value>;

    #[method(name = "getMultipleAccounts")]
    async fn get_multiple_accounts(
        &self,
        pubkeys: Vec<String>,
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

        let AccountAtSlot { account, fidelity } = self
            .store
            .get_account_info_as_of(&key, as_of_slot)
            .await
            .map_err(|_| internal("failed to query account store"))?;

        respond(as_of_slot, account.map(|a| encode(&a)), fidelity)
    }

    async fn get_program_accounts(
        &self,
        owner: String,
        as_of_slot: u64,
        limit: Option<u64>,
        cursor: Option<String>
    ) -> RpcResult<serde_json::Value> {
        let key = decode_pubkey(owner)?;

        let Some(limit) = limit else {
            let ProgramAccountAtSlot { accounts, fidelity } = self
                .store
                .get_program_accounts_as_of(&key, as_of_slot)
                .await
                .map_err(|_| internal("failed to query program accounts"))?;
            return respond(as_of_slot, keyed(accounts), fidelity);
        };

        let cursor = cursor.map(decode_pubkey).transpose()?;
        let ProgramAccountsPage {
            accounts,
            fidelity,
            next_cursor,
        } = self
            .store
            .get_program_accounts_page(&key, as_of_slot, cursor, limit)
            .await
            .map_err(|_| internal("failed to query program accounts"))?;

        let mut response = respond(as_of_slot, keyed(accounts), fidelity)?;
        response["context"]["next_cursor"] =
            to_value(next_cursor.map(|c| Pubkey::from(c).to_string()))?;
        Ok(response)
    }

    async fn get_multiple_accounts(
        &self,
        pubkeys: Vec<String>,
        as_of_slot: u64,
    ) -> RpcResult<serde_json::Value> {
        let keys = pubkeys
            .iter()
            .map(|k| decode_pubkey(k.to_string()))
            .collect::<RpcResult<Vec<_>>>()?;
        let mut fidelities: Vec<Fidelity> = Vec::with_capacity(keys.len());
        let mut accounts: Vec<Option<UiAccount>> = Vec::with_capacity(keys.len());
        for key in keys {
            let AccountAtSlot { account, fidelity } = self
                .store
                .get_account_info_as_of(&key, as_of_slot)
                .await
                .map_err(|_| internal("failed to query account store"))?;
            let value = account.map(|a| encode(&a));
            fidelities.push(fidelity);
            accounts.push(value);
        }
        let mut response = to_value(RpcResponse {
            context: RpcResponseContext {
                slot: as_of_slot,
                api_version: None,
            },
            value: accounts,
        })?;
        response["context"]["fidelities"] = to_value(fidelities)?;
        Ok(response)
    }

    async fn get_balance(&self, pubkey: String, as_of_slot: u64) -> RpcResult<serde_json::Value> {
        let key = decode_pubkey(pubkey)?;
        let AccountAtSlot { account, fidelity } = self
            .store
            .get_account_info_as_of(&key, as_of_slot)
            .await
            .map_err(|_| internal("failed to query account store"))?;

        let value = account.map(|a| a.lamports).unwrap_or(0);
        respond(as_of_slot, value, fidelity)
    }
}

/// Build the Agave `{ context: { slot, fidelity }, value }` envelope shared by every read method.
/// The paginated scan splices `next_cursor` into `context` afterward.
fn respond<T: serde::Serialize>(
    slot: u64,
    value: T,
    fidelity: Fidelity,
) -> RpcResult<serde_json::Value> {
    let mut response = to_value(RpcResponse {
        context: RpcResponseContext {
            slot,
            api_version: None,
        },
        value,
    })?;
    response["context"]["fidelity"] = to_value(fidelity)?;
    Ok(response)
}

fn internal(msg: &'static str) -> ErrorObject<'static> {
    ErrorObject::owned(-32603, msg, None::<()>)
}

/// Map stored accounts to Agave's keyed-account array (`[{ pubkey, account }]`).
fn keyed(accounts: Vec<AccountUpdate>) -> Vec<RpcKeyedAccount> {
    accounts
        .into_iter()
        .map(|a| RpcKeyedAccount {
            pubkey: Pubkey::from(a.pubkey).to_string(),
            account: encode(&a),
        })
        .collect()
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
    serde_json::to_value(v).map_err(|_| internal("failed to serialize response"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use slate_store::AccountUpdateInsert;

    fn pk(first: u8) -> [u8; 32] {
        let mut a = [0u8; 32];
        a[0] = first;
        a
    }
    fn b58(first: u8) -> String {
        Pubkey::from(pk(first)).to_string()
    }

    /// getMultipleAccounts must return accounts in input order, `null` in place for ones that
    /// don't exist, and a `context.fidelities` array parallel to `value`. Needs ClickHouse up.
    /// Uses 0xF1/0xF2 (seeded) and 0xF9 (never seeded) so it doesn't collide with other fixtures.
    #[tokio::test]
    async fn get_multiple_accounts_shape() {
        let store = ClickHouseClient::new("http://localhost:8123");
        let row = |first: u8, lamports: u64| AccountUpdateInsert {
            pubkey: pk(first),
            slot: 100,
            write_version: 0,
            owner: pk(0xC0),
            lamports,
            executable: 0,
            rent_epoch: 0,
            data: Vec::new(),
        };
        store
            .insert_accounts(&[row(0xF1, 111), row(0xF2, 222)])
            .await
            .unwrap();

        let rpc = Rpc { store };
        // Middle key doesn't exist -> null in the middle of the array.
        let v = rpc
            .get_multiple_accounts(vec![b58(0xF1), b58(0xF9), b58(0xF2)], 200)
            .await
            .unwrap();

        let value = v["value"].as_array().expect("value is an array");
        assert_eq!(value.len(), 3);
        assert_eq!(value[0]["lamports"], 111);
        assert!(value[1].is_null(), "missing account is null in place");
        assert_eq!(value[2]["lamports"], 222);

        // Fidelities run parallel to value: one per requested key, same order.
        let fids = v["context"]["fidelities"]
            .as_array()
            .expect("context.fidelities is an array");
        assert_eq!(fids.len(), 3);
        assert_eq!(v["context"]["slot"], 200);
    }

    /// Paginated getProgramAccounts must thread the base58 cursor across the wire: each page's
    /// context.next_cursor feeds the next call, and the walk covers the whole set once, in pubkey
    /// order, ending when next_cursor comes back null. Owner 0x77 + accounts 0x71..0x75 are its own.
    #[tokio::test]
    async fn get_program_accounts_paginates_via_cursor() {
        let seed = ClickHouseClient::new("http://localhost:8123");
        let row = |first: u8| AccountUpdateInsert {
            pubkey: pk(first),
            slot: 100,
            write_version: 0,
            owner: pk(0x77),
            lamports: 10,
            executable: 0,
            rent_epoch: 0,
            data: Vec::new(),
        };
        seed.insert_accounts(&[row(0x71), row(0x72), row(0x73), row(0x74), row(0x75)])
            .await
            .unwrap();

        let rpc = Rpc {
            store: ClickHouseClient::new("http://localhost:8123"),
        };
        let owner = b58(0x77);

        // Walk pages of 2 through the RPC layer, threading the base58 next_cursor until it's null.
        let mut walked: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..10 {
            let v = rpc
                .get_program_accounts(owner.clone(), 200, Some(2), cursor.clone())
                .await
                .unwrap();
            for item in v["value"].as_array().expect("value is an array") {
                walked.push(item["pubkey"].as_str().unwrap().to_string());
            }
            match v["context"]["next_cursor"].as_str() {
                Some(c) => cursor = Some(c.to_string()),
                None => break, // null -> last page
            }
        }

        // The whole set, once, in pubkey (byte) order.
        assert_eq!(
            walked,
            vec![b58(0x71), b58(0x72), b58(0x73), b58(0x74), b58(0x75)]
        );
    }
}
