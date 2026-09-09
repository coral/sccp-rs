# Native bridge lifecycle gate

The live bridge workflow builds `bridge-test-build`, then runs the resulting
image in a separate container step. Compilation may come from Docker's cache;
the lifecycle test runs on every job. The `bridge-test` Docker target remains
available for running the gate during a local build.

Each cycle loads the driver, checks its mapped binary identity, runs all 11
bridge ownership scenarios, unloads the runtime, and verifies that its channel
technology is gone. The bridge scenarios check channel destruction, bridge
removal, and module references independently of process memory measurements.

At warmup and batch checkpoints the harness:

1. Waits for FD and thread counts to settle within a bounded cleanup window.
2. Records RSS before allocator reclamation.
3. Uses Asterisk's `malloc trim` command to return free heap pages to the OS.
4. Waits for the diagnostic CLI worker to retire and checks FD/thread counts
   against the warmup baseline before recording the checkpoint.

The RSS limit remains 1,024 KiB of growth between the second and third measured
batches. Trimming releases free pages, not live allocations. Raw per-cycle and
pre-trim RSS readings remain in `lifecycle.tsv`; the allocator responses are
recorded in `cli.log`. A missing or failed `malloc trim` command fails the gate.
This measurement requires the GNU allocator support in the CI Asterisk builds.

`SCCP_LIVE_WARMUP_CYCLES` and `SCCP_LIVE_BATCH_CYCLES` select longer probes;
their defaults are two warmup cycles and three batches of two measured cycles.
Do not raise `SCCP_LIFECYCLE_RSS_TOLERANCE_KB` to make a failing run pass without
investigating its allocation and resource-lifetime behavior.

CI uploads `lifecycle.tsv`, `cli.log`, and `asterisk.log` as
`bridge-lifecycle-22` or `bridge-lifecycle-latest` on success and failure.
For local container runs, mount a writable directory and set
`SCCP_LIFECYCLE_ARTIFACT_DIR` to its path inside the container. Diagnostic export
happens before the temporary Asterisk sandbox is removed.

RSS includes Asterisk, allocator state, and resident mappings. A passing budget
is not proof that every allocation was freed. Likewise, a driver listed as
`Not Running` can still have a resident DSO when glibc retains Rust TLS
destructors. The gate verifies runtime teardown; it does not promise that the
dynamic loader unmapped the shared object. Loader errors remain visible in
the archived Asterisk log.

Portable measurement failure-path tests run with:

```sh
sh asterisk-module/ci/test-support/test-lifecycle-measurement.sh
```

They are also included in the `live_bridge_harness_contract` Rust test target.
