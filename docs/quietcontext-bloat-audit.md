# QuietContext Bloat Audit

Audience: AI coding agents and maintainers.

Runtime contract: six token-saving tools only. Any inherited component must prove it is required for those tools.

## Remove next, in order

1. Remove meta-tool registration, session analytics, statusline pricing, and lifecycle narration from QuietContext entrypoint.
2. Remove non-Codex manifests/configs from published package.
3. Remove non-Codex adapters after dedicated core entrypoint passes clean-install tests.
4. Retain only boot-critical self-healing; prove both healthy-cache and damaged-cache branches.

Stop deletion when a six-tool acceptance test fails. Restore required dependency; do not restore unrelated feature surface.
