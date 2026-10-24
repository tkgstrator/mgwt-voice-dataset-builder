# Rust MGWT Voice Dataset Builder Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship and push a native Rust CLI that converts an approved local `manosaba.app` installation into the reviewed, deterministic Japanese voice Parquet dataset with a pure Rust runtime and no .NET or proprietary runtime dependencies.

**Architecture:** The CLI validates a portable content-addressed app manifest, uses one pinned `unity-rs-core` bundle session per bundle/pass, parses Naninovel command structures into typed candidates, performs conservative exact-ID matching, decodes and canonicalizes one audio clip at a time, and writes output-adjacent staged Parquet/metadata. A fresh-process verifier streams every file, row, and WAV before one exclusive final-directory promotion.

**Tech Stack:** Rust 1.88.0 minimum, edition 2024, `unity-rs-core` pinned at Git revision `b3e509ed09fe6ff3a897d04ddb75eb5214482265`, Clap, Serde/serde_json, SHA-2, Arrow/Parquet Rust crates, a safe descriptor/capability filesystem crate, `tempfile`, `thiserror`, and synthetic/opt-in real-app tests. A locked Python `datasets` environment is permitted only as a development-time interoperability oracle; it is never part of the Rust CLI runtime.

**Spec:** `docs/superpowers/specs/2026-10-24-rust-mgwt-dataset-builder-design.md`

## Global Constraints

- The runtime is pure Rust: no Python, UnityPy, .NET, AssetRipper, FMOD, external decoder CLI, shell pipeline, or network requirement. A locked, offline Python `datasets` check is development-only and cannot be invoked by the production binary.
- Pin `unity-rs-core` to exact revision `b3e509ed09fe6ff3a897d04ddb75eb5214482265`; commit `Cargo.lock` and reject an unlocked dependency change in CI.
- Set MSRV to Rust 1.88.0, edition 2024, and `#![forbid(unsafe_code)]` for first-party crates.
- Never commit game bundles, extracted script/dialogue text, audio, generated Parquet, real-app reports, absolute local app paths, credentials, or caches.
- The app is read-only. Every approved input is opened by descriptor-relative, no-follow component traversal and checked with descriptor identity. Hashing and Unity parsing consume the same bound handle/source region or one run-owned immutable verified snapshot when a dependency is path-only. Pre/post hashes are checkpoint-equality evidence, not a claim of continuous immutability.
- All candidate reports, staging, caches, and final output are outside the app. Staging and promotion are relative to a bound output-parent directory capability; first-party code remains unsafe-free.
- Output schema order is exactly `audio, transcription, speaker, emotion, language, source_path`; `emotion` is null and `language` is `ja`. Arrow field order, types, nullability, nested audio-field order, and Hugging Face schema metadata are pinned to the approved synthetic reference.
- Output layout is only `data/ja/<character_id>/train-*.parquet`, plus root `README.md` and `metadata/{audit,rejected,manifest,report}`; never create `misc` or `data/**/manifest.json`.
- Preserve decoded PCM sample rate, channels, and integer sample width; canonicalize the RIFF/WAVE container without resampling.
- One Unity bundle may be open at a time. Each bundle opens at most once per inventory/measurement/staging pass. Audio is streamed through a bounded sink and canonicalizer; no API may require a second full decoded-WAV buffer.
- Enforce checked hard ceilings before allocation: 512 MiB per input bundle, 2 GiB expanded bundle data, 200,000 objects per bundle, 256 MiB metadata per bundle, 1 MiB per serialized string, 4,096 bytes per relative path, and 256 MiB per canonical WAV/Parquet audio value. Approved inputs must fit these limits; forged lengths and decompression expansion fail safely.
- Every voice-shaped script candidate and every AudioClip receives one terminal outcome; zero accepted rows and all eligible decode attempts failing in a voice bundle are fatal.
- The final output path must be absent and becomes visible only through an exclusive no-clobber promotion after terminal readback and final app rehash.
- LocalGPT and Claude Code MCP plan/review routes must be attempted as required by the governing instructions. If unavailable, record that fact and use separate fresh read-only `gpt-6-sol` reviewers.
- Before every commit or push, read and apply the governing `gh-commit-identity` skill; use only the installed `guarded-git`, exact literal staging paths, fresh human identity, printable-ASCII English Conventional Commits, and a complete stored message no longer than 128 bytes.
- No force push, history rewrite, amend, rebase, merge/cherry-pick completion, or automatic retry after an uncertain push.

## Review Focus

