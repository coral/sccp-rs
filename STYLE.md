# Repository Style Guide

## The rules

These rules take precedence over the inferred conventions in the rest of this
guide:

- Do not downgrade dependencies.
- Build with the latest stable version of Rust.
- Prefer `match` over long `if`/`else if` chains.
- Prefer idiomatic Rust representations of protocol variation over parallel,
  version-specific structs.
- Do not add comments that merely restate obvious code.
- Do not keep around old structs / enums when refactoring, we don't want "BlaLegacy" for "compatibility purposes"
- Do not write tests for the sake of tests. Express structural guarantees in
  types; test ordering, failure, and lifetime behavior the compiler cannot prove.

## Design priorities

The code favors explicit contracts over convenience:

- Represent protocol, identity, state, and ownership distinctions with types.
- Validate and bound external data before it enters long-lived state.
- Keep domain policy independent of sockets, native APIs, and generated
  bindings.
- Make ordering, rollback, shutdown, and resource ownership visible in the
  API.
- Preserve unknown wire data when that can be done safely and losslessly; fail
  closed when it cannot.
- Keep diagnostics deterministic and free of secrets or opaque payload data.
- Test invariants, failure paths, and architectural boundaries as first-class
  behavior.

## Repository boundaries

### `sccp-protocol`

This crate owns the phone-facing protocol and server, not SIP or PBX policy.
Its public types describe semantic messages and validated application data;
private codec types describe byte layout, padding, and reserved fields. Model
protocol-version differences in shared types unless genuinely incompatible
wire layouts require separate representations.

- Keep frame parsing separate from typed message decoding.
- Keep the message catalog the single source of truth for identifiers,
  direction, layout, bounds, fidelity, runtime use, and expected responses.
- Keep station lifecycle and correlation rules in the server. Applications
  communicate through typed events, commands, and handles.
- Do not expose private wire structs merely to make an integration easier.

### `asterisk-module`

The public domain modules (`ami`, `call`, `config`, `http`, `media`, `pbx`,
`presence`, and `state`) must not depend on Asterisk bindings. The
backend-neutral controller in `runtime` owns state transitions and produces
ordered `DriverEffect` values. Traits describe the ports needed to execute
those effects.

The private `asterisk` module is the production composition root:

- `asterisk/adapters` implements domain ports.
- `asterisk/native` groups raw-edge operations by the native resource they
  own.
- `asterisk/direct` owns the actual Asterisk ABI entry points and ownership
  transfer.
- `asterisk/boundary` contains shared conversion, status, panic-containment,
  and synchronization policy.
- `asterisk/runtime` connects owned domain values to the native adapter.

Do not introduce a second FFI facade, an internal C ABI between Rust modules,
or Asterisk-shaped records in domain code. Keep the `asterisk` module private.

### `bridge` and tools

The bridge coordinates the typed SCCP and SIP APIs. PJSUA remains owned by one
operating-system thread; commands enter through a channel and owned Rust events
leave it. Do not let PJSUA pointers escape that boundary.

Command-line tools should reuse production parsers and codecs, separate
argument parsing from execution, return stable exit statuses, and write normal
results to stdout and diagnostics to stderr.

## Rust source style

### Toolchain and dependencies

- Build and test with the latest stable Rust toolchain. Use Rust edition 2024
  until the project deliberately adopts a newer edition; the edition does not
  limit which stable compiler version the project uses.
- Do not preserve an older minimum supported Rust version at the expense of
  using the current toolchain or idiomatic standard-library features.
- Never downgrade a dependency to avoid adapting the code to its current
  release. Resolve incompatibilities in the integration, update dependent
  code, or replace the dependency with a suitable maintained alternative.

### Formatting and imports

- Use default `rustfmt`; do not hand-format around it.
- Group imports as standard library, external crates, then local crate imports,
  with a blank line between groups.
