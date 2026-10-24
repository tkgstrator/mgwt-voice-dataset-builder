# Rust MGWT Voice Dataset Builder Design

## Intent

Build a native Rust command-line application that reads an authorized local
Steam installation of `manosaba.app`, extracts the Japanese Naninovel dialogue
and matching Unity `AudioClip` assets, and writes a deterministic,
fully-audited Hugging Face-compatible Parquet dataset.

The repository is public. It must contain source code, configuration hashes,
synthetic fixtures, and documentation only. It must never contain game bundles,
decoded dialogue, decoded audio, Steam credentials, Hugging Face credentials,
or generated dataset shards.

The tool is a local builder and verifier. Publishing to Hugging Face is outside
this project.

## Goals

- Provide one native Rust binary with no Python, UnityPy, .NET, AssetRipper, or
  FMOD runtime requirement.
- Read the Steam app strictly read-only and reject any input that differs from
  an explicitly reviewed manifest.
- Parse the serialized Naninovel command structure and retain enough provenance
  to explain every accepted and rejected candidate.
- Match scripts and audio globally by a narrowly normalized exact voice ID.
- Decode supported Unity `AudioClip` payloads to canonical PCM WAV using a
  pure-Rust path.
- Open each Unity bundle once per processing pass, then process one clip buffer
  at a time. Never reopen a large bundle once per clip and never retain all
  decoded clips in memory.
- Produce the exact six-column dataset schema under
  `data/ja/<character_id>/train-*.parquet`.
- Generate deterministic audit, rejection, manifest, README, and terminal build
  reports, then independently read back every output row and WAV before local
  promotion.
- Support macOS Apple Silicon first while keeping portable Rust code and Linux
  CI for the synthetic test suite.

## Non-goals

- Uploading, replacing, or configuring a Hugging Face dataset repository.
- Modifying, patching, decrypting, or repackaging `manosaba.app`.
- Fuzzy voice/text matching, speech recognition, speaker inference, emotion
  inference, or guessed recovery from ambiguous commands.
- Shipping proprietary game content or using real dialogue/audio as committed
  test fixtures.
- Resampling every accepted clip to a new sample rate, channel count, or sample
  width. Version 1 preserves decoded PCM properties and canonicalizes only the
  WAV container.
- General support for arbitrary Unity/Naninovel games in version 1.

## Chosen Approach

Use a pure-Rust implementation based on `unity-rs-core`, pinned to the exact Git
revision `b3e509ed09fe6ff3a897d04ddb75eb5214482265` and locked by `Cargo.lock`.
That revision exposes bounded UnityFS/SerializedFile/TypeTree, `AudioClip`, and
pure-Rust FSB5 decoding paths. The application owns all manosaba-specific
inventory, Naninovel parsing, matching, output, and verification logic.

### Considered alternatives

1. **Chosen: native Rust plus pinned `unity-rs-core`.** This is the only option
   that meets the single-native-binary requirement and provides a bundle-scoped
   parsing model suitable for the performance target.
2. **Rust orchestrator plus UnityPy.** This could reuse the prototype quickly,
   but it would require Python at runtime and retain the UnityPy integration and
   packaging burden.
3. **Rust orchestrator plus AssetRipper.** This would add a .NET application and
   heuristic export stage, while still requiring custom Naninovel matching and
   audit reconstruction.

If the compatibility gate below shows that the pinned Rust dependency cannot
read the approved manosaba structures or codec, implementation stops with
evidence. It must not silently add a Python, .NET, native FMOD, or external CLI
fallback.

## Input Contract

The primary commands are:

```text
mgwt-voice-dataset-builder inspect --app <manosaba.app> --output <candidate-dir>
mgwt-voice-dataset-builder build --app <manosaba.app> --output <dataset-dir>
mgwt-voice-dataset-builder verify --input <dataset-dir>
```

`inspect` is read-only with respect to the app. It may write candidate manifests
only outside the app. It never promotes or overwrites reviewed configuration.

`build` requires checked-in versioned configuration:

- an application/input manifest with every consumed bundle, identity file, and
  external resource relative path, byte size, and SHA-256;
- a finite speaker allowlist with its source manifest identity and structural
  evidence digests;
- the expected Unity and app identity values.

The initial configuration is migrated from the already reviewed Python
prototype and independently rehashed by the Rust implementation. The known
inventory contains 24 script bundles and 35 voice bundles. The migration does
not copy ignored candidate reports or proprietary payloads into this repository.
It records the former candidate, manifest, and full-input digests only as
provenance, preserves and rechecks the structural speaker-evidence digests, and
binds the new speaker allowlist to the newly serialized portable manifest hash
and identity. The manifest, migration record, and allowlist are validated as one
approval unit.

