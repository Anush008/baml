# BEX Event-Stream Design — Gap Review

**Reviewed doc:** `bex-event-stream-design.md` (1124 lines, unchanged across both review passes)
**Method:** Two multi-agent workflow passes (64 agents total, ~4.5M tokens). Each candidate gap was found by one agent and then adversarially verified by a second agent against the actual repo code *and* the doc text, to suppress misreadings and anything the doc already flags as an open question/risk.
**Repo:** `/Users/vbv/repos/baml-3/baml_language`, crates under `crates/`.

**Result:** 24 confirmed issues — **4 blockers, 16 majors, 2 minors** (plus factual citation corrections). The design's architecture (hot path / ring / reconstruction, §§1–8) is rigorous; the gaps cluster in **(a) cross-cutting identity & lifecycle** and **(b) the observability output layer** (markers, artifact, export, future tools), where the spec thins into claims that lack a mechanism.

> **Scope note:** You explicitly OK'd ripping out all of Collector + bex_events + existing tracing. These findings assume that. "Product breakage" items below are about *sequencing/communication* of that rip-out, not objections to it.

---

## Severity index

| # | Severity | Title | Doc § |
|---|---|---|---|
| 1 | **Blocker** | Multi-engine `engine_id` missing from wire / consumer resolution | §3.5, §4.2, §4.4, §5.7, §8.2, §9.1 |
| 2 | **Blocker** | Producer overflow mechanism (free-list + arena + stale `Dropped`) underspecified | §3.1, §3.4, §3.6 |
| 3 | **Blocker** | Consumer thread lifecycle undefined in embedded (PyO3/LSP) mode | §4.2, §4.11 |
| 4 | **Blocker** | GC markers can't be attributed (global STW vs per-thread model) | §4.6, §6.0, §6.1, §6.6 |
| 5 | Major | Spawned bodies: `unconditional emit` (§2.5) vs `emit no Start/End` (§6.4) contradiction | §2.5, §4.11, §6.4 |
| 6 | Major | Unwind paths assert FN_END emission but specify no mechanism; test cites deleted fn | §2.5, §8.1, §10.3 |
| 7 | Major | Inline deep-copy (§2.5) vs engine-side-after-yield (§7.4) — which, and GC-permit safety | §2.5, §7.4 |
| 8 | Major | `bex_thread_id` width: `u32` in header vs `u64` minted | §3.0, §3.5, §10.1.1 |
| 9 | Major | Clock source: "process-monotonic" claimed but `SystemTime` (non-monotonic) defaulted | §3.5, §4.5, §8.4 |
| 10 | Major | `drain_to_quiescent()` coordination mechanism unspecified | §8.7 |
| 11 | Major | Null-`LOCAL_RING` TLS deref unguarded | §3.2, §3.4, §10.1.2 |
| 12 | Major | "Lossless" conflated with "unbounded growth → OOM hard-crash"; throughput unproven | §3.6, §3.7, §4.10, §11 |
| 13 | Major | SDK `Collector` API break: no deprecation/migration path | §8.1, §8.5, §8.9 |
| 14 | Major | Host FFI wire deferred: no timeline/deprecation; product tracing goes dark | §1.4, §8.1, §8.5, §12 |
| 15 | Major | Firefox/speedscope export has no sample source at M4 (call-tree vs sample model) | §9.1, §9.3, §12 |
| 16 | Major | Per-function capture mask has no defined home; §2.5 code omits the OR | §2.5, §5.4, §7.2, §7.3 |
| 17 | Major | Compile-time "zero overhead" excision contradicts always-on; no path | §1.2 (G1), §2.7, §11, §12 |
| 18 | Major | ~10ns floor omits 2 timestamp reads + shadow-stack cost | §2.7, §3.5, §8.4, §10.2.1 |
| 19 | Major | `Program` borsh change breaks on-disk artifacts; no version strategy | §5.4, §9.2, §12 (M0) |
| 20 | Major | `zstd` "zero new deps" (§9.3) vs "net-new dep" (§11) contradiction | §9.2, §9.3, §9.4, §11 |
| 21 | Major | `schema_version` required by type but absent from wire | §3.5, §6.0, §6.7 |
| 22 | Major | Marker tick can fall outside enclosing frame's lifetime | §4.6, §6.2, §6.4, §6.6 |
| 23 | Major | Runtime feature toggles → partial trees; consumer/diff contract unspecified | §2.4, §4.8, §9.3 |
| 24 | Minor | FQN diff-key uniqueness unvalidated (lambdas, generics) | §5.6, §9.3, §10.3 |
| — | Minor | Export shapes not validated against actual Firefox/speedscope schemas | §9.3, §9.4 |

