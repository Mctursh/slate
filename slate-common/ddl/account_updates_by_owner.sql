

CREATE TABLE IF NOT EXISTS slate.account_updates_by_owner 
(
    owner          FixedString(32)  CODEC(ZSTD(1)),
    pubkey         FixedString(32)  CODEC(ZSTD(1)),
    slot           UInt64           CODEC(DoubleDelta, ZSTD(1)),
    write_version  UInt64           CODEC(DoubleDelta, ZSTD(1)),
    lamports       UInt64           CODEC(T64, ZSTD(1)),
    data_len       UInt32           CODEC(T64, ZSTD(1))
)
ENGINE = ReplacingMergeTree
PARTITION BY intDiv(slot, 432000)
ORDER BY (owner, pubkey, slot, write_version);

CREATE MATERIALIZED VIEW IF NOT EXISTS slate.account_updates_by_owner_mv TO slate.account_updates_by_owner AS 
SELECT
    owner,          
    pubkey,         
    slot,            
    write_version,  
    lamports,       
    length(data) AS data_len
FROM slate.account_updates;