The reviewed manifest is portable and content-addressed. It does not authorize
only the prototype's historical absolute realpath. `--app` may name the user's
Steam installation or a byte-identical relocated copy when every reviewed
relative path, app identity value, size, and SHA-256 matches. The resolved
realpath is recorded as run evidence but is not a reusable trust decision.

All input paths are traversed relative to an opened app-directory capability,
with no-follow component checks and descriptor identity checks. Symlinks, path
traversal, unapproved resources, missing files, added consumed resources, size
drift, and hash drift are fatal validation errors. Hashing and Unity parsing use
the same bound handle/source region; if the pinned dependency requires a path,
the tool first creates and hashes a run-owned immutable snapshot and parses that
snapshot rather than resolving the app path again. The app is rehashed after
the build as a checkpoint-equality test; this does not claim continuous
immutability against a privileged concurrent writer.

## Unity Compatibility Gate

Before building the full pipeline, an opt-in real-app probe must prove all of
the following against the approved bounded script/voice pair:

- the pinned `unity-rs-core` revision opens the Unity 6000.0.48f1 bundles;
- all expected serialized script objects can be enumerated and their required
  TypeTree values can be read;
- 1,753 `AudioClip` locators can be enumerated from the bounded voice bundle;
- the TypeTree view exposes and validates the required AudioClip name,
  `m_Resource.m_Source`, `m_Resource.m_Offset`, `m_Resource.m_Size`, channels,
  sample rate, sample count, and bit width, and agrees with the core reader's
  path ID/name/payload source identity, start, and length under the pinned
  inline/external resolution rules;
- strict Unity-version mode is enabled for every load;
- every codec/layout signature observed in the bounded voice bundle has at
  least one representative decoded to valid WAV; the later full matched-clip
  gate repeats this check for all eligible signatures;
- decoded properties and SHA-256 values agree with an independently retained,
  ignored Python baseline for those representatives;
- a processing pass opens the voice bundle exactly once, not once per clip;
- pre/post approved input hashes are identical.

This probe is a gate, not a best-effort warning. A layout or codec mismatch
stops implementation until the dependency or design is explicitly revised.

## Architecture

### CLI and orchestration

`src/main.rs` parses commands and maps errors to stable exit codes. `src/lib.rs`
exposes the internal pipeline for integration tests. The build orchestrator owns
ordered phases, cancellation checks, disk reservations, output staging, terminal
verification, and atomic no-clobber promotion.

Exit codes:

- `0`: complete, including `complete_with_rejections` when every source candidate
  is terminally accounted for;
- `1`: processing, decoder, write, or verification failure;
- `2`: input, path, manifest, output-safety, or configuration validation failure.

### Input inventory

The inventory module walks only the authorized app root, hashes bounded streams,
discovers dependency resources through the Unity parser, and compares a sorted
normalized inventory against the reviewed manifest. Hashing is streaming and
does not materialize whole bundles.

### Bundle-scoped Unity reader

The Unity adapter opens one approved bundle for a pass in strict-version mode,
verifies it through a bound handle or owned verified snapshot, constructs the
serialized object index once, and processes locators in deterministic order.
When a path-only dependency requires snapshots, the tool reserves their exact
bundle/resource working-set bytes before copying, retains them for only that
bundle session/pass, and cleans them on success, error, interruption, and unwind.
It performs a bounded AudioClip TypeTree read for fields not exposed by the
high-level core asset type and cross-checks the two views.
It retains the compressed bundle/container state only for that bundle.

For audio, it exposes one source-bound clip payload/direct-WAV kind at a time.
The caller streams decoded WAV data through a bounded canonicalizing/hash sink
to owned staging before requesting the next; it does not require a second full
WAV buffer. Payload binding has two explicit branches. An empty
`m_Resource.m_Source` is inline: matching the pinned core, its payload starts at
the bounded parser position immediately after the resource fields and uses the
declared size. The declared offset is range-checked, recorded, and compared with
the approved inline-layout evidence, but is not used as the inline payload
start. A nonempty source is external: it is resolved to exactly one reviewed
full relative path, and its declared offset/size must construct the same region
in that verified resource. Both branches require equality with the core payload
source identity, start, and length before decode. Only the selected source-bound
region is visible during the clip, so an earlier same-basename external
resource can never win. Resource handles are scoped to the bundle/session and
are closed on success, candidate rejection, infrastructure error, interruption,
and panic-safe unwind boundaries.

No cache may retain decoded audio. A per-build verified-input cache may retain
only file identities/hashes and immutable indexes. Measurement and staging may
open a bundle in separate passes, but each pass opens it at most once.

Version 1 hard limits are checked before allocation/decompression: 512 MiB per
input bundle, 2 GiB expanded bundle data, 200,000 objects per bundle, 256 MiB
metadata per bundle, 1 MiB per serialized string, 4,096 bytes per relative path,
and 256 MiB per canonical WAV/Parquet audio value. The compatibility inventory
must show every approved input is within these limits. Checked size arithmetic
and the pinned core's load budgets are mandatory.

