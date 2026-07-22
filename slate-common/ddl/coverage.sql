

CREATE TABLE IF NOT EXISTS slate.coverage
(
    segment_lo UInt64,
    segment_hi UInt64
)
ENGINE = ReplacingMergeTree(segment_hi)
ORDER BY segment_lo