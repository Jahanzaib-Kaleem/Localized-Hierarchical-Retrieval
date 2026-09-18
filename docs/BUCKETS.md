# Buckets and database browsing

LHR buckets are independent exact dataset catalogs under one appliance. They are intended for workflows where one LHR instance stores several logically separate data collections, such as client datasets, campaigns, source lists, or processing stages.

## Compatibility model

The pre-bucket LHR catalog at `/data` is the reserved bucket with stable ID:

```text
default
```

No migration or rewrite is required. An existing installation immediately exposes its current generations, indexes, deltas, row IDs, and schema as the `default` bucket.

The display name of the default bucket can be changed in Studio or through the API/MCP. Its stable ID remains `default` so older clients that omit a bucket continue to address the same data.

Named buckets live under:

```text
/data/buckets/<bucket-id>
```

Each named bucket is its own LHR catalog with its own `CURRENT`, immutable generations, indexes, deltas, visibility map, compaction history, recovery state, and workload telemetry.

## Bucket IDs

Named bucket IDs are stable path-safe identifiers:

- 1 to 64 characters;
- lowercase letters, digits, `-`, and `_`;
- first character must be a letter or digit;
- `default` is reserved.

The display name is independent from the ID and can be renamed without rewriting data.

## Studio

The Data workspace provides:

- bucket creation, selection, display-name changes, and deletion;
- the existing dataset automatically visible in the default bucket;
- a database-style row browser with 50 rows per page;
- stable cursor pagination rather than offset/prefix replay;
- CSV import into the active bucket;
- selected-row copy or move between buckets; an empty destination is initialized from the source schema, while populated destinations must have an identical schema;
- whole-bucket combine into a new bucket using a streaming bounded-memory build;
- schema and storage information for the active bucket.

A row transfer can initialize a newly created empty bucket from the selected source rows. A row move is copy-first: the destination commit happens before source deletion. If source deletion fails, LHR reports the warning and leaves the destination copy intact rather than risking data loss.

## API selection

Bucket-aware endpoints accept a bucket ID. Omitting it keeps backward compatibility with `default`.

Query:

```json
{
  "bucket": "prospects",
  "filters": [],
  "limit": 50,
  "after_row_id": null
}
```

An empty `filters` array is the exact table-browse operation. It pages forward by stable logical row ID and does not replay prior pages.

Stats:

```text
GET /v1/stats?bucket=prospects
```

CSV import includes a multipart `bucket` field. Mutations, compaction, vacuum, recovery, and index administration also accept a bucket.

Bucket management endpoints:

```text
GET  /v1/buckets
POST /v1/admin/buckets/create
POST /v1/admin/buckets/rename
POST /v1/admin/buckets/delete
POST /v1/admin/buckets/combine
POST /v1/buckets/transfer
```

Deleting the reserved default bucket is not allowed.

## Combine semantics

Combining buckets creates a new target bucket and leaves each source bucket unchanged. Source schemas must match exactly. Rows are streamed through a temporary CSV into the existing bounded-memory importer, so memory does not scale with the combined row count.

## Empty buckets

A newly created named bucket can exist before it has a dataset. Importing its first CSV initializes its first generation. Query, mutation, index, and row-transfer destinations require a published dataset unless the operation itself creates the dataset (for example CSV import or bucket combine).

## Storage and upgrades

All buckets remain below the persistent `/data` mount. Replacing the application container therefore preserves the default catalog and all named buckets. Software upgrades never delete the Docker volume.
