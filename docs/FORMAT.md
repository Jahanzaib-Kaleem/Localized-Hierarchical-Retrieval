# LHR On-Disk Format (Draft v0)

This document deliberately defines concepts before freezing binary layouts.

## Requirements

- readable without loading the full dataset into RAM
- memory-map friendly
- deterministic
- language-independent
- append/rebuild strategy can evolve independently of reader API
- compact integer representations
- explicit versioning

## Dataset layout

```text
<dataset>/
  manifest.json
  dictionaries/
  canonical/
  routing/
  temp/
```

`manifest.json` records format version, row count, schema, integer widths, canonical segments and routing structures.

### dictionaries

Maps external values to deterministic integer tokens. The database engine must not require semantic knowledge of a token.

### canonical

Authoritative records. Early prototypes use dense integer matrices because they are simple to memory-map. Production layout may become columnar/segmented while preserving stable row addressing.

### routing

Contains hierarchy directories. A directory should preferentially store compact entries such as:

```text
key | start | length | child_address | signature
```

Fields may be omitted by a particular hierarchy type.

Directories with ordered keys are binary-searchable.

## Correctness invariant

Routing metadata is allowed to return a superset of matching rows, but it must never exclude a true match. Exact predicates are evaluated against canonical data before results are returned.

## Integer widths

Widths should be selected from dataset/cardinality requirements rather than defaulting everything to 64 bits. Candidate representations include uint8/uint16 dictionary values, uint32 row addresses where possible, and wider offsets only when required.

## Versioning

The first experimental format is `LHR/0`. Binary compatibility is not promised until the research format is explicitly frozen.
