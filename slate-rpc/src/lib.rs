use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
    types::ErrorObject,
};
use serde::Deserialize;
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

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct AccountConfig {
    pub as_of_slot: Option<u64>,
    // standard Solana request fields: accepted so Solana clients work, not acted on yet
    pub commitment: Option<serde_json::Value>,
    pub encoding: Option<serde_json::Value>,
    pub data_slice: Option<serde_json::Value>,
    pub min_context_slot: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct ProgramAccountsConfig {
    pub as_of_slot: Option<u64>,
    pub limit: Option<u64>,
    pub cursor: Option<String>,
    // standard Solana request fields: accepted so Solana clients work, not acted on yet
    pub commitment: Option<serde_json::Value>,
    pub encoding: Option<serde_json::Value>,
    pub data_slice: Option<serde_json::Value>,
    pub min_context_slot: Option<serde_json::Value>,
    pub filters: Option<serde_json::Value>,
    pub with_context: Option<serde_json::Value>,
}

#[rpc(server)]
pub trait SlateRpc {
    #[method(name = "getAccountInfo")]
    async fn get_account_info(
        &self,
        pubkey: String,
        config: Option<AccountConfig>,
    ) -> RpcResult<serde_json::Value>;

    #[method(name = "getProgramAccounts")]
    async fn get_program_accounts(
        &self,
        owner: String,
        config: Option<ProgramAccountsConfig>,
    ) -> RpcResult<serde_json::Value>;

    #[method(name = "getBalance")]
    async fn get_balance(
        &self,
        pubkey: String,
        config: Option<AccountConfig>,
    ) -> RpcResult<serde_json::Value>;

    #[method(name = "getMultipleAccounts")]
    async fn get_multiple_accounts(
        &self,
        pubkeys: Vec<String>,
        config: Option<AccountConfig>,
    ) -> RpcResult<serde_json::Value>;

    #[method(name = "getCoverage")]
    async fn get_coverage(&self) -> RpcResult<serde_json::Value>;

    #[method(name = "getFirstAvailableSlot")]
    async fn get_first_available_slot(&self) -> RpcResult<serde_json::Value>;
}

pub struct Rpc {
    pub store: ClickHouseClient,
}

impl Rpc {
    async fn resolve_slot(&self, as_of_slot: Option<u64>) -> RpcResult<u64> {
        match as_of_slot {
            Some(slot) => Ok(slot),
            // no captured slot yet: error like getFirstAvailableSlot instead of fabricating slot 0
            None => self
                .store
                .latest_covered()
                .await
                .map_err(|_| internal("failed to query coverage"))?
                .ok_or_else(no_coverage),
        }
    }
}

#[async_trait]
impl SlateRpcServer for Rpc {
    async fn get_account_info(
        &self,
        pubkey: String,
        config: Option<AccountConfig>,
    ) -> RpcResult<serde_json::Value> {
        let key = decode_pubkey(pubkey)?;
        let as_of_slot = self
            .resolve_slot(config.unwrap_or_default().as_of_slot)
            .await?;

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
        config: Option<ProgramAccountsConfig>,
    ) -> RpcResult<serde_json::Value> {
        let key = decode_pubkey(owner)?;
        let config = config.unwrap_or_default();
        let as_of_slot = self.resolve_slot(config.as_of_slot).await?;

        // cursor is only honored with limit; without a limit we return the full unpaginated set
        let Some(limit) = config.limit else {
            let ProgramAccountAtSlot { accounts, fidelity } = self
                .store
                .get_program_accounts_as_of(&key, as_of_slot)
                .await
                .map_err(|_| internal("failed to query program accounts"))?;
            return respond(as_of_slot, keyed(accounts), fidelity);
        };

        let cursor = config.cursor.map(decode_pubkey).transpose()?;
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
        response["context"]["nextCursor"] =
            to_value(next_cursor.map(|c| Pubkey::from(c).to_string()))?;
        Ok(response)
    }

    async fn get_multiple_accounts(
        &self,
        pubkeys: Vec<String>,
        config: Option<AccountConfig>,
    ) -> RpcResult<serde_json::Value> {
        let as_of_slot = self
            .resolve_slot(config.unwrap_or_default().as_of_slot)
            .await?;
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

    async fn get_balance(
        &self,
        pubkey: String,
        config: Option<AccountConfig>,
    ) -> RpcResult<serde_json::Value> {
        let key = decode_pubkey(pubkey)?;
        let as_of_slot = self
            .resolve_slot(config.unwrap_or_default().as_of_slot)
            .await?;
        let AccountAtSlot { account, fidelity } = self
            .store
            .get_account_info_as_of(&key, as_of_slot)
            .await
            .map_err(|_| internal("failed to query account store"))?;

        let value = account.map(|a| a.lamports).unwrap_or(0);
        respond(as_of_slot, value, fidelity)
    }

    async fn get_coverage(&self) -> RpcResult<serde_json::Value> {
        let segments = self
            .store
            .coverage_segments()
            .await
            .map_err(|_| internal("failed to query coverage"))?;
        let segments: Vec<serde_json::Value> = segments
            .iter()
            .map(|s| serde_json::json!({ "firstSlot": s.first_slot, "lastSlot": s.last_slot }))
            .collect();
        Ok(serde_json::json!({ "segments": segments }))
    }

    async fn get_first_available_slot(&self) -> RpcResult<serde_json::Value> {
        let floor = self
            .store
            .earliest_covered()
            .await
            .map_err(|_| internal("failed to query coverage"))?;
        match floor {
            Some(slot) => Ok(serde_json::json!(slot)),
            None => Err(no_coverage()),
        }
    }
}

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

fn no_coverage() -> ErrorObject<'static> {
    ErrorObject::owned(-32000, "no coverage recorded yet", None::<()>)
}

fn keyed(accounts: Vec<AccountUpdate>) -> Vec<RpcKeyedAccount> {
    accounts
        .into_iter()
        .map(|a| RpcKeyedAccount {
            pubkey: Pubkey::from(a.pubkey).to_string(),
            account: encode(&a),
        })
        .collect()
}

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

    #[tokio::test]
    async fn get_multiple_accounts_shape() {
        let store = ClickHouseClient::with_database("http://localhost:8123", "slate_test");
        let row = |first: u8, lamports: u64| AccountUpdateInsert {
            pubkey: pk(first),
            slot: 100,
            write_version: 0,
            owner: pk(0xC0),
            lamports,
            executable: 0,
            rent_epoch: 0,
            data: Vec::new(),
            txn_signature: None,
        };
        store
            .insert_accounts(&[row(0xF1, 111), row(0xF2, 222)])
            .await
            .unwrap();

        let rpc = Rpc { store };
        let v = rpc
            .get_multiple_accounts(
                vec![b58(0xF1), b58(0xF9), b58(0xF2)],
                Some(AccountConfig {
                    as_of_slot: Some(200),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();

        let value = v["value"].as_array().expect("value is an array");
        assert_eq!(value.len(), 3);
        assert_eq!(value[0]["lamports"], 111);
        assert!(value[1].is_null(), "missing account is null in place");
        assert_eq!(value[2]["lamports"], 222);

        let fids = v["context"]["fidelities"]
            .as_array()
            .expect("context.fidelities is an array");
        assert_eq!(fids.len(), 3);
        for f in fids {
            let s = f.as_str().expect("fidelity is a string");
            assert!(
                matches!(s, "exact" | "uncertain"),
                "fidelity serializes to a lowercase wire value, got {s:?}"
            );
        }
        assert_eq!(v["context"]["slot"], 200);
    }

    #[tokio::test]
    async fn get_program_accounts_paginates_via_cursor() {
        let seed = ClickHouseClient::with_database("http://localhost:8123", "slate_test");
        let row = |first: u8| AccountUpdateInsert {
            pubkey: pk(first),
            slot: 100,
            write_version: 0,
            owner: pk(0x77),
            lamports: 10,
            executable: 0,
            rent_epoch: 0,
            data: Vec::new(),
            txn_signature: None,
        };
        seed.insert_accounts(&[row(0x71), row(0x72), row(0x73), row(0x74), row(0x75)])
            .await
            .unwrap();

        let rpc = Rpc {
            store: ClickHouseClient::with_database("http://localhost:8123", "slate_test"),
        };
        let owner = b58(0x77);

        let mut walked: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..10 {
            let v = rpc
                .get_program_accounts(
                    owner.clone(),
                    Some(ProgramAccountsConfig {
                        as_of_slot: Some(200),
                        limit: Some(2),
                        cursor: cursor.clone(),
                        ..Default::default()
                    }),
                )
                .await
                .unwrap();
            for item in v["value"].as_array().expect("value is an array") {
                walked.push(item["pubkey"].as_str().unwrap().to_string());
            }
            match v["context"]["nextCursor"].as_str() {
                Some(c) => cursor = Some(c.to_string()),
                None => break,
            }
        }

        assert_eq!(
            walked,
            vec![b58(0x71), b58(0x72), b58(0x73), b58(0x74), b58(0x75)]
        );
    }

    #[tokio::test]
    async fn get_coverage_lists_recorded_segments() {
        let store = ClickHouseClient::with_database("http://localhost:8123", "slate_test");
        // High, disjoint slots so they don't collide with other tests' coverage rows.
        store.record_coverage(9_000_010, 9_000_020).await.unwrap();
        store.record_coverage(9_000_100, 9_000_150).await.unwrap();

        let rpc = Rpc { store };
        let v = rpc.get_coverage().await.unwrap();
        let segs = v["segments"].as_array().expect("segments is an array");

        let has = |lo: u64, hi: u64| {
            segs.iter()
                .any(|s| s["firstSlot"] == lo && s["lastSlot"] == hi)
        };
        assert!(has(9_000_010, 9_000_020), "first segment present");
        assert!(has(9_000_100, 9_000_150), "second segment present");
    }

    #[test]
    fn account_config_rejects_typos_but_tolerates_solana_fields() {
        use serde_json::{from_value, json};
        // a misspelled asOfSlot is rejected rather than silently returning latest
        assert!(from_value::<AccountConfig>(json!({ "asOfSlots": 5 })).is_err());
        // asOfSlot itself parses
        let c: AccountConfig = from_value(json!({ "asOfSlot": 5 })).unwrap();
        assert_eq!(c.as_of_slot, Some(5));
        // the fields a Solana client (e.g. web3.js) sends are accepted, not rejected
        let c: AccountConfig = from_value(json!({
            "asOfSlot": 5,
            "commitment": "finalized",
            "encoding": "base64",
            "dataSlice": { "offset": 0, "length": 8 },
            "minContextSlot": 10
        }))
        .unwrap();
        assert_eq!(c.as_of_slot, Some(5));
    }

    #[test]
    fn program_accounts_config_rejects_typos_but_tolerates_solana_fields() {
        use serde_json::{from_value, json};
        assert!(from_value::<ProgramAccountsConfig>(json!({ "limitt": 2 })).is_err());
        let c: ProgramAccountsConfig = from_value(json!({
            "asOfSlot": 5,
            "limit": 2,
            "cursor": "abc",
            "commitment": "finalized",
            "encoding": "base64",
            "filters": [{ "dataSize": 165 }],
            "withContext": true,
            "minContextSlot": 10
        }))
        .unwrap();
        assert_eq!(c.limit, Some(2));
    }
}
