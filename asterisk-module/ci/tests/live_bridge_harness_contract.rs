mod support;
use support::{source, workspace_source};

#[test]
fn lifecycle_measurement_waits_for_cleanup_and_rejects_failed_diagnostics() {
    let result = std::process::Command::new("sh")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/ci/test-support/test-lifecycle-measurement.sh"
        ))
        .output()
        .expect("run portable lifecycle measurement fixtures");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr),
    );
}

#[test]
fn live_bridge_gate_is_feature_scoped_and_separate_from_artifact_builds() {
    let manifest = source("Cargo.toml");
    let module = source("src/asterisk/mod.rs");
    let native = source("src/asterisk/native/mod.rs");
    let driver = source("src/asterisk/direct/cli.rs");
    let artifact = source("build-linux-x86_64.sh");
    let live = source("ci/live-tests/test-bridges.sh");
    let workflow = workspace_source(".github/workflows/asterisk-live-bridges.yml");

    assert!(manifest.contains("live-asterisk-tests = []"));
    assert!(module.contains("mod raw;"));
    assert!(native.contains("#[cfg(feature = \"live-asterisk-tests\")]"));
    assert!(driver.contains("crate::asterisk::raw::live_bridge_cli_entry()"));
    assert!(native.contains("live_bridge_tests::cli_entry()"));
    assert!(live.contains("SCCP_LIVE_BRIDGES=1"));
    assert!(!artifact.contains("live-asterisk-tests"));
    assert!(!artifact.contains("test-bridges.sh"));
    assert!(workflow.contains("lane: \"22\""));
    assert!(workflow.contains("lane: latest"));
    assert!(workflow.contains("asterisk-22"));
    assert!(workflow.contains("asterisk-latest"));
    assert!(workflow.contains("resolve-asterisk-ref.sh"));
    assert!(workflow.contains("ASTERISK_REF=${{ steps.asterisk.outputs.ref }}"));
    assert!(workflow.contains(".dockerignore"));
    assert!(!workflow.contains("23.4.1"));
    assert!(workflow.contains("asterisk-module/ci/Dockerfile"));
    assert!(workflow.contains("target: bridge-test-build"));
    assert!(workflow.contains("docker run --rm"));
    assert!(workflow.contains("SCCP_LIFECYCLE_ARTIFACT_DIR=/diagnostics"));
    assert!(workflow.contains("if: always()"));
    assert!(workflow.contains("actions/upload-artifact@"));
}

#[test]
fn docker_context_excludes_local_research_work() {
    let dockerignore = workspace_source(".dockerignore");

    assert!(
        dockerignore
            .lines()
            .any(|line| line == "research/.research")
    );
}

#[test]
fn live_bridge_gate_uses_real_owned_native_boundaries() {
    let harness = source("ci/live-tests/bridge.rs");
    let bridge = source("src/asterisk/native/bridge/conference.rs");

    for required in [
        "__ast_channel_alloc(",
        "ModuleReference::acquire(module_self())",
        "create_bridge(",
        "merge_consultation(",
        "merge_calls(",
        "merge_participant(",
        "admit_two_party_source(",
        "set_participant_muted(",
        "set_participant_music_on_hold(",
        "remove_participant_and_hangup(",
        "acquire_barge_bridge(",
        "prepare_conference_destination(",
        "module_use_count()",
        "bridge_count()",
        "reference_count()",
    ] {
        assert!(harness.contains(required), "live gate lost {required}");
    }
    assert!(
        harness.matches("admit_two_party_source(").count() >= 5,
        "every successful merge path must populate two-party source bridges"
    );
    assert!(bridge.contains("let Some(channel) = (unsafe { ChannelRef::acquire(channel) })"));
    assert!(bridge.contains("let _ = channel.into_raw()"));
}
