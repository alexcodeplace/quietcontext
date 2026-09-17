# QC Semantic Repository Graph

outcome: QC repository navigation becomes a semantic graph over the same bounded, incremental native repository index that already powers `qc map`, `qc sym`, `qc refs`, and `qc outline`. The graph resolves declarations and relationships so QC can answer callers, callees, impact, dependency, and path questions without dumping source or guessing across ambiguous names.
status: COMPLETE
source request: owner 2026-09-17 — preserve the cheap repository map, add semantic relationships and graph traversal, and complete the native QC implementation without introducing a separate heavyweight product surface.

## Product contract

QC keeps two complementary views of one repository index:

1. `qc map` is the cheap orientation view. It stays bounded, deterministic, and fast.
2. semantic queries are precise graph views over the same generation of indexed source.

The graph is internal infrastructure, not a second indexing subsystem. A repository refresh updates structural summaries, symbols, lexical postings, semantic nodes, semantic edges, and traversal answers as one generation.

## Non-negotiable invariants

- **Precision before recall.** Ambiguous relationships are left unresolved. QC must never attach a call or reference to an arbitrary same-named declaration.
- **Definition identity survives collisions.** A symbol is identified by repository-relative file, declaration span, kind, and qualified name. Bare names may select several definitions; results remain grouped by definition.
- **One source generation.** Map, symbol, references, callers, callees, impact, dependencies, and path queries observe the same refreshed snapshot.
- **Incremental by construction.** Existing file-watch reconciliation remains the update mechanism. Semantic data is rebuilt for changed source records and graph-wide resolution is deterministic over the refreshed snapshot.
- **Bounded output.** Every public graph query has a byte cap and logical result cap. Traversal has depth and node limits.
- **Cycle safe.** All traversals maintain visited state and terminate on recursive call/import/type graphs.
- **No source dumping by default.** Navigation responses contain identities, locations, edge kinds, and concise evidence. `qc outline` remains the declaration/body-navigation bridge.
- **No silent partial success.** If scan truncation or read failures make a negative answer untrustworthy, QC returns an incomplete/error result instead of “not found”.
- **No hidden fuzzy binding.** Fuzzy matching may help discover candidate symbols later, but never creates semantic edges.
- **No symlink escape.** Existing canonical-root and no-follow traversal rules continue to apply to semantic extraction and import resolution.

## Scope

The first complete semantic engine covers the languages already treated as first-class source families by native QC:

- JavaScript: `.js`, `.jsx`, `.mjs`, `.cjs`
- TypeScript: `.ts`, `.tsx`
- Python: `.py`
- Rust: `.rs`
- Go: `.go`

`.astro`, `.vue`, and `.svelte` remain visible to the structural map through the existing conservative fallback until a dedicated embedded-script extractor exists. They must not emit speculative semantic edges.

## Layered architecture

```text
filesystem scan / watcher
        |
        v
source records
        |
        v
AST extraction
  symbols + unresolved relationships
        |
        v
resolver
  imports + qualified names + local scope + receiver/type hints
        |
        v
immutable semantic graph for generation N
        |
        +--> map / symbol / references
        +--> callers / callees
        +--> impact
        +--> dependencies / dependents
        +--> path
```

Extraction never performs graph traversal. Resolution never renders user output. Traversal never reparses files. Rendering never mutates the graph.

## Data model

### Semantic node

Each node has:

- stable generation-local `NodeId`
- `NodeKind`
- simple `name`
- `qualified_name`
- repository-relative `file`
- `start_line`, `start_column`, `end_line`, `end_column`
- optional enclosing node
- optional signature/evidence text capped to a small fixed size

Required node kinds:

- `file`
- `function`
- `method`
- `class`
- `interface`
- `struct`
- `enum`
- `trait`
- `type`
- `constant`
- `variable`

The schema may grow additively. Rendering must tolerate unknown future kinds.

### Semantic edge