- Import the names a module actually uses. Production code under
  `asterisk-module/src/asterisk` must not use wildcard imports or a prelude;
  `use super::*` is acceptable in a test module.
- Prefer ordinary module files over textual `include!`; generated bindgen
  output is the intentional exception.
- Put module documentation and declarations near the top, implementation next,
  and local tests at the bottom. Very large test suites may use a sibling
  `tests/` module selected with `#[path]`.
- Scope lint exceptions to the smallest binding-heavy item or module and state
  why the exception is inherent to that boundary.
- For private runtime APIs consumed only by native adapters, gate native-only
  entrypoints with the native features. When owner tests need the complete
  production command or snapshot types, scope `expect(dead_code)` to the
  affected item and development test configuration, with a reason. Keep native
  builds linted; do not widen visibility or add artificial calls to silence lints.

### Visibility and module APIs

Use the narrowest useful visibility: private first, then `pub(super)`, then
`pub(crate)`, and `pub` only for an intended crate API. Re-export the supported
surface from an owning module instead of exposing implementation layout.

Inside the private `asterisk` hierarchy, use ancestry and `pub(super)` to
contain APIs. Do not add `pub(crate)` there; the architecture tests deliberately
prevent that hierarchy from leaking into the rest of the crate.

Module roots should explain ownership and important invariants, then mostly
declare modules and re-export their supported types.

### Naming

Follow standard Rust casing and the vocabulary already used by the domain:

- Files, modules, functions, and fields use `snake_case`.
- Types and variants use `UpperCamelCase`; constants use
  `SCREAMING_SNAKE_CASE`.
- Write acronyms as Rust words in type names: `Sccp`, `Pbx`, `Ami`, `Rtp`,
  `Dtmf`, `Mwi`, and `Blf`.
- Include units and representation in names where confusion is possible:
  `timeout_seconds`, `packet_ms`, `maximum_bytes`, `rtp_port`, or
  `wire_value`.
- Use domain-specific suffixes consistently: `Error`, `Rejection`, `Outcome`,
  `Operation`, `Plan`, `Snapshot`, `Registry`, `Generation`, `Receipt`, and
  `Guard` are distinct concepts, not interchangeable decoration.
- Keep different identity spaces in different newtypes. A `CallId`,
  `CallReference`, `PbxCallId`, `ConferenceId`, generation, and wire token must
  not become bare interchangeable integers.

### Data modeling

- Prefer enums and small structs over boolean combinations or loosely related
  tuples.
- Use newtypes to validate text, numeric ranges, identities, and secrets at
  construction time. Provide `TryFrom`, `FromStr`, `AsRef`, `Display`, or
  lossless `From` conversions when they express the contract naturally.
- Derive only meaningful traits. Value types commonly derive `Clone`, `Copy`,
  `Debug`, `Default`, `Eq`, `Hash`, `Ord`, `PartialEq`, and `PartialOrd` as
  appropriate.
- Give `Default` only a safe, unsurprising semantic state. Mark the enum variant
  with `#[default]` when applicable.
- Preserve extensible protocol numbers with typed `Unknown(raw)` variants and
  lossless `wire_value` conversion. Reject unknown values only where the
  operation itself requires a known value.
- Use macros for genuinely repetitive, auditable definitions such as wire
  enums, identifier newtypes, or catalog entries. Keep policy and exceptional
  behavior outside mechanical macros.
- Prefer `match` when branching repeatedly on one value, especially an enum or
  protocol discriminator. Keep `if`/`else` for short boolean conditions; turn
  long `if`/`else if` chains into a `match` or a clearer typed abstraction.
- Make `match` expressions exhaustive when every state or protocol value
  requires an explicit decision.

### Validation, errors, and arithmetic

External data includes network frames, XML, configuration, native callback
arguments, database rows, environment variables, and CLI/AMI/HTTP input.
Validate it before allocation, publication, or mutation of live state.

- Put named limits near the owning domain and include the unit in the constant
  name, usually `MAX_*_BYTES`, `MAX_*_ITEMS`, or `*_TIMEOUT`.
