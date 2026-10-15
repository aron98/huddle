# ADR 005: Relicense to Apache-2.0; no per-file SPDX headers

- Status: accepted
- Date: 2026-10-15

## Context

huddle was MIT-licensed. Issue #39 asks for a relicense to Apache License 2.0,
which brings an explicit patent grant — relevant for a project shipping a
WebRTC media server — and better familiarity for corporate adopters. The same
change forces a second decision: whether source files carry per-file
`// SPDX-License-Identifier: Apache-2.0` headers or whether the top-level
`LICENSE` file alone defines the project license.

A dependency license audit (recorded in
[`docs/dependency-license-audit.md`](../dependency-license-audit.md)) confirmed
that no dependency's license blocks the relicense.

## Decision

1. The project relicenses from MIT to **Apache License 2.0**. The top-level
   `LICENSE` file contains the full license text with the standard appendix
   copyright notice (`Copyright (c) 2026 aron98`).
2. **No per-file SPDX headers.** Source files stay headerless; the top-level
   `LICENSE` file is the single authoritative license statement, and
   `license = "Apache-2.0"` in `Cargo.toml` makes tooling (crates.io, cargo)
   carry the declaration.

## Consequences

**Positive**
- No mass churn across ~every source file; diffs stay reviewable and `git
  blame` history stays useful.
- One place to update if the license ever changes again; no risk of headers
  and `LICENSE` drifting apart.
- The patent grant and §4 notice mechanics of Apache-2.0 are clear to adopters
  evaluating a self-hosted binary.

**Negative**
- Individual files copied out of the repo don't carry their license inline —
  mitigated by the standard practice of distributing the repo/archive with its
  `LICENSE`, which is what binary releases already do.
- Some corporate checklists ask for SPDX headers; contributors can add them to
  new files if they care, but the project does not require it.

## Notes

- The Apache-2.0 appendix in `LICENSE` is the project's own copyright notice;
  there is no vendored third-party code, so no aggregated third-party notices
  (no `NOTICE` file) are required today. If third-party code is ever vendored
  into the tree or baked into the embedded UI, its license must be
  Apache-2.0-compatible and its notices preserved (appendix or `NOTICE`).
- MIT dependencies remain fine under Apache-2.0 terms; the audit doc lists the
  permissive-but-nonstandard licenses (Unicode-3.0, ISC, Zlib, BSL-1.0,
  CDLA-Permissive-2.0, CC0-1.0, 0BSD, Unlicense, BSD) and why none blocks.