Each edge has:

- `from`
- `to`
- `EdgeKind`
- source evidence location (`file`, `line`, `column`)
- resolution confidence class

Required edge kinds:

- `contains`
- `imports`
- `calls`
- `extends`
- `implements`
- `references`

Edges are unique by `(from, to, kind, evidence location)`.

### Confidence classes

- `exact`: syntax plus namespace/import/scope resolution identifies one target.
- `scoped`: exactly one target remains after file/enclosing-type/local-scope constraints.
- `unique`: repository-wide exact-name fallback found exactly one compatible declaration.

No `ambiguous` edge is stored. Ambiguous unresolved relationships are counted in graph diagnostics.

## Extraction contract

AST parsing is mandatory for semantic edges. Regex declaration matching may remain as the structural fallback for `qc map`/`qc outline`, but cannot create `calls`, `imports`, `extends`, or `implements` edges.

Per file, extraction returns:

- symbols with spans, kind, simple name, qualified name, and enclosure
- imports with module/path text and imported/local aliases when available
- call sites with callee text, receiver text when present, and enclosing callable
- inheritance/implementation references
- identifier references that have enough syntax context to resolve safely

Parse errors are allowed when the parser can still produce a useful tree. A file that cannot produce trustworthy semantic facts contributes structural map data only and increments semantic diagnostics.

## Resolution order

Resolution is deterministic and ordered from strongest to weakest evidence.

### 1. Local lexical scope

Prefer declarations in the same callable/type/file when syntax identifies them.

### 2. Explicit imports and aliases

Resolve import/use targets to repository files, then bind imported names only within those target files or namespaces.

Examples include:

- JS/TS relative `import` / `require`
- Python relative and absolute local-module imports
- Rust `use`, `crate::`, `self::`, `super::`, and module paths
- Go module/package-qualified selectors for local packages

### 3. Receiver/type ownership

For member calls, prefer methods owned by the resolved receiver type or enclosing type. If receiver type cannot be established uniquely, do not guess between same-named methods.

### 4. Same-file exact name

A bare call/reference may bind to one compatible declaration in the same file.

### 5. Repository-wide unique compatible name

Only when exactly one compatible declaration exists across the index may QC bind a bare unresolved relationship repository-wide.

Any stage yielding multiple equally valid targets stops unresolved unless a later stronger syntactic discriminator exists.

## Import/file resolution

Import resolution is repository-local only. Dependencies outside the indexed root become unresolved external relationships and do not create phantom nodes.

Resolution must support common source-layout variants:

- JS/TS extension probing and `index.*`
- Python package `__init__.py` and relative-dot imports
- Rust `mod.rs`, sibling module files, and `crate/self/super` paths
- Go local module path from `go.mod` plus package directory mapping

Configuration-driven alias systems are additive work only when implemented with deterministic local files. Missing configuration never permits name-only cross-package guessing.

## Existing command behavior

### `qc map`

Preserve the existing compact file/declaration orientation format and byte cap. It may add a tiny semantic footer with node/edge counts only when it fits the existing cap.

### `qc sym` / `qc repo symbol`

Return all exact definitions for a name. Multiple definitions stay separate and labeled by file/qualified name. Never silently pick one.

### `qc refs` / `qc repo references`

Prefer resolved semantic references grouped by target definition. Lexical identifier occurrences may be shown as a clearly labeled fallback only when semantic resolution is unavailable, never mixed as if equivalent.

## New command surface

Canonical commands:

```text
qc callers <symbol> [--file <path>] [--depth N]
qc callees <symbol> [--file <path>] [--depth N]
qc impact <symbol> [--file <path>] [--depth N]
qc deps <file-or-symbol> [--depth N]
qc dependents <file-or-symbol> [--depth N]
qc path <from> <to> [--max-depth N]
```

Equivalent `qc repo ...` spellings are accepted for consistency with existing repository navigation.

