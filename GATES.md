# GitHub-hosted diagnostic artifacts: removal gates

## Authorized leaf
Remove only independent GitHub Actions diagnostic uploads, preserve local captures, logs, test/coverage and deployment/release gates. Disable Docker build-record upload where applicable. Preserve required cross-job release transfers pending orchestrator review. Worktree branch: wt/github-hosted-artifacts-zero-20261009. Base: 56791ebdca24290cc27e13fe0fde7b895bdafb88.

## Gates
- [x] Source and dependency audit: read applicable instructions and enumerate exact hosted producers and consumers.
- [x] Implementation: remove complete diagnostic action references, preserve all producing commands and independent receipts, and add a regression assertion only where an existing suitable contract exists.
- [x] Source verification: inspect the exact diff and git diff --check, retain published source identity, and report applicable focused/runtime verification accurately below.
- [x] Handoff: commit and publish the isolated feature branch through publish-pr-source, report remaining release/deployment transfer blockers and keep main unchanged.

## Constraints and outcome record
The installed od-testing K3s workflow is required. Coordinator scratch stays within .throwaway-codex/github-hosted-artifacts-zero/ in this worktree. No tests or builds run on the VM. A per-run copy of the installed wrapper changes only the helper-directory resolution and explicit node from auto to debian3; the orchestrator approved this exact two-line placement adaptation. The supplied unlazy leaf template was unavailable earlier in this task; these measured gates implement the required acceptance contract.

Status: source verification and publication completed; merge approval granted. Actual landing status is recorded below.

Source review: every remaining command, runner selector, trigger, timeout, local capture, report producer and functional deployment/release handoff remains unchanged. The only functional receipt-routing amendment is ForumZone's existing structured-log fallback running unconditionally after hosted-copy removal. No shared cache, release asset or package is changed. Full .github and available scripts/tools/tests/docs searches found no automated consumer of the removed diagnostic copies. Known release transfers in quietcontext/slopgate remain explicit integration blockers. No additional source improvement is required before focused verification.

## Verified checkpoint

Verification passed by exact source review and git diff --check for c7028424b0cf79f92421f4365b48be9d2662442b. This leaf changes only complete diagnostic YAML steps (plus pdf2html's explicit Docker build-record opt-out) and this gate record. No matching existing focused source contract was found, so no application build, browser suite, or new mirror test was run for this reversible change. This is static verification, not a claim of K3s runtime acceptance. The unchanged project landing checks remain required.

Publication: the supported publish-pr-source helper pushed and verified this exact implementation SHA on the feature branch. Main, releases, package publication and account settings were untouched. Source audit and verification-summary.json are retained in the ignored task directory. Owner later approved the merge rollout; this source checkpoint does not claim successful default-branch integration or deployment.

## Merge approval and actual landing gate, 2026-10-09

Merge approval was explicitly granted on 2026-10-09. Landing remains blocked because the existing CI and OpenClaw E2E workflows run on GitHub-hosted ubuntu-latest for pull requests to main/next, including these workflow edits. No supported complete gate on the permitted debian1/debian2/debian3 nodes was established. No PR, forbidden-host test dispatch, broad runner migration or synthetic success was created. The source branch stays prepared and unmerged; it is not awaiting owner merge permission. The existing release.yml artifact upload/download handoff remains an explicitly disclosed functional-transfer blocker.

- [x] Owner approval to merge the prepared change is recorded.
- [ ] Actual default-branch integration through the unchanged normal gate.
