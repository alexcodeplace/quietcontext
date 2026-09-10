# Native engine provenance

This Rust engine was migrated into QuietContext from the current `ft/` implementation in the Fewtok repository at commit `c5dffec` (2026-09-09 migration baseline).

- Original Fewtok-owned code was distributed under the Fewtok MIT license at that baseline.
- `src/filter/` contains code derived from RTK (Rust Token Killer), originally developed by rtk-ai under Apache License 2.0. Preserve `NOTICE` and `LICENSES/Apache-2.0.txt` when redistributing the native engine.
- Fewtok's legacy TypeScript/Bun proxy, dormant root Rust proxy workspace, and generic FTS5 knowledge-base implementation were intentionally not migrated.

The merged product remains QuietContext. `qc-native` is an internal implementation detail, not a separately branded product.
