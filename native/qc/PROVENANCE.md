# Native engine provenance

This Rust engine was migrated into QuietContext from the current `ft/` implementation in the Fewtok repository at commit `c5dffec` (2026-09-09 migration baseline).

- Fewtok-originated portions were distributed under the Fewtok MIT license at that baseline. Preserve `NOTICE` and `LICENSES/Fewtok-MIT.txt` when redistributing the native engine.
- `src/filter/` contains code derived from RTK (Rust Token Killer), originally developed by rtk-ai. At the source-time RTK commit immediately preceding Fewtok's filter import (`805caf7d069e93370a316682b36aad59d562de2e`), RTK's actual `LICENSE` file and README identify the project as Apache License 2.0. Preserve `NOTICE` and the exact upstream license snapshot in `LICENSES/Apache-2.0.txt` when redistributing the native engine.
- Fewtok's legacy TypeScript/Bun proxy, dormant root Rust proxy workspace, and generic FTS5 knowledge-base implementation were intentionally not migrated.

The merged product remains QuietContext. `qc-native` is an internal implementation detail, not a separately branded product. The notices above apply to incorporated portions; QuietContext's overall project license remains the repository root `LICENSE`.
