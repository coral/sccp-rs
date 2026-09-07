mod support;
use support::{SourceContract, rust_region, source};

fn between(source: &str, start: &str, end: &str) -> SourceContract {
    rust_region(source, start, end)
}

#[test]
fn targeted_reload_guards_one_complete_candidate_before_the_shared_transaction() {
    let lifecycle = source("src/asterisk/runtime/lifecycle.rs");
    let selected = between(
        &lifecycle,
        "pub fn reload_selected(",
        "pub fn reconcile_mobility_after_reload(",
    );
    let refresh = selected
        .find(".config_provider\n        .refresh()")
        .expect("complete provider refresh");
    let plan = selected
        .find("ReloadPlan::build(&previous, &next)")
        .expect("complete diff plan");
    let guard = selected
        .find(".validate(&previous, &next, &plan)")
        .expect("target consistency guard");
    let stage = selected
        .find(".load_configuration(&next)")
        .expect("first staged runtime resource");
    let commit = selected
        .find("*access.shared.config.write_unpoisoned() = Arc::new(next)")
        .expect("single snapshot commit");
    assert!(refresh < plan && plan < guard && guard < stage && stage < commit);
    assert_eq!(selected.matches("config_provider").count(), 2);
    assert_eq!(selected.matches(".refresh()").count(), 1);
    assert_eq!(selected.matches(".activated(&access.config())").count(), 1);
    let handset = selected
        .find("reconfigure_station_policy(")
        .expect("handset preparation");
    let controller = selected.find(".reload_policy(").expect("controller commit");
    assert!(handset < controller && controller < commit);
    assert!(selected.contains("previous.device_definitions()"));
    assert!(selected.contains("staged_contexts.abort()"));
    assert!(!selected.contains("reconfigure_anonymous_hotline("));
    assert_eq!(selected.matches("Arc::new(next)").count(), 1);
}

#[test]
fn targeted_reload_cli_keeps_native_work_bounded_and_rust_owned() {
    let driver = source("src/asterisk/direct/cli.rs");
    let exports = source("src/asterisk/exports.rs");
    let adapter = between(&driver, "struct CliArgs<'a>", "fn cli_completion(");
    let callback = between(
        &driver,
        "unsafe fn run_reload_cli(",
        "unsafe extern \"C\" fn cli_reload(",
    );
    assert!(adapter.contains("raw: &'a sys::ast_cli_args"));
    assert!(adapter.contains("required_c_text("));
    assert!(adapter.contains("optional_c_text("));
    assert!(callback.contains("arguments.completion("));
    assert!(callback.contains("arguments.invocation("));
    assert!(callback.contains("MAX_RELOAD_ARGUMENT_BYTES"));
    assert!(callback.contains("MAX_RELOAD_ARGUMENTS"));
    assert!(!callback.contains("MAX_CLI_ARGUMENT_BYTES"));
    assert!(callback.contains("complete_reload_cli("));
    assert!(callback.contains("execute_reload_cli("));
    assert!(!callback.contains("config_provider"));
    assert!(!callback.contains("ReloadPlan"));

    let execute = between(
        &exports,
        "pub fn execute_reload_cli(",
        "pub fn complete_reload_cli(",
    );
    assert!(execute.contains("ReloadSelection::parse"));
    assert!(execute.contains("reload_selected(&access, selection)"));
    assert!(!execute.contains("config_provider"));
}

#[test]
fn cli_argument_pointer_work_is_confined_to_the_lifetime_bound_adapter() {
    let driver = source("src/asterisk/direct/cli.rs");
    let adapter = between(&driver, "struct CliArgs<'a>", "fn cli_completion(");
    let handlers = between(&driver, "unsafe fn run_reload_cli(", "fn cli_entry(");

    assert_eq!(driver.matches("argv.add(").count(), 1);
    assert!(adapter.contains("unsafe { *self.raw.argv.add(index) }"));
    assert!(adapter.contains("struct CliInvocation"));
    assert!(adapter.contains("struct CliCompletion"));
    assert!(!adapter.contains("MAX_RELOAD_ARGUMENTS"));
    assert!(!adapter.contains("ControlCliCommand"));
    assert!(!handlers.contains("arguments.argv"));
    assert!(!handlers.contains("arguments.argc"));
    assert!(!handlers.contains("required_c_text("));
    assert!(!handlers.contains("optional_c_text("));
    assert_eq!(handlers.matches("system::cli_completion").count(), 0);
    assert_eq!(driver.matches("system::cli_completion").count(), 1);
}
