# Third-Party Notices

`aish` is licensed under **Apache-2.0**.

It is built on a number of open-source Rust crates. This file lists the **direct
dependencies** declared in `Cargo.toml` and their licenses. For the complete set of
third-party components (including transitive dependencies) and the full license texts,
generate the bundle with `cargo about` / `cargo bundle-licenses` (see
[`COMPLIANCE_REPORT.md`](./COMPLIANCE_REPORT.md) §4).

Every direct dependency is **permissive (Apache-2.0 OR MIT)** and fully compatible with
`aish`'s Apache 2.0 license. We gratefully acknowledge their authors and contributors.

## Direct dependencies

| Crate | License | Purpose in aish |
|---|---|---|
| [anyhow](https://github.com/dtolnay/anyhow) | Apache-2.0 OR MIT | Flexible error handling |
| [clap](https://github.com/clap-rs/clap) | Apache-2.0 OR MIT | Command-line argument parsing |
| [hf-hub](https://github.com/huggingface/hf-hub) | Apache-2.0 OR MIT | Hugging Face model/GGUF downloads (`local` feature) |
| [libc](https://github.com/rust-lang/libc) | Apache-2.0 OR MIT | Raw FFI bindings to system libraries |
| [miette](https://github.com/zkat/miette) | Apache-2.0 | Diagnostic rendering — span carets, codes, help (`src/diag.rs`) |
| [mistralrs](https://github.com/EricLBuehler/mistral.rs) | Apache-2.0 OR MIT | In-process local LLM inference (`local` feature) |
| [regex](https://github.com/rust-lang/regex) | Apache-2.0 OR MIT | Regular expressions |
| [reqwest](https://github.com/seanmonstar/reqwest) | Apache-2.0 OR MIT | HTTP client (rustls, HTTP/2) |
| [rusqlite](https://github.com/rusqlite/rusqlite) | Apache-2.0 OR MIT | SQLite bindings (bundled) |
| [rustyline](https://github.com/kkawakam/rustyline) | Apache-2.0 OR MIT | Readline-style line editing |
| [serde](https://github.com/serde-rs/serde) | Apache-2.0 OR MIT | Serialization framework |
| [serde_json](https://github.com/serde-rs/json) | Apache-2.0 OR MIT | JSON serialization |
| [sqlite-vec](https://github.com/asg017/sqlite-vec) | Apache-2.0 OR MIT | Vector search extension for SQLite |
| [thiserror](https://github.com/dtolnay/thiserror) | Apache-2.0 OR MIT | Derive macro for the `AishDiagnostic` error type |
| [tokio](https://github.com/tokio-rs/tokio) | Apache-2.0 OR MIT | Async runtime |
| [unicode-width](https://github.com/unicode-rs/unicode-width) | Apache-2.0 OR MIT | Terminal display-width of Unicode |
| [urlencoding](https://github.com/kornelski/rust_urlencoding) | Apache-2.0 OR MIT | URL percent-encoding |
| [uuid](https://github.com/uuid-rs/uuid) | Apache-2.0 OR MIT | UUID generation (v4) |

## A note on transitive dependencies

`aish`'s full dependency graph (~660 crate instances) was audited for license
compatibility. Summary:

- **No GPL / AGPL / LGPL-only or proprietary licenses.**
- The graph is overwhelmingly permissive: Apache-2.0, MIT, BSD-2/3-Clause, ISC, Zlib,
  BSL-1.0, 0BSD, Unicode-3.0, CC0, MIT-0, NCSA, and CDLA-Permissive-2.0.
- A small number of crates are licensed **MPL-2.0** (a weak, file-level copyleft) — e.g.
  the `symphonia` audio family, `cssparser`/`selectors`, and `option-ext`. These are
  consumed unmodified and impose no obligation on `aish`'s own source. See
  [`COMPLIANCE_REPORT.md`](./COMPLIANCE_REPORT.md) §3.3.

To regenerate a complete, authoritative attribution bundle:

```sh
cargo install cargo-about
cargo about generate about.hbs > licenses/THIRD_PARTY_FULL.txt
# or
cargo install cargo-bundle-licenses
cargo bundle-licenses --format yaml --output licenses/THIRD_PARTY.yaml
```