- **Unity 6000.0.48f1 or TypeTree above a verified ceiling:** set `AssetLoadOptions::strict_unity_versions = true` for every load and fail with a typed compatibility error and no output rather than silently interpreting a newest-known layout; Task 3 adds the real and synthetic gates.
- **Streamed AudioClip resources sharing simplified basenames:** read `m_Resource.m_Source` through a bounded TypeTree projection, resolve exactly one approved full path, and make only that source-bound region visible during decode; Tasks 3 and 6 add earlier-basename/later-exact collision and cleanup tests.
- **Malformed Naninovel command exposing multiple or partly-invalid voice IDs:** emit one rejection that blocks every independently safe ID without duplicate source accounting; Task 4 pins it.
- **Interleaved voice IDs across large bundles:** schedule decode work by bundle, open each bundle once, retain only staged paths/metadata, and finalize groups in deterministic voice-ID order; Tasks 3, 5, and 6 pin open counts and output equality.
- **Interrupt/output-path race at atomic promotion:** before rename the final path remains absent and retained owned staging is reported; after rename success cannot be reported as failure; Task 8 pins destination-parent and signal races.
- **Portable approval migration:** preserve the former candidate/manifest/input-inventory digests as provenance while binding the new speaker allowlist to the new portable manifest SHA-256 and identity; Task 2 tests the migrated files as one approval unit.

---

### Task 1: Initialize the guarded Rust CLI foundation

**Files:**
- Create: `Cargo.toml`
- Create: `Cargo.lock`
- Create: `rust-toolchain.toml`
- Create: `.gitignore`
- Create: `.gitattributes`
- Create: `LICENSE`
- Create: `README.md`
- Create: `.github/workflows/ci.yml`
- Create: `src/main.rs`
- Create: `src/lib.rs`
- Create: `src/cli.rs`
- Create: `src/error.rs`
- Create: `src/paths.rs`
- Create: `tests/cli.rs`
- Create: `tests/path_safety.rs`

**Interfaces:**
- Consumes: the approved design, Rust 1.88.0+, and an empty `master` repository.
- Produces: `Cli`, `Command::{Inspect,Build,Verify}`, `AppError`, `ExitClass`, `ValidatedAppPath`, `ValidatedOutputPath`, and a testable `run(Cli) -> Result<RunOutcome, AppError>` entrypoint.

- [ ] **Step 1: Write failing CLI and path-safety tests**

  Add tests proving `--help` names the three commands, missing required paths exit as validation, app/output equality or nesting is rejected, an existing output leaf is rejected, symlinked ancestors are rejected, and a non-existing safe output leaf returns a bound parent directory capability plus literal leaf. Add a platform capability spike proving the selected safe crate can perform descriptor-relative no-follow opens and an exclusive no-clobber directory rename on supported macOS/Linux targets; an existing destination must remain untouched.

- [ ] **Step 2: Run the RED tests**

  Run: `cargo test --test cli --test path_safety`

  Expected: failure because no Cargo project or interfaces exist.

- [ ] **Step 3: Create the minimal package and stable error taxonomy**

  Use package/binary name `mgwt-voice-dataset-builder`, edition 2024, `rust-version = "1.88"`, and first-party unsafe-code denial. Select and pin the safe descriptor/capability filesystem crate only after the capability spike passes. Implement only argument parsing, validation-safe path wrappers, and exit mapping: success `0`, processing `1`, safety/validation `2`.

- [ ] **Step 4: Add deterministic repository hygiene and CI**

  Ignore `.DS_Store`, target directories, real-app evidence, datasets, staging, logs, and local app paths. Keep synthetic fixtures eligible for tracking. CI runs locked fmt, clippy, tests, and docs on macOS and Linux without real assets.

- [ ] **Step 5: Verify Task 1**

  Run:

  ```bash
  cargo fmt --all -- --check
  cargo clippy --all-targets --locked -- -D warnings
  cargo test --all-targets --locked
  cargo doc --no-deps --locked
  git diff --check
  ```

  Expected: all pass; `cargo metadata --locked` resolves the committed lockfile; no ignored/proprietary file is tracked.

- [ ] **Step 6: Review and commit Task 1**

  Request a fresh read-only review of the exact Task 1 diff. Resolve material findings with RED/GREEN tests. Apply guarded identity, stage only the files above plus approved design/plan documents, and commit `chore: initialize Rust dataset builder`. Do not push yet.

### Task 2: Migrate reviewed configuration and implement read-only inventory

**Files:**
- Create: `config/manosaba-inputs-v1.json`
- Create: `config/manosaba-speakers-v1.json`
- Create: `config/manosaba-config-migration-v1.json`
- Create: `src/config.rs`
- Create: `src/hash.rs`
- Create: `src/inventory.rs`
- Modify: `src/cli.rs`
- Modify: `src/lib.rs`
- Test: `tests/config.rs`
- Test: `tests/inventory.rs`
- Test: `tests/real_inventory.rs`

