# Direct Parquet Import

LHR can import existing flat primitive Parquet shards directly into hybrid canonical storage without materializing a CSV copy.

## Guarantees

- Source Parquet files are read-only and remain at their original paths.
- No decoded CSV staging file is created.
- Pass 1 builds LHR dictionaries with bounded external-sort runs.
- Pass 2 streams dictionary tokens directly into native hot segments and cold Parquet sidecars.
- Every logical column still receives an exact singleton LHR index.
- Additional multi-column accelerators can be supplied with `--index`.
- Fully indexed equality filtering remains an LHR-index operation; cold Parquet is only read when a cold value must be materialized or a fallback canonical read is required.
- The published generation is verified by default.
- The command prints final `canonical_bytes`, `routing_bytes`, and `total_bytes`, so a real import immediately shows its actual disk footprint.

## Source requirements

The source files must have the same flat primitive schema and column names as the supplied `LHR-SCHEMA/1` file. Nested/list/map columns are rejected rather than silently flattened. Binary values used as text must be valid UTF-8.

All supplied shard paths are sorted internally before both passes, so a shell glob such as `/apollo/apollo_data_*.parquet` is deterministic.

## Hybrid profile

A profile chooses columns that stay in the mmap-native hot store. Every other column is stored as compressed Parquet dictionary-token data.

Example `apollo-profile.json`:

```json
{
  "hot_columns": [
    "email",
    "first_name",
    "last_name",
    "title",
    "company_name",
    "domain",
    "country",
    "state",
    "city",
    "industry",
    "seniority"
  ],
  "row_group_rows": 65536
}
```

The profile may instead contain `cold_columns`, but not both. At least one column must remain hot and at least one must be cold.

## Import

Inside the LHR appliance image:

```bash
lhr-parquet-import \
  --root /data/buckets/apollo \
  --schema /root/apollo-schema.json \
  --profile /root/apollo-profile.json \
  /apollo/apollo_data_*.parquet
```

Additional exact multi-column indexes can be added during import:

```bash
lhr-parquet-import \
  --root /data/buckets/apollo \
  --schema /root/apollo-schema.json \
  --profile /root/apollo-profile.json \
  --index country,title \
  --index domain,seniority \
  /apollo/apollo_data_*.parquet
```

The command refuses to replace a non-empty catalog. It prints the import report, final storage report, and post-publish verification report as JSON. A failed build abandons the staged generation and leaves the existing catalog and source shards unchanged.

## Compaction

`lhr compact` now rebuilds dictionaries and canonical storage directly from visible LHR rows. It no longer writes a full temporary CSV. Hybrid generations preserve their existing cold-column profile and row-group size. Newly evolved columns default to hot storage during compaction.

## Operational notes

The importer is intentionally two-pass. The source dataset is decoded twice, but disk usage stays bounded: source Parquet + dictionary-sort runs + the staged LHR generation. This is preferable to expanding a compressed Parquet dataset into a much larger CSV before LHR can begin building.

For very large imports, place the LHR catalog and its staging generation on a filesystem with enough free space for the final generation plus temporary dictionary runs. The source Parquet files themselves are never copied into a separate decoded staging format.
