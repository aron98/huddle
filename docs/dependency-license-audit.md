# Dependency license audit — Apache-2.0 relicense (issue #39)

- Date: 2026-10-15
- Audited by: parsing the `license` / `license-files` fields of every
  registry dependency's `Cargo.toml` in the local cargo registry cache, for
  all packages resolved in `Cargo.lock` (411 registry packages; workspace
  members excluded).

## Result

**No dependency blocks relicensing huddle to Apache-2.0.** There are **no
GPL, LGPL, or AGPL dependencies**. Every dependency is permissive and
Apache-2.0-compatible, either as a single permissive license or a choice
(`OR`) that includes Apache-2.0 / MIT terms.

## License distribution (411 packages)

| License (SPDX expression) | Count |
|---|---|
| `MIT OR Apache-2.0` / `Apache-2.0 OR MIT` / `MIT/Apache-2.0` (equivalent dual) | 283 |
| `MIT` | 87 |
| `Apache-2.0` | 2 |
| `Unicode-3.0` | 18 |
| `Zlib` | 6 |
| `ISC` (incl. `Apache-2.0 AND ISC`, `ISC AND (Apache-2.0 OR ISC)`) | 8 |
| `Unlicense OR MIT` / `Unlicense/MIT` | 6 |
| `BSD-2-Clause OR Apache-2.0 OR MIT`, `BSD-3-Clause`, `MIT AND BSD-3-Clause` | 4 |
| `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | 3 |
| Others (see below) | 14 |

## Notable non-dual-MIT/Apache licenses (all permissive, none blocking)

| Dependency | License | Notes |
|---|---|---|
| `ring` 0.17 | `Apache-2.0 AND ISC` | Both permissive. |
| `rustls`, `hyper-rustls`, `rustls-webpki` | Apache-2.0 OR ISC OR MIT / ISC | Permissive TLS stack. |
| `aws-lc-rs`, `aws-lc-sys` | ISC / BSD-style composite | Permissive; parts of aws-lc are also covered by its own ISC-style notices. |
| `webpki-roots` | `CDLA-Permissive-2.0` | Data license for the bundled root store; permissive, commercial use allowed. |
| `unicode-ident`, `icu_*`, `litemap`, `tinystr`, `yoke`, `zerovec` (+ siblings) | `Unicode-3.0` (sometimes `AND`-ed) | Unicode License v3, permissive, GPL-compatible and Apache-2.0-compatible. |
| `r-efi` | `MIT OR Apache-2.0 OR LGPL-2.1-or-later` | `OR` expression — LGPL is only one choice; compliance via the MIT or Apache-2.0 terms. Not an LGPL dependency. |
| `const_format`, `konst`, `foldhash`, `slotmap` | `Zlib` | Permissive. |
| `subtle` | `BSD-3-Clause` | Permissive. |
| `xxhash-rust` | `BSL-1.0` | Boost Software License, permissive. |
| `base16` | `CC0-1.0` | Public domain dedication; fine for code we use (not required to Apache-2.0-ify it). |
| `dunce` | `CC0-1.0 OR MIT-0 OR Apache-2.0` | Permissive. |
| `wasi`, `wasip2`, `wit-bindgen` | `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | Permissive. |
| `quoted_printable` | `0BSD` | Permissive. |
| `aho-corasick`, `memchr`, `byteorder`, `same-file`, `walkdir`, `winapi-util` | `Unlicense OR MIT` | Permissive. |
| `zerocopy` (+ derive) | `BSD-2-Clause OR Apache-2.0 OR MIT` | Permissive. |
| `matchit` | `MIT AND BSD-3-Clause` | Permissive. |
| `whoami`, `ryu` | Apache-2.0 OR BSL-1.0 (OR MIT) | Permissive. |
| `simple_asn1`, `untrusted` | ISC | Permissive. |

## Vendored / embedded code

`rust-embed` bakes the trunk build output of `crates/web` plus the first-party
fallback `crates/server/assets/index.html` into the release binary. All
embedded content is first-party huddle code — no third-party notices need to be
aggregated, and no `NOTICE` file is required today (see ADR 005).

## Ongoing compliance

- MIT, ISC, Zlib, BSD, Unicode-3.0, BSL-1.0, CC0-1.0, 0BSD, Unlicense, and
  CDLA-Permissive-2.0 all permit inclusion and redistribution in an
  Apache-2.0-licensed project.
- When an Apache-2.0-licensed dependency requires it (e.g. dependencies with
  `NOTICE` files of their own), standard per-dependency compliance applies to
  binary distributions; nothing in the current tree requires a project-level
  `NOTICE` file.
- Re-run this audit when adding dependencies; treat any GPL/AGPL/LGPL-*
  **only** license as a blocker requiring discussion.