**Interfaces:**
- Consumes: `ValidatedAppPath`, checked-in Python prototype configurations, and pinned config schema constants.
- Produces: `ApprovedManifest`, `ApprovedSpeakers`, `ApprovalMigration`, `VerifiedInputFile`, `VerifiedBundleInputs`, `VerifiedSnapshot`, `InventorySnapshot`, descriptor-bound `inventory_app(&ValidatedAppPath, &ApprovedManifest) -> Result<InventorySnapshot>`, and atomic `inspect` candidate reports.

- [ ] **Step 1: Write failing manifest/config tests**

  Test exact schema/version strings, sorted unique safe relative paths, 24 script and 35 voice bundles, finite sorted speaker IDs, source manifest/config hashes, rejection of unknown fields, duplicate paths/IDs, traversal, absolute paths, oversized text, and malformed hashes. Load the portable manifest, migration record, and speaker allowlist together; require the allowlist to bind the new portable manifest SHA-256/identity and reject any altered speaker evidence.

- [ ] **Step 2: Write failing synthetic inventory tests**

  Construct a temporary fake app and assert streaming SHA-256, missing/added/changed approved entries, identity drift, deterministic candidate-report bytes, and rejection of a component or leaf swap after validation, during hashing, and before parser handoff. Prove the bytes hashed are the bytes exposed by `VerifiedInputFile` or copied once into `VerifiedSnapshot`. For the snapshot path, test exact peak disk reservation, one-session/pass lifetime, and cleanup after success, error, interruption, and unwind. Candidate generation must never rewrite checked-in config.

- [ ] **Step 3: Migrate configuration without proprietary content**

  Transform the reviewed Python manifest into a portable content-addressed Rust schema: retain relative paths, kinds, sizes, hashes, Unity/app identity, dependency declarations, and approval evidence; omit the old absolute `app_realpath`. Copy the approved 26 speaker IDs and compact structural evidence only. Record the former candidate SHA-256 `e8112ff95dd504fa9819316d4814716291d7fc14c7b63d53d7a351b98e7aec2f`, former manifest SHA-256 `b6fc6867ae2d32c59783c83a7fcd34dbdd7b660d855b4bbc432c5e1729a17e61`, and former full-input fingerprint `fe209715f41b88d7b1c54c76b22aaa46819d5f1110effb02fa2677fb48b5b1c6` only as provenance in the migration record. Bind the Rust speaker file to the newly serialized portable manifest hash and identity, and prove its speaker/evidence digests are unchanged.

- [ ] **Step 4: Implement bounded inventory and `inspect`**

  Traverse from a bound app directory descriptor with no-follow component opens, validate `fstat` identity, hash in bounded chunks, and return a source whose exact bytes remain bound for parsing. If `unity-rs-core` requires a path, reserve the exact current bundle/resource working-set bytes, copy the already-bound bytes once into a run-owned, content-addressed, read-only verified snapshot, and parse that snapshot; never reopen the original by pathname. Retain snapshots only for that bundle session/pass and clean them on every exit. Collect a normalized sorted inventory, compare every consumed entry, and atomically write diagnostic plus separately named candidate files outside the app. Do not invoke Unity parsing beyond the dependency surface owned by Task 3; expose an injectable dependency-discovery trait for tests.

- [ ] **Step 5: Run the opt-in real manifest gate**

  Run:

  ```bash
  test -n "${MGWT_APP_PATH:-}"
  cargo test --test real_inventory --locked -- --ignored --nocapture
  ```

  Expected: approved relative paths/sizes/hashes and Unity/app identity match, pre/post normalized snapshots are checkpoint-equal, and the Rust snapshot can be reconciled with the ignored Python inventory without persisting the absolute path.

- [ ] **Step 6: Verify, review, and commit Task 2**

  Run focused plus full locked fmt/clippy/tests/docs. Request a fresh config/inventory/path-safety review. Resolve findings and guarded-commit `feat: validate approved game inventory` with only Task 2 files.