---

## Factual citation corrections (the doc's own references)

The doc's self-corrections are mostly accurate, and pass-2 confirmed several load-bearing claims hold (GC-timing window at `lib.rs:1154`/`:1167`/`:1175`; `size_of::<Object> ≤ 80` fit; `EarlyYieldCheck` "no per-call atomic"; `Usage::default()` hardcoding; `FunctionKindWire` proxy). But verification caught these:

- **More runtime-construction sites than the three named (`bridge_cffi:64`, `run_command.rs:656`/`:936`).** At least four more exist and would each need the `on_thread_start` hook or their workers get a null ring:
  - `baml_cli/src/pack_command.rs:521` (`download_release_asset`)
  - `baml_lsp_server/src/lib.rs:99` (`run_server`)
  - `baml_pack_host/src/main.rs:250` (`run_single`, likely `run_subcommand`)
  - `bex_project/src/bex_lsp/multi_project/wasm_helpers.rs:78` (`OnceLock` fallback runtime)
- **The wasm fallback cites a non-existent path.** §4.2 says "wasm keeps the in-band `emit()` path (`lib.rs:2031-2032`)" — but those lines are just the `cfg(wasm32)` `spawn_local` vs `tokio::spawn` split, **not** an emit path, and `emit()`/Collector are deleted by §8.1. **wasm32 has no defined event path in this design.** (Real hole, see also #11.)
- **Minor stale line numbers (substance intact):** enter arm is `vm.rs:2629-2686` (not 2626-2683); `OpCode::Return` is `:4317` (not 4315); the "no string interner" claim is correct but the `types.rs:1616` TODO attribution is wrong (that line is a doc comment on `Object::String`).

---

## BLOCKERS

### 1. Multi-engine `engine_id` is "resolved" in prose but absent from the entire data path

§5.7/Q8 says cross-engine collisions are handled by pairing `{engine_id, function_id}`, and §11 marks Q8 **resolved**. The data path contradicts this:

- The 16-byte header (§3.5) has no `engine_id` field: `tag u8 | flags u8 | payload_len u16 | bex_thread_id u32 | ts_nanos u64` = exactly 16, zero slack.
- `TAG_FN_START` carries only `function_id u32`.
- Reconstruction (§4.5) keys on bare `FunctionId` (`HashMap<FunctionId, FnAggregate>`), no engine scoping.
- The registry is `Vec<Option<FunctionMeta>>` indexed by `FunctionId.0` (§5.4), assuming global uniqueness.
- ProfileState (§9.1) has one flat `functions` vec.

Because the engine uses the **ambient** tokio runtime (`lib.rs:2030`), two `BexEngine`s share the same per-OS-thread ring, both mint `FunctionId(1)` (per-compilation `NEXT_FUNCTION_ID`), and the consumer cannot tell `user.A.func` from `user.B.func`. **Markers have the same problem** — two engines' concurrent GC markers carry no discriminator.

**Fix:** Put `engine_id` on the wire (header → 20 bytes is fine at ~32-byte records), key reconstruction by `{engine_id, bex_thread_id}`, hold `HashMap<EngineId, Arc<FunctionRegistry>>` in the consumer, stamp `engine_id` at `lib.rs:2135-2136` alongside `bex_thread_id`. Thread it end-to-end through §3.5 → §4.4 → §9.1 → §9.3 with a two-engine resolution test. *Or* explicitly forbid multi-engine and demote Q8 to a documented assumption (remove the "resolved" claim at line 1093 and the cross-engine language at lines 24/850).

### 2. Producer overflow mechanism — the thing G0 rests on — is underspecified and self-contradictory

Three coupled problems, all in the path that guarantees losslessness:

- **Stale `Dropped` pseudocode.** §3.4's `push_record` still does `if !self.swap_active() { return PushResult::Dropped; }`, directly contradicting §3.6's lossless-by-growth ("never drops"). The `DoubleBufferedRing` struct (§3.1) is `halves: [Half; 2]` with **no overflow/free-list fields** to "link a fresh segment." The pseudocode and the prose disagree, and the doc never flags it.
- **Cross-thread "thread-local" free-list.** §3.6 says overflow segments come from a "thread-local free-list" that **the consumer returns to** — that's cross-OS-thread access to a thread-local structure with no specified synchronization, while simultaneously claiming the consumer path is "100% heap-permit-free." Is it a global lock-free MPSC? A per-producer SPSC return channel? Undefined.
- **Arena lifecycle entirely absent.** Value records (`TAG_FN_ARGS`/`RESULT`/`ERROR`) carry `(arena_off, len)` into "the per-thread arena," but nothing says who allocates it, when it's freed, or how its lifetime couples to ring-half recycling. §4.3 frees the half "fast" after draining — if the arena lives in/with the half, the offsets dangle → **use-after-free**. Not even in §11.

**Fix:** Rewrite §3.4 to show overflow allocation on swap failure (never `Dropped`) and add the fields to the struct. Add a §3.6.1 specifying the free-list (global lock-free MPSC vs per-producer return channel) and the arena (allocation site, free point, and the guarantee that arena bytes are copied into consumer-owned scratch *before* the half is recycled). Move the loom/miri work *ahead* of M2 implementation — today it's "discover during coding," risky for the correctness keystone.

### 3. Consumer thread lifecycle is undefined for the embedded case — which is the only case that matters

§4.2 mandates a pinned process-global `std::thread` with `core_affinity`, but the doc never says:

- **Who spawns it** (the three `on_thread_start` hook sites are producer-side; no consumer-spawn site or symmetric shutdown hook is given).
- **Who calls `on_shutdown`** (it's on the `ProfileSubscriber` trait with no owner).
- **How it tears down under PyO3/LSP.** The existing `NativeEventSink` has an explicit `flush()` contract the bridges call ("Callers must call `flush()` before process shutdown"); the replacement has no equivalent ownership. Pinning a thread to a core inside someone else's Python process at import is intrusive.

For a library this is a resource-leak / lost-tail-data bug, not a detail. §10.1.2 marks the sync-caller lifecycle "future," but the async embedded shutdown problem is present-tense.

**Fix:** Name the owner (`bex_events::init_consumer()` called by bridge/CLI, or bridge-owned with explicit shutdown duty). Add a symmetric `on_shutdown`/`on_process_exit` hook mirroring the three startup sites. Specify PyO3 (atexit handler?) and LSP (reload → drain old consumer before new) teardown. Add M2/M3 acceptance gates that `on_shutdown` fires on normal exit in `bridge_cffi`/`baml_cli`.

### 4. GC markers can't be attributed to threads (global STW vs per-thread model)

The marker model is per-`(bex_thread_id, tick)`, attached to "the currently-open frame" (§4.6/§6.0/§6.6). But `collect_garbage` (`lib.rs:1091`) is one **global stop-the-world** operation; the `checking_gc` CAS means exactly **one** VM emits the marker, into **one** thread's ring with that thread's `bex_thread_id`. Consequences:

- The GC pause appears in only the triggering thread's timeline. Every other parked thread shows an unexplained gap with no marker — the user can't see why B/C went idle.
- "Attach to the currently-open frame" is undefined when N threads each have an open frame during the pause.
- No broadcast mechanism exists: inside `collect_garbage` you have one `bex_thread_id`, no per-thread visibility (permits parked at `:1094`, contexts severed at `:1800`/`:1823`).

This is categorically different from LLM/HTTP/FFI/Sched markers, which *are* tied to one thread's context.

**Fix:** Pick a model explicitly in §6.1 + §4.6 + the marker wire format:
- **(A, simplest)** Emit one global marker with a sentinel `bex_thread_id` (`ROOT_THREAD_ID`/`u32::MAX`); the consumer projects it onto all timelines live at that tick.
- **(B)** Broadcast into every parked thread's ring (needs the parked-VM set + per-ring writes + ordering).
- **(C)** Each thread self-emits a "GC pause" marker on resume (loses precise STW timing — state the trade-off).
Add a 3+-thread test asserting the pause is visible with equal duration in every thread's profile.

---

## MAJORS

### 5. Spawned bodies: unconditional-emit vs emit-nothing contradiction

§2.5 makes structural enter/exit **unconditional** (`ring_push_fn_start`, "ALWAYS, no flag check"). §6.4 says spawned bodies (`span_state = None`, `lib.rs:2006`) "emit **no** FunctionStart/End." §4.11 says the consumer "closes any frame still open" for spawned threads (implying they *do* emit). No suppression mechanism exists in the §2.5 path (`FunctionId::SENTINEL` is for synthesized fns, not spawned bodies). Unmatched FN_START/FN_END breaks reconstruction and violates the §2.5 balance invariant + G0. **Fix:** pick one — gate the push for spawned roots, or accept they emit and fix §6.4/§4.11 to match — and make §2.5/§4.11/§6.4 consistent.

### 6. Unwind paths assert FN_END emission but specify no mechanism

§2.5's balance invariant requires one FN_END per frame popped during exception unwinding (`vm.rs:2202`, `:2290` — verified real `frames.pop()` sites). But §2.5 shows emit code only for `OpCode::Return`; the unwinder gets none, and M3 (§12) lists "unwind paths" with no detail. Worse, the M3 acceptance test (§10.3) says "`emit_error_function_end_events` balances the span stack" — but §8.1 **deletes** that function, so the test criterion is undefined. **Fix:** show the `ring_push_fn_end(Error)` pattern at each `frames.pop()` (handling Native frames that own no function), and rewrite the M3 acceptance test to not reference a deleted function.

### 7. Inline deep-copy vs engine-side-after-yield — and GC-permit safety

§2.5 shows the `as_owned_for_trace` capture deep-copy **inline in `step_compact`** (holding the `ActiveHeapPermit`); §7.4's "Correction" says capture is **engine-side after a yield**. The doc never reconciles which the *new* design uses. If inline, a recursive deep-copy of the heap graph while holding the permit reopens the exact GC-park interaction §3.6 spent effort closing (the permit doesn't release until an await the deep-copy never reaches). **Fix:** state which path is canonical; if inline, add the blocking-safety analysis (the `HeapPtr`-stability argument covers *escape* safety, not *blocking* safety) or move the copy post-yield and update the §2.5 snippet.

### 8. `bex_thread_id` width mismatch

§3.0/§3.5/§3.1 say `u32`; §10.1.1 says "mint a monotonic `BexThreadId(u64)`." Storing the u64 in the u32 header silently truncates the high 32 bits → collisions, breaking the work-stealing demux the whole reconstruction depends on. Trivial fix, but unflagged. **Fix:** make them consistent (u32 is adequate for 1000+ threads; just correct §10.1.1) or widen the header field.

### 9. Clock source contradicts itself

§3.5 calls `ts_nanos` "process-monotonic," but §8.4 defers to `SystemTime::now()`, which is **not** monotonic (NTP/clock steps go backward). After a migration, an inverted tick makes `wrapping_sub` (§4.5) produce a near-`u128::MAX` inclusive time — silent G0 corruption (the `wrapping_sub` defends against arithmetic overflow, not sequence inversion). **Fix:** mandate `std::time::Instant` for the tick (monotonic across migrations); state explicitly that reconstruction orders by **ring position, not tick value**; add inversion detection (log/clamp/error rather than silently wrap). Note: cross-thread tick comparison is *not* required — the doc regroups by `bex_thread_id` first — so the requirement is only per-thread monotonicity, which `Instant` gives for free.

### 10. `drain_to_quiescent()` coordination mechanism unspecified

This is the replacement for the old synchronous global lock, used wherever a caller needs a complete result right after a call (§8.7, §10.3). The doc describes the *effect* ("consumer catches up to the last produced record") but no *mechanism*: how the producer publishes "I wrote up to tick X," how the caller waits, blocking semantics/timeout, dead-consumer handling, and whether the caller's own last record (still in an un-swapped active half) is forced out. **Fix:** add §8.7.1 specifying the coordination primitive (e.g., per-ring published drain-position atomic + condvar), the FunctionRegistry-immutability precondition, blocking/timeout semantics, and failure modes.

### 11. Null-`LOCAL_RING` deref is unguarded

`push_record` (§3.4) dereferences `LOCAL_RING` with no null check. Any worker that skipped `on_thread_start` — a host-provided runtime, or one of the four un-cited runtime sites (see Factual Corrections), or the undefined wasm path — segfaults. Option A (lazy self-register) is marked "future," but the failure is present-tense. **Fix:** add a null guard (skip or lazy-self-register), or make Option A v1 scope, or add an explicit asserted invariant + a detector test.

### 12. "Lossless" quietly means "unbounded growth until a hard crash"; throughput unproven

Always-on + lossless-by-growth under sustained producer>consumer grows overflow until `BAML_RING_MAX_OVERFLOW_BYTES`, then it's a **hard error that kills the user's program**. Better than silent drop, but the doc conflates the two as equivalently fine. Consumer sharding (the real mitigation) is gated on a benchmark that doesn't exist (§4.10/Q3), and there are no throughput numbers (drain rate, per-record size, the 50ms-vs-4096-record cadence interaction) to let an operator decide if one consumer suffices. **Fix:** add the steady-state formula + a measured baseline drain rate + a sharding decision tree, and state plainly that hitting the cap = crash.

### 13. SDK `Collector` API break with no migration path

Deletion breaks the Python/TS SDK `Collector` API (`sdks/python/.../baml_core/__init__.py`, `test_collector.py`'s ~15 test classes, the TS equivalent) and the Rust `tests/tracing.rs`/`tests/event_system.rs`. The doc acknowledges "consumers break by design" but names no SDK files, offers no deprecation timeline, no warning shim, no migration guide. **Fix:** add a "Breaking Changes & SDK Migration" section naming the affected SDKs and mapping old `Collector` usage → the new `profile()` query API + `drain_to_quiescent()`.

### 14. Host FFI wire deferred with no timeline; product tracing goes dark

The protobuf-FFI event wire (`bridge_ctypes/src/event_encode.rs`), the LSP `playground_event_sink.rs` `EventSink`, and Studio/dashboard tracing all break, with the host wire "deferred to a separate future design" (§8.5). That means **customer-facing tracing has no replacement until that ships**. The doc covers the architecture but not the product transition: no timeline, no statement of whether the host wire is an M4 dependency or M4 ships with a known-dark window. **Fix:** add a §0.1 "Product Impact & Deprecation Plan" with the impact list, a concrete "deferred = which release," and a mitigation for users needing Studio events in the gap.

### 15. Firefox/speedscope export has no sample source at M4

`ProfileState` carries `samples: Vec<StackSample>` and §9.3 reprojects *samples* into Firefox's `frameTable`/`stackTable`. But SIGPROF (the only sample producer) is **M5**, while Firefox export is **M4**. The consumer (§4) only builds a call-tree (`FnAggregate`: counts + excl/incl ns), never samples. Firefox's `stackTable` is sample-point-indexed; you can't build it from nesting+duration without a synthesis step the doc never gives. **Fix:** decide — synthesize samples from the call-tree (specify the algorithm), ship a non-sample export at M4, or move Firefox export to M5. Also: the export shapes are never validated against the actual Firefox/speedscope JSON schemas (`schemaVersion`, `meta`, `libs`, `frames`, `stacks`...) — add a schema-mapping section + a golden "loads in profiler.firefox.com" test.

### 16. Per-function capture mask has no defined home; §2.5 code omits the OR

§7.2 says the 3-bit capture mask lives on `FunctionMeta` (off-heap registry); §7.3 says the decision is `engine_features | function_capture_mask` as "a Copy-value the VM already holds"; but §5.4's `FunctionMeta` struct **doesn't contain the field**, and §2.5's code only checks `self.features.contains(...)` with **no OR**. These can't all be true: reading the mask from the off-heap registry per-Call contradicts Commitment 4 ("carry only `FunctionId`, never deref"), while "a value the VM already holds" implies it's on the heap `Function` (replacing `Function.trace`, `types.rs:435`). **Fix:** pick one location, add the field where it actually lives, and fix the §2.5 snippet to show the OR — else a function with per-function capture on but engine-global off is silently not captured.

### 17. Compile-time "zero overhead" excision contradicts always-on and has no path

G1/§2.7/§11/§12 promise "a cargo feature can excise the whole subsystem." But structural push is **unconditional, inline in `step_compact`** (§2.5, Commitment 2). You can't have unconditional inner-loop code that's also `cfg`-gated to nothing without `cfg`-gating the inline `ring_push` — which violates the doc's own "tight inner loop is untouched." No such feature exists in any Cargo.toml, it's never named/scoped, and `bex_vm` hard-depends on `bex_events`. **Fix:** either drop the claim (accept the ~10ns floor is permanent — the doc already calls it "the permanent cost") or actually design the feature (`bex_events = { optional = true }` + cfg-gated push + optional dep) and admit the loop *is* touched when off.

### 18. The ~10ns floor omits two real costs

(a) **Two timestamp reads per call** — FN_START and FN_END are separate records each carrying `ts_nanos` (§3.5), so two reads per call pair, not one; and §8.4 *defers* the clock source. If producer-side `SystemTime::now()` (~20–30ns/read on many platforms), that's 40–60ns/pair, 4–6× the claimed floor that justifies the ≤2% G7 target. (b) **SIGPROF shadow stack** (§10.2.1) — the doc never says whether it's maintained always (adding a `Vec` push/pop per call, uncounted in §2.7) or only when `SIGPROF_SAMPLE` is set (incomplete depth if enabled mid-run). **Fix:** resolve both; fold the real timestamp cost into the M3 bench instead of deferring; commit `Instant`-or-cheaper for the tick.

### 19. `Program` borsh change breaks on-disk artifacts; no version strategy

`Program` is `BorshSerialize` and embedded in `PackEnvelope` (`baml_exec/src/envelope.rs:59`), written to packed `.baml` binaries that `baml_pack_host` (`main.rs:39`) and bridges deserialize. M0 adds `FunctionRegistry` to `Program` → existing compiled artifacts fail to deserialize. The doc versions `.bamlprof` (§9.2) but specifies **nothing for `Program`**. **Fix:** add a `Program` `format_version` (or `Option<FunctionRegistry>` default-empty schema evolution), document read-path branching in `extract_envelope`, and cover it in M0's acceptance criteria.

### 20. `zstd` "zero new deps" vs "net-new dep" contradiction

§9.3 claims `baml_cli` has "zero new deps," but §9.2 puts optional `zstd` compression in the `.bamlprof` body, §9.4 puts the writer in a new `bex_profile` crate `baml_cli` depends on, and §11 lists `zstd` as net-new. If `.bamlprof` writing includes zstd and `baml_cli` writes it, zstd is transitive into `baml_cli`. **Fix:** defer zstd (drop the flag bit from §9.2) or correct the "zero new deps" claim and document the dependency graph.

### 21. `schema_version` required by the type but absent from the wire

Every `MarkerInterval` carries `schema_version: u16` (§6.0) and readers branch on `(category, schema_version)` (§6.7, Q9 "resolved") — but the wire (§3.5 `TAG_MARKER = 0x06 : marker_kind u8 [kind_payload]`) has no slot for it and §6.0 says markers carry "only `(marker_kind, payload)`." **Fix:** specify where `schema_version` sits (header vs first field of each payload), show the producer stamping it and consumer parsing it — otherwise Q9 isn't actually resolved.

### 22. Marker tick can fall outside its frame's lifetime

SysOp/Await markers are emitted *after* permit release and *after* re-acquire (`:2395`/`:2402`, `:2587`/`:2601`) — potentially ms and many nested calls later (LLM/HTTP latency). The consumer's stack-based "attach to currently-open frame" can then attach to the wrong frame or underflow, because the intended frame already popped. §4.6 (stack-based) and §6.6 (tick-based) are unreconciled. **Fix:** use tick-interval matching (find the frame whose `[enter_tick, exit_tick]` contains the marker tick) rather than top-of-stack; pin down exactly when each marker captures its tick; add a test that forces frame exits before the marker is processed.

### 23. Runtime feature toggles → partial trees; consumer/diff contract unspecified

§2.4 admits a `Relaxed` mid-run toggle drops/adds events and yields half-captured trees; §4.8 says payloads are "present only if the bit was set." But nothing says how readers or `baml profile diff` (G6) treat sparse data — silently accept, reject as incomparable, or require matching capture masks across the two runs. Without a rule, the CI diff gate compares unlike data. **Fix:** specify the contract (recommend: diff requires matching capture masks, else flags incomparable); mark payload fields `Option<...>` so absent ≠ null.

---

## MINORS

### 24. FQN diff-key uniqueness unvalidated

FQN is asserted as "the stable join key" for cross-run diff (§5.6/§9.3) with no collision detection. `ItemRef::to_string()` does no uniqueness check; lambdas format as `.<lambda(parent, idx)>` (empty package) vs `package.fn` for regular functions; monomorphized generics drop type params. Likely fine in practice (parser restricts identifiers), but a collision would make `diff` silently merge two functions. **Fix:** document the lambda/package differentiation; add an optional `--check-fqn-uniqueness` / detection stub that reports collisions with source locations instead of merging.

### Export shapes not validated against real schemas
(Folded into #15.) §9.3/§9.4 commit to Firefox + speedscope JSON but never cite or validate against the published schemas, and `profile_command.rs` doesn't exist yet. Add a schema-mapping subsection and conformance tests.

---

## Correctly dismissed (verified NOT gaps)

These were proposed by finder agents and killed by the adversarial verifiers — listed so you don't chase them:

- **Open-frame snapshot inconsistency** — aggregates only update on frame *exit* (§4.5/§4.7); open frames never appear in a snapshot, so a child exiting after a snapshot can't corrupt it. Design is correct.
- **CodSpeed gate blind to per-call cost** — already acknowledged (G7 line 79, §11, §12 M3) with a call-heavy canary planned. Flagged, not hidden.
- **`ts_nanos` wire-vs-consumer "misalignment"** — already an explicit open decision in §8.4.
- **COVERAGE bitmap gating** — properly deferred; bit 10 is defined, the gated `fetch_add` is specified.
- **`i64` relative-offset overflow** — `i64` ns ≈ ±292 years; non-issue. (The *undefined reference epoch* for `StackSample.t_ns` is a real micro-nit worth one sentence, but not a risk.)

---

## Pattern / takeaway

- **§§1–8 (hot path, ring, reconstruction, function identity)** are rigorously specified — the self-correction discipline against the originating ticket is genuinely strong, and pass-2 confirmed the load-bearing code claims hold.
- **The gaps cluster in two bands:** (1) *cross-cutting identity & lifecycle* that touches every layer (`engine_id`, `bex_thread_id` width, consumer lifecycle, null-TLS, clock source, `Program` versioning); and (2) *the observability output layer* (§§6/9/10 — markers, artifact, export, future tools), where the spec repeatedly asserts a property ("Q8 resolved," "schema_version on every marker," "compile-time excision," "export to Firefox") without a mechanism behind it.
- **None invalidates the approach.** The four blockers and the marker/export cluster are what to close before M2/M3, because they're load-bearing for G0 and for the embedded reality BAML ships into.

---

*Generated from two adversarially-verified multi-agent review passes. Line numbers are as of the reviewed commit; a few cited lines drift by ±2–60 (see Factual Corrections) but the substance was verified against the live code.*