- Distinguish byte limits from character limits and wire payload size from full
  frame size.
- Reject truncation, trailing data, invalid padding, impossible counts,
  forbidden controls, interior NULs, and lossy integer conversions explicitly.
- Use `TryFrom`, checked arithmetic, `NonZero*`, and deliberate saturating
  arithmetic instead of unchecked casts or overflow assumptions.
- Structured Serde input normally uses `deny_unknown_fields`.
- Prefer typed `thiserror` enums with concrete variants and sources. Preserve
  whether an operation was invalid, unavailable, conflicting, stale,
  exhausted, or failed in a dependency.
- Use `?` and `map_err` to retain context. Do not collapse a useful error into
  `false` or a generic string inside library code.
- Reserve `unwrap`, `expect`, and `panic!` for tests, build-time failures, or a
  locally proven invariant. An `expect` message should name that invariant.
  Production code in the Asterisk composition hierarchy is linted against
  `unwrap` and `expect`.
- Ignoring a result is acceptable only for an explicitly best-effort cleanup
  or notification path; make that intent obvious at the call site.

### Privacy and diagnostics

Do not place credentials, private keys, authentication bodies, raw XML, caller
data, or opaque packet contents in `Debug`, errors, logs, or management events.

- Implement custom `Debug` for secret-bearing or opaque types and show only
  safe metadata such as kind, length, or a redacted marker.
- Validate diagnostics and management fields through the same bounded policy
  as their transport.
- Prefer identifiers, counts, states, and hashes over raw values in logs.
- Sanitized protocol fixtures must use documentation addresses and synthetic
  station/caller data.

### State transitions, concurrency, and lifecycle

- Centralize mutable domain state in the component that owns the invariant.
  Other layers request a transition; they do not patch its maps directly.
- Have state transitions return owned outcomes, plans, or ordered effects.
  An owner loop must not await effects inline. In code that still uses shared
  state, release the state lock before awaiting, calling native code,
  publishing an event, or doing other adapter I/O. A queue around shared mutable
  registries is not an ownership migration.
- Treat effect order as part of the contract. Ordinary execution stops at the
  first failure; committed terminal cleanup attempts every remaining effect.
- Use explicit prepare/commit/abort or stage/validate/commit phases for
  multi-resource work. Use RAII for local rollback, unpublished resources, and
  lifetime permits. Asynchronous compensation belongs to the state owner;
  `Drop` may release native resources it exclusively owns, but must not start
  new native operations, spawn detached work, or depend on obtaining ordinary
  command capacity. A cancellation sent from `Drop` must already own
  guaranteed delivery capacity and be idempotent.
- Use generations, tokens, and exact identities to reject stale callbacks,
  acknowledgements, timeouts, and replacement-session work.
- Use `Arc` for explicit shared ownership and `Weak` when callbacks must not
  keep a runtime alive. Prefer one owner plus channels when a foreign library
  has thread affinity.
- Bound externally driven queues, callback admission, pending registries,
  concurrency, and wait times where exhaustion is possible.
- Never hold a synchronous lock across `.await`. Move blocking native work to
  `spawn_blocking` or a dedicated owner thread.
- Shutdown order matters: stop admission and producers, invalidate or cancel
  new work, drain in-flight callbacks/tasks, then release registrations and
  native owners. Make shutdown idempotent where callers may race.
- Recover poisoned locks only at a boundary with an explicit policy; do not
  scatter ad hoc poison recovery through domain code.

#### Runtime owner boundaries

Apply these constraints when introducing an owner or migrating a subsystem:

- One component owns each mutable invariant throughout the migration. Handles
  expose typed operations and immutable snapshots, not mutable registries or
  arbitrary closures over the controller. Executors receive owned plans and
  scoped native handles; they cannot mutate controller state.