### Task 3: Prove and wrap the pinned Unity reader

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `src/unity.rs`
- Create: `src/model.rs`
- Create: `tests/unity_adapter.rs`
- Create: `tests/real_unity_probe.rs`
- Modify: `src/inventory.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `ApprovedManifest`, approved bundle/resource entries, and `unity-rs-core` at exact revision `b3e509ed09fe6ff3a897d04ddb75eb5214482265`.
- Produces: `UnityBackend`, `UnityReader`, `BundleSession`, `RawScriptObject`, `AudioAssetRef`, `AudioMetadata`, `AudioLimits`, `DecodedAudioReceipt`, codec/layout signatures, and bundle-open/peak-buffer metrics.
- Required signatures:

  ```rust
  pub trait UnityBackend {
      type Session: BundleSession;
      fn open_bundle(&self, approved: &VerifiedBundleInputs) -> Result<Self::Session, AppError>;
  }

  pub trait BundleSession {
      fn raw_script_objects(&mut self) -> Result<Vec<RawScriptObject>, AppError>;
      fn audio_refs(&mut self) -> Result<Vec<AudioAssetRef>, AppError>;
      fn stream_audio(
          &mut self,
          reference: &AudioAssetRef,
          sink: &mut dyn std::io::Write,
          limits: &AudioLimits,
      ) -> Result<DecodedAudioReceipt, AppError>;
  }
  ```

  `VerifiedBundleInputs` binds one verified bundle plus its possibly empty approved external-resource map. `AudioAssetRef` contains the TypeTree-derived `PayloadBinding::{Inline, External}`, metadata-completeness state, and codec/layout signature; it does not own decoded audio. `BundleSession` owns one opened verified bundle and exposes only the exact approved source-bound region for the clip being streamed. The sink enforces byte and expansion limits before allocation and supports hashing/canonicalization without a second full payload.

- [ ] **Step 1: Pin the dependency and write failing adapter-contract tests**

  Assert exact Cargo source revision, `AssetLoadOptions::strict_unity_versions = true` on every real adapter entry, synthetic above-ceiling rejection through that entry, wrong-bundle locator rejection, path-ID/name/container/resource declaration checks including offset/size drift, empty-source inline payloads starting at the post-resource-fields parser position, nonempty-source external payloads starting at declared offset, deterministic object/reference ordering, one open for many refs, hard-limit enforcement, and typed separation among candidate decode, approved-input, unsupported-layout/codec, I/O, and unexpected failures.

- [ ] **Step 2: Add fake streamed-resource lifecycle tests**

  Test an empty `m_Resource.m_Source` and require the declared-size region beginning at the bounded parser position immediately after the resource fields in the verified owning serialized-object source. Mutate the declared inline offset independently and prove it is validated against approved layout evidence but never used as the payload start. Separately simulate an earlier external resource whose simplified basename matches and a later resource whose full path exactly equals a nonempty `m_Resource.m_Source`; require the declared offset/size region there. Assert only the selected source-bound region is visible to `stream_audio`, remains readable during the call, and is released/restored after success, error, and interruption. No later clip may observe a stale closed entry.

- [ ] **Step 3: Implement the minimal `unity-rs-core` adapter**

  Open and index a verified bundle once in strict-version mode. Use a bounded AudioClip TypeTree projection to read and validate path ID, name, `m_Resource.m_Source`, `m_Resource.m_Offset`, `m_Resource.m_Size`, channels, sample rate, sample count, and bit width. Cross-check path ID/name against the high-level core object. For an empty source, range-check and record the declared offset as approved-layout evidence but construct the expected region from the bounded parser position immediately after the resource fields plus declared size. For a nonempty source, resolve exactly one approved verified full path rather than the core's basename fallback and construct the expected region from declared offset/size. In both branches require source identity, start, and length to equal the core `AudioClipAsset.payload`; mark metadata complete only when the fields and codec justify exact sizing. Expose materialized script metadata and payload-free AudioClip locators, and stream one locator at a time through the pinned pure-Rust audio path. Convert dependency errors into the stable local error taxonomy without message-string matching.

- [ ] **Step 4: Run the mandatory real compatibility probe**

  Use the exact approved bounded pair and ignored evidence file. Assert:

  - script bundle parsing covers every expected object;
  - voice bundle exposes exactly 1,753 AudioClip refs;
  - all AudioClip TypeTree projections cross-check high-level path ID/name and declared resources resolve exactly;
  - every observed codec/layout signature has at least one representative streamed to a valid bounded WAV;
  - representative WAV property/hash records match the ignored Python baseline;
  - the voice bundle open counter is exactly one for the pass;
  - peak live decoded/canonicalizer buffering remains within the configured bound and never duplicates a full clip;
  - full approved input hashes are unchanged.

  Run:

  ```bash
  test -n "${MGWT_APP_PATH:-}"
  cargo test --test real_unity_probe --locked -- --ignored --nocapture
  ```

  Expected: PASS. Any observed signature without a successful representative, or any unsupported layout/codec/API gap, retains non-proprietary diagnostics, stops the plan, and requires an explicit design revision. Do not add a fallback.

- [ ] **Step 5: Verify, review, and commit Task 3**

  Run all locked checks and obtain an independent review focused on the real API, bounds, source regions, resource lifetime, and exception boundary. Resolve findings and guarded-commit `feat: add bundle scoped Unity reader`.

### Task 4: Parse Naninovel dialogue, normalize text, and resolve speakers

**Files:**
- Create: `src/naninovel.rs`
- Create: `src/normalize.rs`
- Create: `src/speaker.rs`
- Modify: `src/model.rs`
- Modify: `src/lib.rs`
- Test: `tests/naninovel.rs`
- Test: `tests/normalization.rs`
- Test: `tests/speaker.rs`
- Test: `tests/real_script_inventory.rs`

**Interfaces:**
- Consumes: `RawScriptObject` and `ApprovedSpeakers`.
- Produces: `ScriptParseResult`, `ParsedScriptReference`, `ScriptReference`, `CandidateRejection`, `NormalizationResult`, `SpeakerResolution`, and `resolve_script_references`.

- [ ] **Step 1: Port only synthetic structural fixtures and write RED parser tests**

  Cover at least three proven embedded relations and differing command positions. Pin unreadable top-level failure; unsupported voice-shaped command rejection; zero/multiple dialogue ambiguity; duplicate/missing RefIds; text-map mismatches; known safe blocking IDs; invalid `VoicePath`; bounded logical paths; indexed neighbor classes; and ignoring ordinary non-voice commands as candidates.

- [ ] **Step 2: Write RED normalization tests**

  Pin the approved ruby/control markup, line-break/whitespace rules, operation order, lexical requirement, NUL/control rejection, bounded bytes, and no Unicode/fuzzy normalization of voice IDs beyond proven wrappers and a case-consistent extension.

- [ ] **Step 3: Write RED speaker tests**

  Assert finite allowlist membership, author/voice agreement, unknown/absence/conflict rejection, exact 26 approved IDs, and no automatic expansion from observed input.

- [ ] **Step 4: Implement parser, normalizers, and speaker resolution**

  Preserve raw evidence separately from normalized values. One malformed source command produces one rejection; `blocking_group_ids` is a sorted unique tuple and `group_id` is set only for a singleton. Never serialize real text in committed fixtures or logs.

- [ ] **Step 5: Run the full metadata-only real script inventory**

  Parse all 24 approved script bundles without decoding audio. Assert every top-level object parses, all voice-shaped sources are accounted for, the approved speaker set does not drift, and aggregate/per-ID evidence digests equal the reviewed config. Store only ignored aggregate diagnostics.

- [ ] **Step 6: Verify, review, and commit Task 4**

  Run all locked checks and request an independent parser/normalization/accounting review. Resolve findings and guarded-commit `feat: parse Naninovel voice references`.

### Task 5: Implement global exact matching and preliminary accounting

**Files:**
- Create: `src/matching.rs`
- Modify: `src/model.rs`
- Modify: `src/lib.rs`
- Test: `tests/matching.rs`

**Interfaces:**
- Consumes: resolved `ScriptReference`, metadata-only `AudioAssetRef`, and prior typed rejections.
- Produces: `MatchPlan`, `PendingMatchGroup`, `MatchAuditRecord`, `MatchGroupExplanation`, and complete preliminary source accounting.

- [ ] **Step 1: Write RED group/join tests**

  Pin global rather than chapter-local matching, deterministic bytewise ordering, normalized repeated-text agreement, speaker conflict, missing sides, invalid audio IDs, same-ID multiple assets left pending, and no eligible candidates in one bundle being nonfatal. Keep the distinct health cases “no eligible decode attempts” (nonfatal by itself) and “eligible attempts exist but every decode fails” (fatal at resolution).

- [ ] **Step 2: Write RED blocker/accounting tests**

  A single prior rejection with multiple `blocking_group_ids` must reject every related group while remaining one source rejection. Reject duplicate candidate identities, unknown terminal stages, unaccounted sources, and double-accounting.

- [ ] **Step 3: Implement the pure deterministic matcher**

  Keep audio undecoded and matched groups pending. Choose the earliest valid terminal stage by the fixed stage order and retain original typed source reasons plus per-group explanations.

- [ ] **Step 4: Verify, review, and commit Task 5**

  Run all locked checks, obtain a fresh matching/accounting review, resolve findings, and guarded-commit `feat: match exact voice candidates`.

### Task 6: Decode, canonicalize, and stage audio by bundle

**Files:**
- Create: `src/audio.rs`
- Create: `src/disk.rs`
- Modify: `src/model.rs`
- Modify: `src/unity.rs`
- Modify: `src/lib.rs`
- Test: `tests/audio.rs`
- Test: `tests/disk.rs`
- Test: `tests/real_audio_probe.rs`

**Interfaces:**
- Consumes: `MatchPlan`, `UnityBackend`, validated output/app roots, and disk limits.
- Produces: `ResolvedDataset`, canonical staged WAV paths, `AudioProperties`, `DuplicateAlias`, terminal audit records, exact planned WAV bytes, streaming peak-memory receipts, and owned-staging cleanup/retention receipts.

- [ ] **Step 1: Write RED RIFF/WAV tests**

  Cover complete chunk walking, duplicate/missing chunks, odd-byte zero padding, integer PCM 8/16/24/32, float/compressed rejection, format-field consistency, empty audio, overflow, canonical idempotence, exact SHA-256, safe deterministic filenames, and the 256 MiB canonical-WAV ceiling. Add forged RIFF/FSB lengths and decompression-expansion cases that fail before oversized allocation.

- [ ] **Step 2: Write RED bundle-scheduled decode tests**

  Use interleaved voice IDs across two fake bundles. Assert exactly one open per bundle, bounded streaming with no second full clip, measured peak live bytes, candidate failure followed by success, infrastructure failure abort, per-clip disk callback, staged file cleanup, and original voice-ID finalization order. Separately prove a voice bundle with eligible attempts whose every decode fails is fatal, while a bundle with no eligible attempts is not.

- [ ] **Step 3: Write RED duplicate/conflict tests**

  Pin same-ID identical aliases, same-ID differing hashes, cross-ID compatible deduplication, cross-ID transcription/speaker conflict, lexical canonical source choice, per-alias provenance, and terminal source equations.

- [ ] **Step 4: Implement measurement and staging passes**

  Group actual decode work by approved bundle. Exact metadata clips require no measurement decode only when TypeTree fields and codec make the exact canonical size provable. Incomplete clips open their bundle once in measurement; staging opens each needed bundle once separately. Stream decoder output into a bounded canonicalizer/hash/file sink, release per-clip state before advancing, and retain only staged path/hash/properties metadata for later group finalization. Never materialize decoder output and canonical WAV simultaneously.

- [ ] **Step 5: Implement exact disk reservation**

  Calculate canonical WAV bytes including RIFF padding, verified-snapshot peak working-set bytes, shard allowance, deterministic sidecar allowance, and safety margin. Recheck immediately before snapshot creation, measurement decode, staging decode, and shard flush. Apply all global hard ceilings to declared lengths and observed streaming output. Disk/I/O/allocator/limit errors are fatal, not candidate rejections.

- [ ] **Step 6: Run the real bounded audio probe**

  Inventory codec/layout signatures across all eligible matched clips, stream at least one representative for every signature, compare ignored Python WAV hashes/properties, assert one bundle open per pass, and prove complete pre/post app inventory checkpoint equality. Any eligible signature with no successful representative fails the compatibility gate.

- [ ] **Step 7: Verify, review, and commit Task 6**

  Run all locked checks and an independent review focused on memory, bundle ordering, decoder errors, disk math, duplicates, and cleanup. Resolve findings and guarded-commit `feat: resolve canonical voice audio`.

### Task 7: Write deterministic Parquet, sidecars, and independent readback

**Files:**
- Create: `src/schema.rs`
- Create: `src/parquet.rs`
- Create: `src/report.rs`
- Create: `pyproject.toml`
- Create: `uv.lock`
- Create: `tools/verify_hf_interop.py`
- Create: `tests/fixtures/hf-schema-reference.json`
- Modify: `src/model.rs`
- Modify: `src/lib.rs`
- Test: `tests/parquet.rs`
- Test: `tests/report.rs`
- Test: `tests/readback.rs`

**Interfaces:**
- Consumes: replayable `ResolvedDataset`, `InventorySnapshot`, source/config revision hashes, and shard/disk callbacks.
- Produces: `ShardWriteResult`, deterministic README/JSONL/JSON, `BuildSummary`, `FileManifest`, and `verify_local_dataset(path) -> VerificationResult`.

- [ ] **Step 1: Write RED exact-schema and layout tests**

  Assert six columns in exact order/types/nullability, exact nested Hugging Face audio struct field order, pinned Hugging Face schema metadata, null emotion, `ja`, safe speaker directories, no extra paths, 500 MiB target behavior, rejection of one audio value above 256 MiB, byte/row-bounded Arrow batches, and deterministic multi-shard names/order. Compare the schema and metadata byte-for-byte with a committed dialogue-free synthetic Python reference fixture.

- [ ] **Step 2: Write RED sidecar/accounting tests**

  Pin every accepted occurrence, own audio provenance, alias/canonical linkage, filtered alias occurrences, stable rejection stages/reasons/blockers, candidate equations, shard totals, source/config/lock/dependency revisions, exact publishable file set, and sorted JSON/JSONL serialization without timestamps.

- [ ] **Step 3: Implement streaming deterministic writers**

  Hash files while writing where possible. Do not materialize a whole shard or row group, and enforce the global metadata/string/audio/object ceilings before Arrow allocation. Keep README free of game dialogue. Produce a pre-report manifest and terminal report without recursive self-hashing.

- [ ] **Step 4: Write RED coordinated-tampering/readback tests**

  Rewrite report/audit/manifest consistently while corrupting an alias link, row field, WAV byte, hash, row location, ordering, schema, or file set. Independent readback must still fail. Test symlinks, unknown files, truncated pages, and same-size byte corruption.

- [ ] **Step 5: Implement fresh-file streaming verification**

  Reparse serialized sidecars and Parquet. Stream every row/WAV and recompute canonical properties/hash, normalization, provenance relationships, ordering, totals, and file hashes. The verifier must not trust builder memory or a footer-only check.

- [ ] **Step 6: Run the locked offline Hugging Face interoperability gate**

  Generate a Rust-written synthetic shard containing no proprietary content, then run the locked development-only Python verifier with network access disabled. Require `datasets` streaming load to infer the pinned `Features`, recognize `audio` as the intended Audio feature, and read all six fields. The script must refuse real-app paths and is never called by the production CLI.

- [ ] **Step 7: Verify, review, and commit Task 7**

  Run all locked checks and an independent Parquet/schema/determinism/readback review. Resolve findings and guarded-commit `feat: write verified Parquet datasets`.

### Task 8: Orchestrate atomic builds and finish the CLI

**Files:**
- Create: `src/build.rs`
- Modify: `src/cli.rs`
- Modify: `src/main.rs`
- Modify: `src/lib.rs`
- Modify: `README.md`
- Test: `tests/build.rs`
- Test: `tests/cli.rs`
- Test: `tests/bounded_build.rs`

**Interfaces:**
- Consumes: all previous modules.
- Produces: production `inspect`, `build`, and `verify`; ordered phase reports; `BuildOutcome`; retained-staging failure evidence; and one exclusive final promotion.

- [ ] **Step 1: Write RED preflight and phase-order tests**

  Assert manifest/config/migration/spec/lock/dependency revision validation precedes Unity parsing; output/app/staging separation; dirty/uncommitted producer rejection for provenance-bearing real builds; and fixed phase ordering through final input rehash. Verify every parser receives the `VerifiedInputFile` handle or its owned verified snapshot, never a freshly resolved source pathname.

- [ ] **Step 2: Write RED failure/atomicity tests**

  Cover input component/leaf swaps after validation and during a pass; output appearing during work; destination parent becoming missing/file/symlink or being swapped before promotion; staging source disappearance; partial audio/shard writes; snapshot reservation/cleanup failure; disk check failure; cleanup failure; SIGINT/SIGTERM before rename; signal after rename; and panic-safe cleanup. A missing or identity-mismatched requested parent must fail before rename into the old/detached directory, leave the requested final path absent, and report owned staging. Before successful rename final is absent; after successful rename outcome is success.

- [ ] **Step 3: Implement the build state machine**

  Use a bound output-parent directory capability for output-adjacent unique staging and the Task 1-proven exclusive no-clobber directory-relative rename through a reviewed safe crate API. Store the parent-of-parent capability, requested parent leaf, and original descriptor identity; immediately before promotion, reopen that leaf no-follow and require the same identity. Make unsupported target semantics a build-time/platform gate; do not introduce first-party `unsafe`. Add cancellation polling, exact disk planning, inventory checkpoint checks, typed phase errors, retained owned paths on failure, and terminal readback. Never overwrite or recursively delete an unowned path.

- [ ] **Step 4: Implement production CLI behavior and documentation**

  Add JSON/human diagnostics without dialogue or credentials, exact exit codes, progress by phase/bundle/count, recovery instructions for retained staging, Japanese-first README with English summary, legal-use notice, schema/layout examples, resource estimates, and no promise of automatic Hugging Face publication.

- [ ] **Step 5: Run the bounded two-build integration gate**

  From a clean committed producer in an isolated run-owned workspace, build the approved script/voice pair twice with empty owned caches and network disabled. Assert one open per bundle per pass, all terminal equations, exact six-column/readback success, equal deterministic file maps, exact producer revision in both reports, and unchanged full app inventory. Record elapsed time and peak RSS as diagnostics; do not impose a machine-specific wall-clock threshold.

- [ ] **Step 6: Verify, review, and commit Task 8**

  Run all locked checks, request an independent whole-orchestrator safety review, resolve findings with RED/GREEN tests, rerun both bounded builds after any producer change, and guarded-commit `feat: orchestrate atomic dataset builds`.

### Task 9: Whole-project review, full extraction, and remote delivery

**Files:**
- Modify: only files required by accepted review findings.
- Generate but never commit: a full local dataset in an explicitly chosen ignored output directory.
- Generate but never commit: redacted verification evidence under an ignored local evidence directory.

**Interfaces:**
- Consumes: clean committed Task 8 producer and authorized local app.
- Produces: independently verified full local dataset, final reviewed source commit(s), and published GitHub `master` source history.

- [ ] **Step 1: Obtain two independent whole-project reviews**

  Attempt required LocalGPT and Claude Code MCP reviews first. If unavailable, record that and use two separate fresh read-only `gpt-6-sol` reviewers. Keep reviews independent. Focus on Unity assumptions, content safety, candidate equations, bundle/memory/disk bounds, WAV/Parquet compatibility, atomicity, and missing tests.

- [ ] **Step 2: Resolve every material finding with TDD**

  Add a failing regression first, make the smallest fix, rerun owning tests, and request re-review. Apply guarded commits before producing final artifacts. Any later source/config/lock/spec change invalidates bounded/full evidence.

- [ ] **Step 3: Run the complete clean verification suite**

  From a clean producer commit run:

  ```bash
  cargo fmt --all -- --check
  cargo clippy --all-targets --all-features --locked -- -D warnings
  cargo test --all-targets --all-features --locked
  cargo doc --no-deps --locked
  test -n "${MGWT_APP_PATH:-}"
  cargo test --locked -- --ignored --nocapture
  ```

  Expected: all pass, including the two-run bounded gate; source/app/config hashes and bundle-open counts match.

- [ ] **Step 4: Execute the full local build once**

  Require final output absent, exact clean producer HEAD, unchanged reviewed config/lock/spec, sufficient disk space, and a complete pre-build app inventory. Run `build --shard-audio-mib 500` against all 24 script and 35 voice bundles. Expected: exit `0`, status `complete` or nonempty `complete_with_rejections`, at least one accepted row, all eligible-bundle success floors met, and atomic final output only after readback.

- [ ] **Step 5: Independently verify full output and app immutability**

  In a fresh process run `verify`, independently count JSONL records, inspect every Parquet schema/footer, stream every row/WAV, validate every file hash and equation, require zero pending candidates, exact `data/ja/<character_id>` layout, no stale/extra paths, and exact pre/post approved app inventory checkpoint equality. Confirm report producer/config/lock/dependency revisions.

- [ ] **Step 6: Run the full redacted Python semantic differential**

  Against the ignored reviewed Python baseline, compare per-bundle script/object/AudioClip parse counts, voice-shaped candidate equations and terminal totals, normalized row identities and transcription hashes, speaker IDs, canonical WAV hashes/properties, and rejection stage/reason distributions. Parquet container bytes may differ across implementations, but Rust's two clean builds must have identical file maps. Any difference outside the explicitly mapped implementation-neutral reason taxonomy is unexplained and blocks acceptance; save only redacted aggregate/hash evidence under the ignored evidence directory and obtain review of any proposed mapping change.

- [ ] **Step 7: Apply the final commit and push gates**

  Re-read the identity skill, fetch fresh human identity, validate every local commit in the unpublished history, confirm origin is exactly `https://github.com/tkgstrator/mgwt-voice-dataset-builder.git`, branch is attached `master`, worktree/index are clean except ignored generated evidence, and remote `master` remains absent or at the expected known parent. Use guarded-git for one normal `master` push. Do not resend automatically after an uncertain result.

