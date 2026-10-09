# GitHub-hosted diagnostic artifacts: removal gates

## Authorized leaf
Remove only independent GitHub Actions diagnostic uploads, preserve local captures, logs, test/coverage and deployment/release gates. Disable Docker build-record upload where applicable. Preserve required cross-job release transfers pending orchestrator review. Worktree branch: wt/github-hosted-artifacts-zero-20261009. Base: 56791ebdca24290cc27e13fe0fde7b895bdafb88.

## Gates
- [x] Source and dependency audit: read applicable instructions and enumerate exact hosted producers and consumers.
- [x] Implementation: remove complete diagnostic action references, preserve all producing commands and independent receipts, and add a regression assertion only where an existing suitable contract exists.
- [ ] Verification: inspect exact diff and run the applicable focused source contract in K3s on debian3; retain exact published source identity, real pod node, command and receipt.
- [ ] Handoff: commit and publish the isolated feature branch through publish-pr-source, report remaining release/deployment transfer blockers and keep main unchanged.

## Constraints and outcome record
The installed od-testing K3s workflow is required. Coordinator scratch stays within .throwaway-codex/github-hosted-artifacts-zero/ in this worktree. No tests or builds run on the VM. A per-run copy of the installed wrapper changes only the helper-directory resolution and explicit node from auto to debian3; the orchestrator approved this exact two-line placement adaptation. The supplied unlazy leaf template was unavailable earlier in this task; these measured gates implement the required acceptance contract.

Status: gates established before workflow edits; source and execution evidence pending.

Source review: every remaining command, runner selector, trigger, timeout, local capture, report producer and functional deployment/release handoff remains unchanged. The only functional receipt-routing amendment is ForumZone's existing structured-log fallback running unconditionally after hosted-copy removal. No shared cache, release asset or package is changed. Full .github and available scripts/tools/tests/docs searches found no automated consumer of the removed diagnostic copies. Known release transfers in quietcontext/slopgate remain explicit integration blockers. No additional source improvement is required before focused verification.