### Naninovel parsing

The parser consumes serialized script objects and validates the proven
`playlist.commands`, `references.RefIds`, and `textMap.idToText` shapes. For
each command it records:

- stable candidate ID;
- normalized voice ID evidence;
- raw author ID;
- raw display text;
- normalized transcription and ordered normalization operations;
- bundle, object path ID, container path, logical script path/name, command
  index, and bounded neighboring command classes.

Unreadable top-level script objects fail their containing bundle. Candidate
accounting covers every voice-shaped command/reference and every `AudioClip`;
ordinary non-voice Naninovel commands are structural context, not dataset
candidates. A malformed individual voice-shaped command becomes one typed
rejection. If it exposes multiple independently safe voice IDs, that one
rejection blocks every exposed group without being counted more than once.

### Normalization and speaker resolution

Voice IDs use only fixture-proven wrapper removal and a case-consistent audio
extension removal. There is no fuzzy, substring, edit-distance, or chapter-name
matching.

Dialogue normalization removes only reviewed Naninovel markup/ruby/control
forms, normalizes line breaks, and records every operation. Empty or non-lexical
results are rejected.

Speakers are resolved only from the checked-in finite allowlist and proven
author/voice evidence. Missing, unknown, or conflicting evidence rejects the
complete voice-ID group.

### Matching and candidate accounting

Script and audio references are grouped globally across all recognized bundles
by exact normalized voice ID.

- Repeated script occurrences must agree on normalized transcription and
  resolved speaker. All occurrences remain in the audit record.
- A group with a prior typed blocker, missing side, or conflicting script/audio
  evidence is rejected completely.
- Same-ID audio candidates with identical canonical WAV hashes select the
  bytewise-lexically first provenance as canonical; remaining candidates are
  duplicate aliases.
- Same-ID candidates with differing WAV hashes reject the group.
- Cross-ID identical WAVs deduplicate only when normalized transcription and
  speaker agree; otherwise every involved group is rejected.
- Every source script command and audio asset has exactly one terminal outcome:
  accepted occurrence, canonical accepted audio, duplicate alias, or rejection.

In the equation above, "source script command" means a voice-shaped candidate,
not every ordinary Naninovel command. A build with zero accepted rows is fatal.
For each voice bundle with one or more eligible matched decode attempts, all
attempts failing is also fatal even when the rejection equations balance. A
bundle with no eligible matched attempts is reported and may be nonfatal.

### Audio decoding and canonical WAV

The adapter uses the pinned pure-Rust audio path to decode supported resident or
streamed Unity `AudioClip` payloads, including the needed FSB5 codec. Unsupported
codec/layout errors remain explicit and never fall back to raw bytes or another
runtime.

Canonicalization accepts complete uncompressed integer PCM WAV with 8, 16, 24,
or 32-bit samples. It validates RIFF size/chunks/padding, format tag, channels,
sample rate, byte rate, block alignment, frame count, and payload length. It
rewrites exactly one 16-byte PCM `fmt ` chunk followed by one `data` chunk,
preserving PCM frames, channels, sample rate, and integer width. Odd payloads
receive the required zero pad byte.

Decoded audio that is empty, multi-sample when unsupported, non-WAV, malformed,
or unsupported is a candidate-level rejection only when the decoder proves the
failure is local to that clip. I/O, approved-input, parser, allocator, and
unexpected codec infrastructure errors abort the build.

### Parquet output

The output schema and order are exactly:

```text
audio, transcription, speaker, emotion, language, source_path
```

`audio` is the Hugging Face-compatible struct containing canonical WAV `bytes`
and a deterministic safe `.wav` `path`. `emotion` is null and `language` is
`ja`. Files are written only under:

```text
data/ja/<character_id>/train-00000-of-00000.parquet
```

There is no `misc` directory and no per-character manifest. Rows are sorted by
bytewise UTF-8 speaker/source/audio identity. Shards target 500 MiB of embedded
audio. Arrow record batches are bounded by both row count and estimated payload
bytes; an audio value above the 256 MiB hard limit is rejected before Arrow
allocation. Field order, nested field order, types, nullability, and Hugging
Face schema metadata are pinned by a synthetic reference. A development-only,
locked Python `datasets` check opens a Rust-written synthetic shard offline and
must recognize the audio feature; Python is not installed or invoked by the
runtime binary.

### Audit and verification

The build writes deterministic:

```text
README.md
metadata/audit.jsonl
metadata/rejected.jsonl
metadata/manifest.json
metadata/report.json
data/ja/<character_id>/train-*.parquet
```