The MCP `repo` tool adds the same actions rather than adding more public tools. The seven-tool public surface remains unchanged.

## Query semantics

### Symbol selection

A symbol query uses exact simple-name or qualified-name matching. When several definitions match:

- without `--file`, render one result section per definition
- with `--file`, restrict to matching repository-relative file suffix/path
- if the file filter matches no definition, return an explicit no-match result, not a fallback union

### Callers

Incoming `calls` edges. Depth 1 is direct callers. Depth >1 traverses incoming call edges only.

### Callees

Outgoing `calls` edges. Depth 1 is direct callees. Depth >1 traverses outgoing call edges only.

### Impact

Incoming semantic dependency traversal from the selected definition. It includes `calls`, `references`, `imports`, `extends`, and `implements`, then promotes affected enclosing symbols/files as needed for useful reporting. Default depth is 3.

Separate matching definitions get separate impact radii. They are never merged behind a bare-name heading.

### Dependencies / dependents

For a file target, traverse file-level `imports` plus symbol edges projected to owning files. For a symbol target, traverse outgoing/incoming semantic dependencies respectively.

### Path

Breadth-first shortest path over semantic dependency edges with cycle protection. The output shows each hop, edge kind, and evidence location. Default maximum depth is bounded.

## Traversal limits

Defaults:

- callers/callees depth: `1`
- impact depth: `3`
- dependency depth: `1`
- path max depth: `8`
- maximum returned graph nodes per query: `200`
- maximum rendered bytes: existing repository response cap unless a smaller action-specific cap is configured

Depth and limits are validated before daemon dispatch. No request may create an unbounded graph walk.

## Incremental index behavior

The current repository daemon remains authoritative.

On refresh:

1. reconcile filesystem changes using existing path safety and ignore policy
2. rebuild source/structural records for changed files
3. extract semantic facts for changed files
4. remove old semantic facts owned by deleted/changed files
5. rebuild deterministic lookup tables
6. resolve relationships against the refreshed declaration/import tables
7. publish the new immutable generation atomically

Readers never observe half-resolved state.

A full re-resolution after an incremental extraction is acceptable initially because declaration/import changes can affect edges from unchanged files. Correctness comes before micro-optimizing resolver invalidation. The implementation must keep this seam explicit so dependency-directed re-resolution can be introduced later.

## Memory and CPU bounds

Semantic indexing must continue to honor:

- maximum source files
- maximum source file bytes
- logical index byte ceiling
- ignored/build/dependency directory policy
- no-follow symlink policy

Semantic structures count toward the logical index byte estimate. When the hard index ceiling would be exceeded, the generation fails closed instead of publishing an incomplete semantic graph as complete.

## Diagnostics

Graph diagnostics are generation metadata, not normal response noise. Track at minimum:

- parsed semantic files
- parse-fallback files
- semantic node count
- semantic edge count by kind
- unresolved import count
- unresolved call count
- ambiguous relationship count

Negative semantic answers include the existing partial-scan marker when diagnostics make completeness uncertain.

## Output examples

A direct caller query should resemble:

```text
[qc-callers v1] saveUser — 2 definitions
saveUser — src/a/user.ts:18
  <- submitForm — src/a/form.ts:42 [calls @42]
saveUser — src/b/user.ts:11
  <- syncUser — src/b/sync.ts:77 [calls @77]
```

Impact keeps radii separate:

```text
[qc-impact v1] saveUser — 2 definitions, depth 3
saveUser — src/a/user.ts:18
  d1 submitForm — src/a/form.ts:42 [calls]
  d2 POST /users adapter — src/a/api.ts:30 [references]
saveUser — src/b/user.ts:11
  d1 syncUser — src/b/sync.ts:77 [calls]
```

## Compatibility

