# License Compliance Report — aish

**Project:** `aish` v0.11.0
**Project license:** `Apache-2.0`
**Analysis basis:** `cargo license` over the full dependency graph (`Cargo.lock`), default + `local` features enabled.
**Date:** generated from the committed `Cargo.lock`.

## 1. Overview

`aish` is licensed under **Apache-2.0**. This report evaluates every crate in the
resolved dependency tree (direct **and** transitive — ~660 crate instances across the
graph) for compatibility with **Apache-2.0** as the outbound license.

**Headline result: the dependency tree is clean.**

- **No GPL, AGPL, or LGPL-only licenses.** (LGPL appears *only* inside `OR` clauses that
  also offer Apache-2.0/MIT — we take the permissive side.)
- **No proprietary, commercial, or source-restricted licenses.**
- **No "viral"/strong-copyleft obligations** are imposed on `aish`'s own source.
- Every dependency is either a permissive license (Apache-2.0, MIT, BSD-2/3-Clause, ISC,
  Zlib, BSL-1.0, 0BSD, Unicode-3.0, CC0, MIT-0, NCSA, CDLA-Permissive-2.0) or **MPL-2.0**,
  a *weak, file-level* copyleft that is fully compatible with shipping an Apache-2.0 binary.

The only items needing **documentation / attribution handling** (not remediation) are:

1. **MPL-2.0** crates (weak copyleft — file-level source-availability obligation).
2. **Apache-2.0** crates, which require carrying their `NOTICE` text where present.
3. Permissive licenses (MIT/BSD/ISC/Zlib/…) whose text must be reproduced in distributions.

## 2. License compatibility summary

| License (SPDX) | # crates | Compatible with Apache-2.0 | Type | Required action |
|---|---:|---|---|---|
| Apache-2.0 OR MIT (and BSD/Zlib/ISC `OR` variants) | ~390 | ✅ Yes | Permissive (choose a side) | Reproduce license text |
| Apache-2.0 (only) | ~20 | ✅ Yes | Permissive | Reproduce license + any `NOTICE` |
| MIT (only) | ~155 | ✅ Yes | Permissive | Reproduce MIT text + copyright |
| MIT OR Unlicense | 9 | ✅ Yes | Permissive | Reproduce (choose a side) |
| BSD-2-Clause / BSD-3-Clause | ~10 | ✅ Yes | Permissive | Reproduce text + copyright |
| ISC | 4 | ✅ Yes | Permissive | Reproduce text |
| Zlib / 0BSD / BSL-1.0 | ~6 | ✅ Yes | Permissive | Reproduce text (0BSD/BSL: no attribution required) |
| Unicode-3.0 | 18 | ✅ Yes | Permissive (ICU4X) | Reproduce text |
| CDLA-Permissive-2.0 | 3 | ✅ Yes | Permissive data license (webpki-roots) | Reproduce text |
| `… OR LGPL-2.1-or-later OR …` | 2 (`r-efi`) | ✅ Yes (take Apache/MIT) | Copyleft offered in OR-clause | Elect non-LGPL term; document choice |
| **MPL-2.0** | **16** | ✅ **Yes** | **Weak (file-level) copyleft** | **Document; preserve MPL files' source availability** |

> Crate counts are approximate because the SDPX `OR`/`AND` groupings overlap; the
> compatibility *conclusion* is exact: **all are compatible.**

## 3. Problematic / attention-worthy dependencies

### 3.1 GPL / AGPL — **NONE**
No crate in the graph is licensed GPL-2.0, GPL-3.0, AGPL, or any strong-copyleft license
(neither standalone nor inside an `OR` clause). No mitigation required.

### 3.2 Proprietary / restrictive — **NONE**
No `license_file`-only crates with bespoke restrictive terms, and no commercial/proprietary
SPDX identifiers were found.

### 3.3 MPL-2.0 (weak copyleft) — **document, do not remediate**

MPL-2.0 is **file-level** copyleft: obligations attach only to the *MPL-licensed files
themselves*, not to `aish`. Because `aish` consumes these crates unmodified as Cargo
dependencies and does not copy MPL source into its own files, the only obligation is to
**preserve the MPL notice and make the (unmodified) MPL source available** — which Cargo +
the public crates.io/GitHub sources already satisfy.