- Reserve all overlapping resources before dispatch. Resource keys must remain
  conservative while a request waits; preparation revalidates identities and
  generations. Unrelated work and deadlines must continue to progress, and
  matching acknowledgements/cancellations must not wait behind the operation
  they complete.
- Keep protocol readers able to consume acknowledgements and terminal messages
  when ordinary event admission is saturated. A separate completion receiver is
  insufficient if sending an earlier ordinary event prevents reading the reply.
- Bound admitted work through completion and compensation, including owner-side
  pending queues and finished worker results. Moving a message out of a mailbox
  must not release its budget. Reserve teardown and completion delivery before
  ordinary traffic can exhaust capacity; coalesce duplicate terminal work.
- Separate admission, completed delivery, and response timeout in types and
  names. A response timeout does not prove that an operation never executed.
  Expire queued requests before starting effects; retain ownership of work that
  has already started when the requester stops waiting.
- Track the actual blocking job, not just an async wrapper awaiting it. Close
  admission before draining. A state-transition panic must not detach native
  jobs during unwinding, and a timed-out join is not evidence that native work
  has stopped. Unfinished native work must prevent successful unload.
  Failed startup has the same obligation: guard threads created before fallible
  registration so returning an error to the native loader closes and joins them.
- Preserve a policy change's invalidation and fallback decisions when updates
  are coalesced. Reject stale completions in both published snapshots and the
  cache used for future fallback; equality with an earlier configuration does
  not make an old completion current again.
- Native callback delivery must never wait for its subscription worker, which
  may be joining that callback during unregistration. Reserve delivery space
  before subscribing; retain initial and terminal updates when coalescing, and
  serve subscription keys fairly. Reclaim a retired subscription's pending
  delivery only after its callbacks have joined.
- Keep narrow synchronization for native lifetime admission, callback draining,
  resource replacement, and immutable snapshot publication. Borrowed C pointers
  stay on their callback thread. Split prepare/native-operation/commit rather
  than moving a borrowed pointer to a worker or blocking a thread needed for
  the reply. Native unload should reject an active operation promptly when
  waiting under loader locks or callback re-entry could deadlock.

### FFI and unsafe code

Unsafe code belongs at a narrow native edge, never in ordinary domain models.

- Keep generated bindings private and call them through resource-oriented
  adapters.
- Convert raw pointers, C strings, status integers, and sentinel values into
  owned or typed Rust values immediately. Bound every C-string scan.
- Use `NonNull` and RAII wrappers to represent retained references, locks,
  allocations, module references, and unpublished resources. Encode ownership
  transfer with consuming methods or `Option::take`, not comments alone.
- An `unsafe fn` must document its caller obligations under `# Safety`.
  Keep individual unsafe operations small and explicit; the workspace denies
  implicit unsafe operations in unsafe functions except in isolated raw
  binding modules.
- Explain non-obvious `unsafe impl Send`/`Sync` decisions next to the impl and
  enforce any thread affinity with the type system where possible.
- Wrap the complete body of every C callback in panic containment and return a
  stable ABI fallback. No Rust unwind may cross a foreign boundary.
- Callback registration must define admission, maximum concurrency,
  unregister ordering, draining, self-unregister behavior, and exactly-once
  release of native userdata.
- Only real host entry points should use `extern "C"`; Rust-to-Rust calls stay
  on the Rust ABI.

### Wire formats, XML, and configuration

- Treat wire layouts as protocol facts. Decode exact or explicitly supported
  version-selected sizes; validate padding and reserved fields instead of
  silently discarding malformed bytes.
- Keep one semantic public message model separate from declarative private
  wire details. Represent protocol variation with enums, optional fields,
  validated newtypes, or length-aware decoding before reaching for parallel
  version-specific structs. Use separate wire structs only for genuinely
  incompatible layouts, and convert them immediately into the shared semantic
  model.
- When a known layout is not modeled, preserve it only in a bounded opaque type
  and report that fidelity in the catalog.
