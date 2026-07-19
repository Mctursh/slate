

CREATE TABLE IF NOT EXISTS slate.account_updates 
(
    pubkey         FixedString(32)  CODEC(ZSTD(1)),
    slot           UInt64           CODEC(DoubleDelta, ZSTD(1)),
    write_version  UInt64           CODEC(DoubleDelta, ZSTD(1)),
    owner          FixedString(32)  CODEC(ZSTD(1)),
    lamports       UInt64           CODEC(T64, ZSTD(1)),
    executable     UInt8            CODEC(ZSTD(1)),
    rent_epoch     UInt64           CODEC(T64, ZSTD(1)),
    data           String           CODEC(ZSTD(3)),
    data_len       UInt32 MATERIALIZED length(data) CODEC(T64, ZSTD(1)),
    txn_signature  Nullable(FixedString(64)) DEFAULT NULL CODEC(ZSTD(1))
)
ENGINE = ReplacingMergeTree
PARTITION BY intDiv(slot, 432000)
ORDER BY (pubkey, slot, write_version)