- Existing `qc map|sym|refs|outline` aliases continue to work.
- Existing native protocol clients remain compatible only if protocol versioning says the action enum is understood. Adding graph actions requires a protocol version bump or a backward-compatible tagged extension with tests proving old actions unchanged.
- Existing seven MCP tools remain seven.
- Existing map/outline byte budgets and deterministic ordering remain contract-tested.

## Security

- No arbitrary command execution is introduced by graph queries.
- Import resolution reads only files/configuration under the canonical repository root except standard user-level language configuration explicitly permitted by a future spec.
- No network resolution, package registry lookup, dependency installation, or external language server is part of indexing.
- Paths from syntax are normalized and checked before filesystem probing.

## Acceptance criteria

### Structural compatibility

- Existing native repository corpus tests remain green unchanged or with additive semantic assertions.
- `qc map`, `qc sym`, `qc refs`, and `qc outline` retain deterministic bounded output.

### Semantic correctness

Fixtures must prove all of the following for JS/TS, Python, Rust, and Go where syntax applies:

- direct call resolves to the correct declaration
- imported alias resolves to its source declaration
- same-named declarations in separate files remain distinct
- ambiguous bare cross-file calls produce no fabricated edge
- same-file exact call resolution works
- methods on distinct owner types do not cross-bind by method name
- inheritance/implementation edge extraction works for supported syntax
- file imports resolve to repository-local file nodes
- changing/deleting a file removes stale nodes and edges after refresh

### Traversal correctness

- callers and callees return correct direct neighbors
- depth traversal is cycle safe
- impact returns incoming dependency radius and keeps colliding definitions separate
- path returns a shortest semantic path and terminates when none exists
- file filter narrows a colliding symbol and fails explicitly when it matches none
- logical node/depth caps are enforced

### Freshness

- daemon hit returns generation N
- edit causes refreshed/reconciled generation N+1
- semantic queries after the refresh observe N+1 only

### Performance guardrails

On the existing native corpus fixture set:

- warm graph query does not rescan the repository
- map/symbol/reference warm-hit latency does not regress by more than the repository test tolerance
- semantic extraction remains bounded by existing file and logical-byte ceilings

### Delivery

- Rust unit tests green
- native QC integration tests green
- package build/typecheck green
- focused CLI/MCP graph tests green
- repository-wide grep confirms the implementation/spec/comments contain no unrelated product branding or copied attribution strings

## Implementation order

1. semantic node/edge model and AST extraction
2. deterministic resolver and ambiguity policy
3. graph storage inside `SourceIndex`
4. callers/callees/impact/deps/dependents/path traversal
5. native daemon protocol actions
6. JS `qc` launcher and MCP `repo` action wiring
7. fixtures for collisions/import aliases/cycles/incremental edits
8. full validation and receipts

## Completion evidence

Implemented in native QC with AST-backed extraction for JavaScript/TypeScript, Python, Rust, and Go; deterministic import/call/type resolution; graph storage in the existing immutable repository generation; semantic references; callers, callees, impact, dependencies, dependents, and shortest-path traversal; protocol-v2/native-handshake upgrade; daemon socket revision fencing; CLI aliases; and the existing seven-tool MCP surface.

Validated on the exact feature worktree through the sanctioned K3s builder:

- native Rust suite: all tests green, including alias resolution, same-name ambiguity, receiver ownership, cycles, inheritance, trait implementation, and semantic-reference fallback guards
- package build/typecheck/bundle assertions: green
- QuietContext package suite: 92/92 green
- native integration/corpus/HTTP suite: 18/18 green with one existing platform-specific fidelity skip
- `tools/list`: under the existing 4 KiB cap and byte-identical over stdio/HTTP
- live repository watcher coverage: semantic call edges disappear/reappear after edits and stale semantic definitions disappear after file deletion
- changed-file audit: no unrelated product branding or attribution strings in spec, code, comments, or tests

The implementation deliberately leaves `.astro`, `.vue`, and `.svelte` on the conservative structural-map fallback until dedicated embedded-script parsing is specified.

Next executable action: none for this specification.