- Use the typed XML boundary and Serde models. Do not construct XML with string
  concatenation or parse it with manual tag searches. Validate schema rules
  that types and Serde cannot express before serialization or publication.
- Configuration providers build complete normalized candidates. A refresh
  must not mutate live configuration; diff, validate, stage dependent
  resources, and commit one snapshot only after all fallible preparation
  succeeds.
- Preserve the distinctions among absent, database `NULL`, explicit empty,
  inherited, and deleted configuration values.
- Keep defaults, parsing, canonicalization, inheritance, provider access, and
  reload planning in their owning modules instead of duplicating policy in an
  adapter.

### Documentation and logging

- Documentation must add information the code cannot state clearly. Do not
  comment assignments, control flow, calls, or names whose purpose is already
  obvious from the code.
- Use `//!` to explain a module's responsibility, boundary, and non-obvious
  invariants. Use `///` for public API contracts, units, ordering, ownership,
  and failure behavior.
- Explain why a constraint exists; do not narrate syntax. Comments beside
  timing constants, protocol exceptions, and unsafe ownership should record
  the evidence or invariant that makes them correct.
- Keep examples compilable when practical and link related public types with
  intra-doc links.
- Outside the native Asterisk logger, use structured `tracing` fields. Put the
  stable human-readable event last, use `%value` for display and `?value` for
  safe debug output, and choose levels consistently: `debug` for expected
  protocol detail, `info` for lifecycle changes, `warn` for recoverable
  operational failures, and `error` for failed top-level tasks.

## Testing style

Tests are executable contracts, not incidental coverage.

- Match CI's feature selection and warning policy when verifying a change.
  For Asterisk development tests, run
  `RUSTFLAGS=-Dwarnings cargo test --locked -p asterisk-module --lib --tests`;
  native builds exercise a different set of consumers and cannot replace this
  check.
- Name tests as complete behavioral claims in `snake_case`, for example
  `replacement_calls_and_media_requests_never_reuse_identifiers`.
- Put focused unit tests beside the code. Use integration tests for public
  behavior, wire fixtures, cross-module architecture, and native ownership
  contracts.
- Cover the success path plus malformed input, exact boundaries, stale
  identity, exhaustion, rollback/compensation, ordering, teardown, privacy,
  and idempotence as relevant.
- Assert complete typed values and ordered effect/event sequences. Avoid tests
  that only assert that an operation returned `Ok`.
- Use table-driven cases when every enum state, protocol version, backend, or
  boundary value needs the same assertion.
- Use fake trait implementations to record requests and inject failures.
  Assert that adapter work does not retain state ownership, that unrelated
  work progresses, and that failure/shutdown joins the actual native jobs.
- Prefer paused Tokio time, injected clocks, channels, and bounded timeouts to
  timing-dependent sleeps.
- Test RAII and FFI ownership for exactly-once destruction, failed preparation,
  handoff, callback races, and shutdown while work is in flight.
- Source-inspection architecture tests are intentional. Update them when a
  legitimate boundary or exported callback changes; do not work around them.
  Assert ownership and ordering rather than requiring a particular mutex,
  task-spawn expression, or local variable name. Prefer behavioral regression
  tests for races that can be reproduced with controlled completions.
- `unwrap` and `expect` are normal in tests when the message identifies the
  fixture assumption being established.

### Golden protocol evidence

Golden byte fixtures are independent compatibility evidence, not snapshots
generated from the codec under test.

- Keep only the smallest sanitized complete frames needed to prove a layout.
- Record direction, message ID, protocol version, provenance, outcome, and
  SHA-256 in the manifest.
- Preserve original bytes when claiming exact round trips. Record
  normalization explicitly when exact reconstruction is impossible.
- Keep raw packet captures outside Git until reviewed for addresses, phone
  numbers, names, credentials, and XML application data.
- Do not infer a protocol cause from one observed response or non-response;
  distinguish byte validity from handset observation.