- [ ] **Step 8: Verify remote delivery**

  Read back `refs/heads/master`, default branch, commit OIDs/parents/raw human identities/messages, repository tree, README, workflow, pinned dependency revision, and absence of generated/proprietary files. Confirm GitHub Actions status if available. Report the exact source commit and any unverified CI state; do not claim the dataset itself was uploaded.

## Plan Self-Review

- **Spec coverage:** Every design section is owned: repo/CLI/path rules in Task 1; portable manifests and approval rebinding in Task 2; strict pinned Unity compatibility, TypeTree cross-checking, and one-open sessions in Task 3; Naninovel/speakers in Task 4; exact matching in Task 5; streaming audio/memory/disk in Task 6; exact Hugging Face schema/audit/readback/interoperability in Task 7; descriptor-relative atomic orchestration in Task 8; full semantic differential and GitHub delivery in Task 9.
- **Type consistency:** `ApprovedManifest`/`ApprovedSpeakers`/`ApprovalMigration`/`VerifiedInputFile`/`VerifiedBundleInputs` flow from Task 2; `UnityBackend`/`BundleSession`, `AudioAssetRef`, `AudioLimits`, and streaming receipts from Task 3; script/rejection models from Task 4; `MatchPlan` from Task 5; `ResolvedDataset` from Task 6; `BuildSummary`/`VerificationResult` from Task 7; `BuildOutcome` from Task 8.
- **Review-focus tests:** Each of the six global focus risks has an explicit owning task and test step.
- **Proportion:** The plan fixes interfaces, gates, and acceptance evidence without prescribing routine Rust function bodies. The compatibility gate occurs before downstream implementation so a dependency mismatch stops cheaply.
- **Delivery safety:** The plan never commits or publishes game data, never adds a non-Rust runtime fallback, and never pushes before clean independent verification.