The audit records accepted rows and duplicate aliases with complete script and
audio provenance, raw/normalized text evidence, canonical linkage, audio
properties/hash, and Parquet row location. Rejections retain the earliest
terminal stage, stable reason, known provenance, and blocking group IDs.

Independent verification starts from serialized files, not in-memory builder
objects. It validates the exact file set and hashes, sidecar equations,
canonical/alias relationships, Arrow schema and ordering, every Parquet row,
every embedded WAV, every row location, shard totals, and zero pending
candidates. It streams rows and WAVs without materializing a whole shard.

### Atomicity, disk, and interruption

The requested final directory must not exist. The builder uses an
output-adjacent, uniquely owned staging directory and an independently owned
audio staging directory. It calculates and reserves space from exact planned WAV
sizes, shard targets, sidecar allowance, and safety margin, then rechecks before
large writes.

The final directory becomes visible through one directory-capability-relative,
exclusive no-clobber rename only after terminal verification and final app
rehash succeed. Immediately before promotion, the builder reopens the requested
output-parent pathname through its bound parent capability and requires its
descriptor identity to equal the originally bound output parent. A missing or
replaced parent is a validation failure: no rename occurs into the old/detached
directory, the requested final path remains absent, and owned staging is
reported. Safe crate APIs wrap the platform primitive; first-party unsafe code
remains forbidden. Before that rename, any failure leaves the final path absent
and reports owned retained staging paths. After an irreversible successful
rename, a signal cannot turn the result into a reported failure. Cleanup removes
only paths created and recorded by that run.

## Repository Structure

```text
Cargo.toml
Cargo.lock
rust-toolchain.toml
LICENSE
README.md
.gitignore
.github/workflows/ci.yml
config/manosaba-inputs-v1.json
config/manosaba-speakers-v1.json
config/manosaba-config-migration-v1.json
docs/superpowers/specs/
docs/superpowers/plans/
src/main.rs
src/lib.rs
src/cli.rs
src/error.rs
src/paths.rs
src/inventory.rs
src/unity.rs
src/naninovel.rs
src/normalize.rs
src/speaker.rs
src/matching.rs
src/audio.rs
src/parquet.rs
src/report.rs
src/build.rs
pyproject.toml
uv.lock
tools/verify_hf_interop.py
tests/
```

Modules may be split further by the implementation plan when one file would own
multiple independent responsibilities. Public APIs remain minimal; this is a
CLI product first.

## Testing Strategy

- Unit tests use synthetic bytes/structures for path safety, normalization,
  parsing, group accounting, RIFF canonicalization, deterministic names,
  sharding, sidecars, and error taxonomy.
- Property/fuzz-style bounded tests cover malformed RIFF, offsets, sizes,
  counts, and unsafe paths without unbounded allocation or panic.
- Integration fixtures are synthetic or generated and contain no game text or
  audio.
- An opt-in local real-app suite reads `MGWT_APP_PATH`; it never runs in public
  CI and writes only to run-owned temporary directories.
- Differential real-app evidence compares approved inventory, parsed counts,
  selected WAV hashes/properties, candidate equations, normalized rows, and
  rejection semantics with the ignored Python baseline. Cross-language Parquet
  and sidecar bytes are not required to match because writer implementations
  may encode equivalent data differently. The Rust builder itself must still
  produce byte-identical deterministic outputs across two clean runs. The
  Python extractor is an oracle during development, not a runtime dependency.
- A bundle-open counter proves exactly one open per bundle per pass.
- The full differential report compares per-bundle object/reference counts,
  terminal candidate totals, normalized row identity/text hashes, and rejection
  stage/reason distributions. Every unexplained difference blocks acceptance.
- The bounded real pair is built twice from a clean committed producer and every
  deterministic output hash must match.
- The full build is accepted only after complete independent readback and exact
  pre/post app inventory equality.

Public CI runs, at minimum:

```text
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --all-targets --all-features --locked
cargo doc --no-deps --locked
```

Dependency/license/security checks may be added when their configuration is
checked in and deterministic.

## Documentation and Distribution

The README is Japanese-first with a short English summary. It documents legal
use of locally authorized files, prerequisites, commands, output schema,
configuration approval, expected disk usage, exit codes, failure recovery, and
the fact that no game data or dataset is included.

The project uses the MIT license. Third-party licenses and the pinned
`unity-rs-core` revision are identified. `Cargo.lock` is committed. Generated
datasets, app paths, caches, logs, temporary staging, and real evidence are
ignored.

## Delivery

Implementation proceeds task-by-task with TDD and independent review. No game
asset is committed. Before each local Git commit or push, the guarded human
GitHub identity workflow is applied. After the complete synthetic and opt-in
real verification gates pass, the reviewed commits are pushed normally to the
empty repository's `master` branch. No force push or history rewrite is used.