| Crate | Role in aish |
|---|---|
| `symphonia`, `symphonia-bundle-flac`, `symphonia-bundle-mp3`, `symphonia-codec-pcm`, `symphonia-codec-vorbis`, `symphonia-core`, `symphonia-format-isomp4`, `symphonia-format-ogg`, `symphonia-format-riff`, `symphonia-metadata`, `symphonia-utils-xiph` | Audio decoding (pulled in via `mistralrs` audio, `local` feature) |
| `cssparser`, `cssparser-macros`, `selectors`, `dtoa-short` | CSS/HTML parsing (via `scraper`, used for HTML→text) |
| `option-ext` | Small helper (via `dirs`) |

**Mitigation / handling:**
- Keep these crates **unmodified** (consume from crates.io). If you ever vendor & patch an
  MPL file, that *modified file* must be published under MPL-2.0.
- List them in `THIRD_PARTY_NOTICES.md` with the MPL-2.0 designation and a pointer to source.
- If a build *without* MPL deps is ever desired, `symphonia` is reachable only through the
  optional `local` (mistral.rs) feature — `cargo build --no-default-features` drops the
  audio stack; `cssparser`/`selectors` come via `scraper`.

### 3.4 `OR LGPL` crates — elect the permissive term
`r-efi` is offered as `Apache-2.0 OR LGPL-2.1-or-later OR MIT`. We **elect Apache-2.0 (or
MIT)**; no LGPL obligation attaches. Documented here for the record.

## 4. Recommended NOTICE / ATTRIBUTION structure

The repo already ships `LICENSE-APACHE` and `LICENSE-MIT` (good). Recommended additions:

```
/
├── LICENSE-APACHE                 # (exists) outbound Apache-2.0
├── LICENSE-MIT                    # (exists) outbound MIT
├── NOTICE                         # NEW — top-level attribution pointer (Apache-2.0 §4(d))
├── THIRD_PARTY_NOTICES.md         # NEW — direct deps, user-friendly (this PR)
└── licenses/                      # OPTIONAL — full third-party license texts
    ├── THIRD_PARTY_FULL.txt       #   generated: `cargo about generate` or `cargo bundle-licenses`
    └── <per-license texts>        #   MPL-2.0.txt, Unicode-3.0.txt, BSD-3-Clause.txt, …
```

**`NOTICE` file (suggested top-level content):**

```
aish
Copyright (c) The aish authors

This product is licensed under MIT OR Apache-2.0.

This product bundles third-party software. See THIRD_PARTY_NOTICES.md for
direct-dependency attributions, and licenses/THIRD_PARTY_FULL.txt for the
complete set of third-party license texts (generated from Cargo.lock).

Portions of this software are licensed under the Mozilla Public License 2.0
(MPL-2.0). The source for those components is available unmodified from
crates.io / their upstream repositories.
```

**Tooling to generate the full bundle (CI-friendly):**
- `cargo install cargo-about` → `cargo about generate about.hbs > licenses/THIRD_PARTY_FULL.txt`
- or `cargo install cargo-bundle-licenses` → `cargo bundle-licenses --format yaml --output licenses/THIRD_PARTY.yaml`
- Add a CI check (`cargo deny check licenses`) to fail the build if a future dependency
  introduces a disallowed license.

## 5. Action items

| # | Action | Priority | Owner | Status |
|---|---|---|---|---|
| 1 | Land `THIRD_PARTY_NOTICES.md` (direct deps) | High | maintainers | ✅ done |
| 2 | Add top-level `NOTICE` file (template in §4) | Medium | maintainers | ✅ done |
| 3 | Generate `licenses/THIRD_PARTY_FULL.txt` via `cargo about`/`cargo bundle-licenses` | Medium | maintainers | ☐ future (optional) |
| 4 | List MPL-2.0 crates with source pointers in notices | Medium | maintainers | ✅ §3.3 |
| 5 | Add `deny.toml` + `cargo deny check licenses` to CI (allowlist permissive + MPL-2.0) | Medium | CI | ✅ done |
| 6 | Keep MPL-2.0 crates unmodified (no vendored patches) | Ongoing | all | ✅ policy |
| 7 | Re-run this analysis on any `Cargo.lock` change (CI gate) | Low | CI | ☐ future (optional) |

## 6. Conclusion

**`aish` can be distributed under Apache-2.0 (or MIT) with no license conflicts.** There are
no GPL/AGPL/LGPL-only or proprietary dependencies. The dependency tree is overwhelmingly
permissive; the single weak-copyleft family (MPL-2.0) imposes only a file-level
source-availability obligation that is satisfied by consuming the crates unmodified and
documenting them. The remaining work is **attribution hygiene** (NOTICE + bundled license
texts + a CI license gate), not remediation.
