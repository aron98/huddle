# ADR 004: Single static binary with embedded UI; SQLite as the only store

- Status: accepted
- Date: 2026-10-13

## Context

The deployment story is the product: "one file + one SQLite file + coturn" is
what separates huddle from the JVM-heavy incumbents. Two decisions fall out:

1. Serve the compiled WASM UI from the same binary or from a separate web tier?
2. SQLite, Postgres, or a storage trait with multiple backends?

## Decision

1. The web UI (Leptos build output) is embedded into the `server` binary with
   `rust-embed`, with an optional filesystem-override flag for development.
2. **SQLite via `sqlx` is the only storage backend** for the foreseeable future.

## Consequences

**Positive**
- `scp huddle user@vps && ./huddle` is genuinely the whole install; no Nginx,
  no Node, no second container. Docker-compose remains for those who want it.
- Zero-database-ops: the SQLite file is trivially backed up by `cp` while the
  server is stopped (or via litestream for hot backups — document, don't build).
- The binary doubles as the container entrypoint on AWS/Fargate and in the Helm
  chart; one artifact for all deploy targets.

**Negative**
- Embedded UI means the binary must be rebuilt (or the override dir used) to ship
  frontend changes — acceptable; releases bundle both anyway.
- SQLite caps write concurrency — irrelevant at our scale (scheduling writes are
  tiny; chat persistence is low-rate). WAL mode enabled.
- If a hosted/multi-tenant variant ever appears, Postgres support becomes a
  `sqlx`-transparent swap; do not build the abstraction before it's needed.
