use std::collections::BTreeSet;
use std::fs;

use syn::visit::Visit;

mod support;
use support::{
    crate_root, docker_stage, path_source, rust_attribute_count, rust_extern_c_functions,
    rust_item, rust_modules, rust_region, rust_repr_c_types, rust_sources, rust_token_count,
    source, workspace_source,
};

const RUST_NATIVE_MODULES: &[&str] = &[
    "bridge/mod.rs",
    "bridge/conference.rs",
    "bridge/parking.rs",
    "bridge/pickup.rs",
    "channel/mod.rs",
    "channel/allocation.rs",
    "channel/completion.rs",
    "channel/control.rs",
    "channel/media.rs",
    "channel/metadata.rs",
    "channel/ownership.rs",
    "channel/party_metadata.rs",
    "channel/video.rs",
    "dialplan.rs",
    "handles.rs",
    "http.rs",
    "manager.rs",
    "presence/mod.rs",
    "presence/hints.rs",
    "presence/mwi.rs",
    "realtime.rs",
    "recording.rs",
    "registry/mod.rs",
    "registry/callback.rs",
    "sorcery.rs",
    "sorcery/object.rs",
    "system.rs",
];

#[derive(Default)]
struct ProductionImportViolations {
    wildcard_imports: usize,
    prelude_modules: usize,
}

impl<'ast> Visit<'ast> for ProductionImportViolations {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if has_test_cfg(&item.attrs) {
            return;
        }
        if item.ident == "prelude" {
            self.prelude_modules += 1;
        }
        syn::visit::visit_item_mod(self, item);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        if has_test_cfg(&item.attrs) {
            return;
        }
        if use_tree_has_glob(&item.tree) {
            self.wildcard_imports += 1;
        }
        syn::visit::visit_item_use(self, item);
    }
}

fn has_test_cfg(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        let syn::Meta::List(configuration) = &attribute.meta else {
            return false;
        };
        attribute.path().is_ident("cfg") && configuration.tokens.to_string() == "test"
    })
}

fn use_tree_has_glob(tree: &syn::UseTree) -> bool {
    match tree {
        syn::UseTree::Glob(_) => true,
        syn::UseTree::Group(group) => group.items.iter().any(use_tree_has_glob),
        syn::UseTree::Path(path) => use_tree_has_glob(&path.tree),
        syn::UseTree::Name(_) | syn::UseTree::Rename(_) => false,
    }
}

fn production_import_violations(source: &str) -> ProductionImportViolations {
    let syntax = syn::parse_file(source).expect("import contract fixture must parse");
    let mut violations = ProductionImportViolations::default();
    violations.visit_file(&syntax);
    violations
}

#[test]
fn asterisk_production_imports_are_explicit_without_preludes() {
    let asterisk_root = crate_root().join("src/asterisk");
    let mut files = Vec::new();
    rust_sources(&asterisk_root, &mut files);
    files.sort();

    let mut violations = Vec::new();
    for path in files {
        let relative = path
            .strip_prefix(&asterisk_root)
            .expect("Asterisk source must remain below its module root")
            .to_string_lossy()
            .replace('\\', "/");
        if path.file_name().is_some_and(|name| name == "prelude.rs") {
            violations.push(format!("{relative}: prelude source file"));
        }

        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("unable to read {}: {error}", path.display()));
        let source_violations = production_import_violations(&source);
        if source_violations.wildcard_imports != 0 {
            violations.push(format!(
                "{relative}: {} production wildcard import(s)",
                source_violations.wildcard_imports
            ));
        }
        if source_violations.prelude_modules != 0 {
            violations.push(format!(
                "{relative}: {} production prelude module(s)",
                source_violations.prelude_modules
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "Asterisk production imports must stay explicit:\n{}",
        violations.join("\n")
    );
}

#[test]
fn production_import_guard_allows_only_test_scoped_wildcards() {
    let allowed = production_import_violations(
        r#"
        use crate::{first, second};

        #[cfg(test)]
        mod tests {
            use super::*;
        }
        "#,
    );
    assert_eq!(allowed.wildcard_imports, 0);
    assert_eq!(allowed.prelude_modules, 0);

    let production_wildcard = production_import_violations("use super::*;");
    assert_eq!(production_wildcard.wildcard_imports, 1);

    let production_prelude = production_import_violations("mod prelude;");
    assert_eq!(production_prelude.prelude_modules, 1);
}

#[test]
fn domain_layers_do_not_depend_on_asterisk_bindings() {
    for directory in [
        "src/ami",
        "src/call",
        "src/config",
        "src/http",
        "src/media",
        "src/pbx",
        "src/presence",
        "src/state",
    ] {
        let mut files = Vec::new();
        rust_sources(&crate_root().join(directory), &mut files);
        for path in files {
            let contents = path_source(&path);
            assert!(
                !contents.contains("crate::asterisk"),
                "domain module imports Asterisk integration details: {}",
                path.display()
            );
            assert!(
                !contents.contains("ffi::sys"),
                "domain module imports generated bindings: {}",
                path.display()
            );
        }
    }
}

#[test]
fn asterisk_visibility_is_scoped_to_its_owning_module() {
    let library = source("src/lib.rs");
    assert!(library.contains("mod asterisk;"));
    assert!(
        !library.contains("pub mod asterisk;"),
        "the production Asterisk composition root must not become public API"
    );

    let mut files = Vec::new();
    rust_sources(&crate_root().join("src/asterisk"), &mut files);
    for path in files {
        let contents = path_source(&path);
        assert!(
            !contents.contains("pub(crate)"),
            "broad crate visibility escaped the Asterisk hierarchy: {}",
            path.display()
        );
        assert!(
            !contents.contains("pub(in crate::asterisk"),
            "private Asterisk ancestry should cap ordinary module APIs: {}",
            path.display()
        );
    }
}

#[test]
fn project_owned_internal_c_records_are_absent() {
    let mut files = Vec::new();
    rust_sources(&crate_root().join("src/asterisk"), &mut files);
    for path in files {
        let contents = path_source(&path);
        let relative = path
            .strip_prefix(crate_root().join("src/asterisk"))
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let records = rust_repr_c_types(&contents);
        let allowed = match relative.as_str() {
            "native/http.rs" => vec!["File"],
            "native/sorcery/object.rs" => vec!["StoredObject"],
            _ => Vec::new(),
        };
        assert_eq!(
            records, allowed,
            "unexpected project-owned C-shaped record in {relative}"
        );
    }
}

#[test]
fn rust_does_not_call_rust_through_legacy_c_named_functions() {
    let mut files = Vec::new();
    rust_sources(&crate_root().join("src/asterisk"), &mut files);
    for path in files {
        let contents = path_source(&path);
        for legacy in ["rust_sccp_", "sccp_ast_"] {
            assert!(
                !contents.contains(legacy),
                "legacy internal C-ABI name {legacy} returned in {}",
                path.display()
            );
        }
    }
}

#[test]
fn every_rust_defined_c_callback_is_an_actual_asterisk_entrypoint() {
    let mut files = Vec::new();
    rust_sources(&crate_root().join("src/asterisk"), &mut files);
    let allowed_native = BTreeSet::from(
        [
            ("direct/channel_driver.rs", "requester_with_stream_topology"),
            ("direct/channel_driver.rs", "call"),
            ("direct/channel_driver.rs", "hangup"),
            ("direct/channel_driver.rs", "answer"),
            ("direct/channel_driver.rs", "read"),
            ("direct/channel_driver.rs", "write"),
            ("direct/channel_driver.rs", "get_rtp_info"),
            ("direct/channel_driver.rs", "get_vrtp_info"),
            ("direct/channel_driver.rs", "update_peer"),
            ("direct/channel_driver.rs", "get_codec"),
            ("direct/channel_driver.rs", "indicate"),
            ("direct/channel_driver.rs", "send_digit_begin"),
            ("direct/channel_driver.rs", "send_digit_end"),
            ("direct/channel_driver.rs", "send_text"),
            ("direct/channel_driver.rs", "set_option"),
            ("direct/channel_driver.rs", "query_option"),
            ("direct/channel_driver.rs", "fixup"),
            ("direct/channel_driver.rs", "device_state"),
            ("direct/channel_driver.rs", "call_completion"),
            ("direct/cli.rs", "cli_version"),
            ("direct/cli.rs", "cli_reload"),
            ("direct/cli.rs", "cli_forwarding"),
            ("direct/cli.rs", "cli_devices"),
            ("direct/cli.rs", "cli_lines"),
            ("direct/cli.rs", "cli_channels"),
            ("direct/cli.rs", "cli_media"),
            ("direct/cli.rs", "cli_media_statistics"),
            ("direct/cli.rs", "cli_sessions"),
            ("direct/cli.rs", "cli_reset"),
            ("direct/cli.rs", "cli_restart"),
            ("direct/cli.rs", "cli_dnd"),
            ("direct/cli.rs", "cli_dnd_schedule"),
            ("direct/cli.rs", "cli_message"),
            ("direct/cli.rs", "cli_answer"),
            ("direct/cli.rs", "cli_end"),
            ("direct/cli.rs", "cli_originate"),
            ("direct/module_info.rs", "load"),
            ("direct/module_info.rs", "unload"),
            ("direct/module_info.rs", "reload"),
            ("direct/module_info.rs", "register_module"),
            ("direct/module_info.rs", "unregister_module"),
            ("direct/module_info.rs", "__internal_chan_sccp2_self"),
            ("native/bridge/parking.rs", "async_application_thread"),
            ("native/bridge/parking.rs", "parking_event"),
            ("native/dialplan.rs", "function_read"),
            ("native/dialplan.rs", "function_write"),
            ("native/dialplan.rs", "application_execute"),
            ("native/http.rs", "callback"),
            ("native/manager.rs", "manager_action"),
            ("native/presence/hints.rs", "hint_update"),
            ("native/presence/hints.rs", "hint_watcher_destroy"),
            ("native/presence/mwi.rs", "mwi_event"),
            ("native/sorcery/object.rs", "object_alloc"),
            ("native/sorcery/object.rs", "object_destroy"),
            ("native/sorcery/object.rs", "object_copy"),
            ("native/sorcery/object.rs", "object_validate"),
            ("native/sorcery/object.rs", "field_apply"),
            ("native/sorcery/object.rs", "fields_export"),
            ("native/sorcery.rs", "device_created"),
            ("native/sorcery.rs", "device_updated"),
            ("native/sorcery.rs", "device_deleted"),
            ("native/sorcery.rs", "line_created"),
            ("native/sorcery.rs", "line_updated"),
            ("native/sorcery.rs", "line_deleted"),
        ]
        .map(|(file, name)| (file.to_owned(), name.to_owned())),
    );
    let mut actual = BTreeSet::new();
    for path in files {
        let relative = path
            .strip_prefix(crate_root().join("src/asterisk"))
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let contents = path_source(&path);
        for name in rust_extern_c_functions(&contents) {
            actual.insert((relative.clone(), name));
        }
    }
    assert_eq!(
        actual, allowed_native,
        "the Asterisk C callback inventory changed"
    );
}

#[test]
fn conference_destination_work_is_owned_by_the_rust_runtime() {
    let native = source("src/asterisk/native/bridge/conference.rs");
    assert!(!native.contains("ast_pthread_create"));
    assert!(!native.contains("extern \"C\" fn conference_application"));
    assert!(native.contains("pub struct ConferenceApplication"));
    assert!(native.contains("pub struct ConferenceApplicationCancellation"));

    let runtime = source("src/asterisk/runtime/backend.rs");
    let supplementary = source("src/asterisk/runtime/backend/supplementary.rs");
    assert!(supplementary.contains("conference_destination_tasks"));
    assert!(supplementary.contains("spawn_blocking"));
    assert!(runtime.contains("begin_shutdown"));
    let destination = rust_item(&supplementary, "fn start_conference_destination");
    assert!(destination.contains("conference_destination_failed("));
    assert!(destination.contains("complete_conference_mutation(mutation)"));
}

#[test]
fn native_adapter_is_split_into_rust_owned_domains() {
    let native = source("src/asterisk/native/mod.rs");
    for module in RUST_NATIVE_MODULES {
        let contents = source(&format!("src/asterisk/native/{module}"));
        assert!(
            rust_token_count(&contents) < 16_000,
            "src/asterisk/native/{module} has regrown into a monolith"
        );
    }
    let native_modules = rust_modules(&native).into_iter().collect::<BTreeSet<_>>();
    for module in [
        "bridge",
        "channel",
        "dialplan",
        "handles",
        "http",
        "manager",
        "presence",
        "realtime",
        "recording",
        "registry",
    ] {
        assert!(
            native_modules.contains(module),
            "native module root does not compile {module}"
        );
    }
    let bridge = source("src/asterisk/native/bridge/mod.rs");
    let bridge_modules = rust_modules(&bridge).into_iter().collect::<BTreeSet<_>>();
    for module in ["conference", "parking", "pickup"] {
        assert!(
            bridge_modules.contains(module),
            "bridge module root does not compile {module}"
        );
    }
    for module in ["channel_driver.rs", "handles.rs", "module_info.rs"] {
        assert!(
            crate_root()
                .join("src/asterisk/direct")
                .join(module)
                .is_file(),
            "missing direct Asterisk adapter module {module}"
        );
    }
}

#[test]
fn attended_transfer_runs_off_the_serial_handset_event_loop() {
    let calls = source("src/asterisk/phone/calls.rs");
    let transfer = source("src/asterisk/phone/transfer.rs");
    let completion = rust_item(&transfer, "pub(super) async fn execute_transfer_completion");
    assert!(completion.contains("tokio::task::spawn_blocking"));
    assert!(completion.contains("access.handle.spawn"));
    assert!(completion.contains("retain_two_channels"));
    assert!(
        completion.contains_literal("Transfer in progress")
            || transfer.contains_literal("Transfer in progress")
            || calls.contains_literal("Transfer in progress")
    );
}

#[test]
fn build_uses_one_upstream_binding_surface_and_compiles_no_repository_c() {
    let build = source("build.rs");
    let manifest = source("Cargo.toml");
    let sys = source("src/asterisk/sys.rs");

    assert!(build.contains_literal("sccp_asterisk_sys.h"));
    assert!(build.contains_literal("asterisk_sys.rs"));
    assert!(sys.contains_literal("/asterisk_sys.rs"));
    for retired in [
        "cc::Build",
        "NATIVE_SOURCES",
        "native/wrapper.h",
        "asterisk_shim.rs",
        "asterisk_raw.rs",
    ] {
        assert!(
            !build.contains(retired) && !build.contains_literal(retired),
            "build.rs restored retired native path {retired}"
        );
    }
    assert!(
        !manifest.contains("cc ="),
        "the retired C compiler dependency returned"
    );
    assert!(
        !crate_root().join("src/asterisk/ffi.rs").exists(),
        "the retired flat FFI re-export facade returned"
    );
    assert!(!sys.contains_literal("asterisk_shim.rs"));
    assert!(!sys.contains_literal("asterisk_raw.rs"));

    let legacy_native = crate_root().join("native");
    if legacy_native.exists() {
        for entry in fs::read_dir(legacy_native).unwrap() {
            let path = entry.unwrap().path();
            assert!(
                !matches!(
                    path.extension().and_then(|value| value.to_str()),
                    Some("c" | "h")
                ),
                "retired repository-owned native source remains: {}",
                path.display()
            );
        }
    }

    assert!(
        !crate_root().join("src/asterisk/abi.rs").exists(),
        "the retired project-owned Asterisk ABI catalog returned"
    );
    let persistence = source("src/asterisk/adapters/persistence.rs");
    assert!(persistence.contains("sys::ast_db_put"));
    assert!(persistence.contains("sys::ast_db_del"));
    let persistence_domain = source("src/state/persistence.rs");
    assert!(!persistence_domain.contains("crate::asterisk"));
    assert!(!persistence_domain.contains("ffi::sys::"));
}

#[test]
fn only_the_asterisk_module_self_hook_is_exported_explicitly() {
    let mut files = Vec::new();
    rust_sources(&crate_root().join("src"), &mut files);
    let mut exports = Vec::new();
    for path in files {
        let contents = path_source(&path);
        for _ in 0..rust_attribute_count(&contents, "no_mangle") {
            exports.push(path.clone());
        }
        assert!(
            rust_attribute_count(&contents, "export_name") == 0,
            "unexpected explicit export name in {}",
            path.display()
        );
    }
    assert_eq!(
        exports.len(),
        1,
        "only Asterisk's module-self hook may be exported"
    );
    assert!(exports[0].ends_with("asterisk/direct/module_info.rs"));
    let module_info = path_source(&exports[0]);
    assert!(module_info.contains("fn __internal_chan_sccp2_self("));
}

#[test]
fn rust_asterisk_root_remains_a_small_composition_root() {
    let root = source("src/asterisk/mod.rs");
    assert!(rust_token_count(&root) < 4_000);
    let modules = rust_modules(&root).into_iter().collect::<BTreeSet<_>>();
    for module in ["adapters", "boundary", "direct", "raw", "runtime", "sys"] {
        assert!(
            modules.contains(module),
            "composition root lost {module} module"
        );
    }
    assert!(
        !root.contains("include!("),
        "composition still uses textual include fragments instead of modules"
    );
}

#[test]
fn protocol_string_policy_stays_in_rust() {
    let http_policy = source("src/http/mod.rs");
    for required in [
        "request_body_length",
        "http_status_title",
        "validate_response_header",
    ] {
        assert!(
            http_policy.contains(required),
            "HTTP policy lost {required}"
        );
    }
    let manager_policy = source("src/ami/manager.rs");
    for required in [
        "struct ManagerField",
        "public_value",
        "validate_field_value",
        "request_field_name_sensitive",
        "struct RequestFields",
    ] {
        assert!(
            manager_policy.contains(required),
            "AMI string policy lost {required}"
        );
    }

    let manager_edge = source("src/asterisk/native/manager.rs");
    for required in [
        "ManagerRequestField::new",
        "serialized.push_str",
        "REDACTED_MANAGER_VALUE",
        "(*message).headers",
    ] {
        assert!(
            manager_edge.contains(required),
            "AMI native serialization lost {required}"
        );
    }
}

#[test]
fn http_unlink_cannot_free_a_descriptor_selected_by_asterisk() {
    let http = source("src/asterisk/native/http.rs");
    for required in [
        "struct HttpRouteGate",
        "closing: AtomicBool",
        "readers: AtomicUsize",
        "fn close_and_drain_readers",
        "if gate.is_null() || !(*gate).enter()",
        "sys::ast_http_uri_unlink",
        "release_from_native::<HttpPayload>",
    ] {
        assert!(
            http.contains(required),
            "HTTP callback/unlink ownership lost {required}"
        );
    }
    let unregister = rust_item(&http, "fn unregister_http");
    let close = unregister
        .find("close_and_drain_readers")
        .expect("close URI admission");
    let unlink = unregister.find("ast_http_uri_unlink").expect("unlink URI");
    let release = unregister
        .find("release_from_native")
        .expect("release callback payload");
    assert!(close < unlink && unlink < release);
    assert!(http.contains("Box::into_raw(gate)"));
    assert!(!unregister.contains("Box::from_raw"));
}

#[test]
fn proceeding_control_is_typed_at_the_actual_asterisk_callback() {
    let driver = source("src/asterisk/direct/channel_driver.rs");
    assert!(driver.contains("sys::AST_CONTROL_PROCEEDING"));
    assert!(driver.contains("ChannelIndication::Proceeding"));
    let exports = source("src/asterisk/exports.rs");
    assert!(exports.contains("ChannelIndication::Proceeding => RuntimeCallSignalKind::Proceeding"));
}

#[test]
fn native_call_indications_use_one_ordered_rust_queue() {
    let exports = source("src/asterisk/exports.rs");
    let answer_and_indicate = rust_region(
        &exports,
        "fn answer_channel",
        "fn send_digit_begin_to_channel",
    );
    assert!(answer_and_indicate.contains("enqueue_call_signal"));
    assert!(answer_and_indicate.contains("RuntimeCallSignalKind::Proceeding"));
    assert!(!answer_and_indicate.contains(".spawn("));
    let hangup = rust_item(&exports, "fn hangup_channel");
    assert!(hangup.contains("RuntimeCallSignalKind::Hangup"));
    assert!(hangup.contains("if !access.enqueue_call_signal"));
    assert!(hangup.contains("handle_runtime_hangup_signal"));

    let management = source("src/asterisk/runtime/management.rs");
    assert!(management.contains("Mutex<RuntimeCallSignalQueue>"));
    let lifecycle = source("src/asterisk/runtime/lifecycle.rs");
    assert!(lifecycle.contains("checked_add(1)"));
    assert!(lifecycle.contains("queue.sender.send(signal)"));

    let services = source("src/asterisk/runtime/services.rs");
    assert!(services.contains("signal.sequence <= last_sequence"));
    assert!(services.contains("HashMap::<PbxCallId, mpsc::UnboundedSender<RuntimeCallSignal>>"));
    assert!(services.contains("handle_runtime_call_signal(&lane_access, signal).await"));
    assert!(services.contains("controller.pbx_progress_with_media_mode"));

    let backend = source("src/asterisk/runtime/backend.rs");
    let handset = source("src/asterisk/runtime/backend/handset.rs");
    assert!(handset.contains("PhoneCommandAction::SetCallState"));
    assert!(handset.contains("PhoneCommandAction::CommitOutboundCall"));
    assert!(handset.contains("PhoneCommandAction::PresentOutboundRinging"));
    assert!(backend.contains("PhoneCommandAction::OpenOutboundMedia"));
    let handset_failure = rust_item(&backend, "fn handle_effect_error");
    assert!(handset_failure.contains("terminate_failed_pbx_call"));
    let outbound_media = rust_item(&backend, "async fn begin_outbound_media");
    let open = outbound_media
        .find("PhoneCommandAction::OpenOutboundMedia")
        .expect("coupled open command");
    let progress = outbound_media
        .find("PhoneCommandAction::DisplayPrompt")
        .expect("coupled progress prompt");
    assert!(open < progress);
    assert!(outbound_media.contains("\"Call Progress\".into()"));

    let handset_executor = rust_item(&handset, "async fn execute_handset_effect");
    assert!(handset_executor.contains_between(
        "HandsetEffect::SetCallState",
        "HandsetEffect::SetMicrophoneMode",
        "state != PhoneCallState::OnHook",
    ));
    assert!(handset_executor.contains_between(
        "HandsetEffect::SetCallState",
        "HandsetEffect::SetMicrophoneMode",
        "PhoneCommandAction::CloseCall",
    ));
}

#[test]
fn audio_receive_requests_use_the_selected_local_media_source() {
    let backend = source("src/asterisk/runtime/backend.rs");
    let begin_media = rust_item(&backend, "async fn begin_handset_media");
    assert!(begin_media.contains("let source = receive_media_source"));
    assert!(begin_media.contains("source: Some(source)"));

    let begin_answer = rust_item(&backend, "async fn begin_answer_media");
    assert!(begin_answer.contains("let source = receive_media_source"));
    assert!(begin_answer.contains("source: Some(source)"));

    let begin_outbound = rust_item(&backend, "async fn begin_outbound_media");
    assert!(begin_outbound.contains("let mut endpoint = receive_media_source"));
    assert!(begin_outbound.contains("source: Some(endpoint)"));

    let handset = source("src/asterisk/runtime/backend/handset.rs");
    let execute = rust_item(&handset, "async fn execute_handset_effect");
    assert!(execute.contains("let source = receive_media_source"));
    assert!(execute.contains("source: Some(source)"));
}

#[test]
fn unload_keeps_active_calls_subscriptions_and_conferences_in_one_ordered_drain() {
    let lifecycle = source("src/asterisk/runtime/lifecycle.rs");
    let stop = rust_item(&lifecycle, "fn stop");
    let ordered = [
        "manager_registrations",
        "http_registrations",
        "dialplan_registrations",
        "uninstall_blf(&self.access)",
        "self.event_task.abort()",
        "shutdown_conferences(&self.access).await",
        "shutdown_remote_hangups(&self.access).await",
        "shutdown_one_way_microphones(&self.access).await",
        "phone.shutdown().await",
        "registration_contexts",
        "self.parking_subscription.unsubscribe()",
        "self.runtime.shutdown_timeout",
    ];
    assert!(stop.contains_in_order(&ordered));

    let backend = source("src/asterisk/runtime/backend.rs");
    let conference_shutdown = rust_item(&backend, "async fn shutdown_conferences");
    for required in [
        "drain_conferences_for_shutdown",
        "cancel_conference_announcement_locked",
        "execute_cleanup_effects",
        "remaining_bridges",
        "remaining_barge_bridges",
        "remaining_calls",
        "remove_channel",
    ] {
        assert!(
            conference_shutdown.contains(required),
            "conference/call unload lost {required}"
        );
    }

    let presence = source("src/asterisk/runtime/presence.rs");
    let blf_shutdown = rust_item(&presence, "fn uninstall_blf");
    assert!(blf_shutdown.contains(".clear();"));

    let exports = source("src/asterisk/exports.rs");
    let unload = rust_item(&exports, "fn stop_module");
    assert!(unload.contains_in_order(&[
        ".take()",
        "shutdown_observers()",
        "uninstall_mwi(&module.access)",
        "module.stop()",
    ]));
}

#[test]
fn conference_announcements_are_generated_by_owned_pbx_channels() {
    let controller = source("src/runtime/controller/domains/conference.rs");
    let backend = source("src/asterisk/runtime/backend.rs");
    let native = source("src/asterisk/native/channel/control.rs");

    assert!(controller.contains("PbxEffect::ConferenceAnnouncement"));
    assert!(!controller.contains("HandsetEffect::ConferenceAnnouncement"));
    assert!(backend.contains("native_channel::start_tone_pair"));
    assert!(backend.contains("native_channel::stop_tone_pair"));
    assert!(!backend.contains("PhoneCommandAction::StartAnnouncement"));
    assert!(!backend.contains("PhoneCommandAction::AnnouncementFinish"));
    assert!(!backend.contains("PhoneCommandAction::StopAnnouncement"));
    assert!(native.contains("sys::ast_tonepair_start"));
    assert!(native.contains("sys::ast_tonepair_stop"));
}

#[test]
fn monitor_soft_key_uses_the_owned_recording_transaction() {
    let calls = source("src/asterisk/phone/calls/call_control.rs");
    let services = source("src/asterisk/runtime/services/recording.rs");

    assert!(calls.contains("soft_key: SoftKey::Monitor"));
    assert!(calls.contains("toggle_monitor_recording(access, recordings"));
    assert!(services.contains("plan_recording_toggle("));
    assert!(services.contains("recording_service_operation("));
    assert!(services.contains("PhoneCommandAction::SetRecordingStatus"));
}

#[test]
fn phone_events_dispatch_through_bounded_exhaustive_families() {
    let dispatcher = source("src/asterisk/phone/calls.rs");
    for family in [
        "session::handle_session_event",
        "call_control::handle_call_control_event",
        "media_events::handle_media_event",
        "telemetry::handle_telemetry_event",
    ] {
        assert!(dispatcher.contains(family), "dispatcher lost {family}");
    }
    assert!(dispatcher.contains("fn phone_event_family("));
    assert!(dispatcher.contains("PhoneDeviceEventKind::Registered(_)"));
    assert!(dispatcher.contains("PhoneDeviceEventKind::UnhandledMessage { .. }"));

    let session = source("src/asterisk/phone/calls/session.rs");
    let call_control = source("src/asterisk/phone/calls/call_control.rs");
    let media = source("src/asterisk/phone/calls/media_events.rs");
    let telemetry = source("src/asterisk/phone/calls/telemetry.rs");
    assert!(session.contains("PhoneDeviceEventKind::Registered(registration)"));
    assert!(call_control.contains("PhoneDeviceEventKind::OffHook"));
    assert!(media.contains("PhoneDeviceEventKind::ReceiveChannelOpened"));
    assert!(telemetry.contains("PhoneDeviceEventKind::Alarm"));
}

#[test]
fn device_state_publication_and_channel_callback_share_one_mapping() {
    let system = source("src/asterisk/native/system.rs");
    let driver = source("src/asterisk/direct/channel_driver.rs");
    assert!(system.contains("pub const fn device_state_raw("));
    assert!(system.contains("let mapped = device_state_raw(state)"));
    assert!(driver.contains("device_state_raw(line_device_state(&line))"));
    assert!(!driver.contains("AST_DEVICE_NOT_INUSE =>"));
}

#[test]
fn recording_callback_owner_precedes_channel_owner_in_actual_session_layout() {
    let recording = source("src/asterisk/native/recording.rs");
    let session = rust_item(&recording, "pub struct NativeRecordingSession");
    let callback = session.find("callback: CallbackOwner").unwrap();
    let channel = session.find("channel: ChannelRef").unwrap();
    assert!(callback < channel);
    let callback_owner = rust_item(&recording, "struct CallbackOwner");
    assert!(callback_owner.contains("callback: RecordingCallback"));
    assert!(!callback_owner.contains("Option<RecordingCallback>"));
}

#[test]
fn native_lifecycle_gate_stays_separate_from_artifact_builds() {
    let script = source("ci/test-native-lifecycle.sh");
    for required in [
        "module load chan_sccp2.so",
        "module unload chan_sccp2.so",
        "core show channeltypes",
        "record_metrics \"$cycle_label-start\"",
        "/proc/$asterisk_pid/fd",
        "/proc/$asterisk_pid/task",
        "/proc/$asterisk_pid/status",
        "assert_loaded_module_identity",
        "verify-loaded-module.sh",
        "second_batch_rss + RSS_TOLERANCE_KB",
        "kill -0",
    ] {
        assert!(
            script.contains(required),
            "native lifecycle gate lost {required}"
        );
    }
    assert!(script.contains("WARMUP_CYCLES:-4"));
    assert!(script.contains("BATCH_CYCLES:-12"));
    assert!(script.contains("autoload = no"));
    assert!(!script.contains("autoload = yes"));

    let docker = source("ci/Dockerfile");
    let artifact_source = docker_stage(&docker, "asterisk-source");
    let artifact_build = docker_stage(&docker, "artifact-build");
    assert!(artifact_source.contains("make include/asterisk/buildopts.h"));
    assert!(!artifact_source.contains("make -j"));
    assert!(!artifact_source.contains("make install"));
    assert!(!artifact_source.contains("make basic-pbx"));
    assert!(!artifact_build.contains("test-native-lifecycle.sh"));
    assert!(artifact_build.contains("rust_sccp_|sccp_ast_"));

    let workflow = workspace_source(".github/workflows/asterisk-module.yml");
    for version in ["22.7.0", "23.4.1"] {
        assert!(workflow.contains(version));
    }
    assert!(workflow.contains("make include/asterisk/buildopts.h"));
    assert!(!workflow.contains("make -j"));
    assert!(!workflow.contains("make install"));
    assert!(!workflow.contains("make basic-pbx"));
    assert!(!workflow.contains("test-native-lifecycle.sh"));
    assert!(workflow.contains("rust_sccp_|sccp_ast_"));
}

#[test]
fn binary_upgrade_checks_loaded_inode_and_requires_an_asterisk_restart() {
    let verifier = source("verify-loaded-module.sh");
    for required in [
        "/proc",
        "maps",
        "stat -Lc %i",
        "$6 == module",
        "(deleted)",
        "Restart the Asterisk process",
    ] {
        assert!(
            verifier.contains(required),
            "loaded-module identity verifier lost {required}"
        );
    }

    let install = workspace_source("docs/INSTALL.md");
    for required in [
        "This unload/load sequence is not a binary hot upgrade",
        "stop the Asterisk process",
        "verify-loaded-module.sh",
        "A `Running` row from `module show` proves lifecycle state, not binary identity",
        "If it reports `STALE`",
    ] {
        assert!(
            install.contains(required),
            "binary-upgrade guidance lost {required}"
        );
    }
}

#[test]
fn release_artifacts_are_versioned_and_debug_builds_are_explicit() {
    let script = source("build-linux-x86_64.sh");
    assert!(script.contains("module_version=$(sed"));
    assert!(script.contains("MODULE_VERSION=v$module_version"));
    assert!(script.contains("chan_sccp2-asterisk-linux-x86_64-v${module_version}.so"));

    let docker = source("ci/Dockerfile");
    assert!(docker.contains("ARG ARTIFACT_VARIANT=normal"));
    assert!(docker.contains("ARG MODULE_VERSION"));
    assert!(docker.contains("amd64) artifact_arch=x86_64"));
    assert!(docker.contains("arm64) artifact_arch=aarch64"));
    assert!(docker.contains("artifact_prefix=chan_sccp2-asterisk;"));
    assert!(docker.contains("artifact_prefix=chan_sccp2-asterisk-debug;"));
    assert!(docker.contains("sccp.dbg.coral.works"));
    assert!(docker.contains("${artifact_prefix}-linux-${artifact_arch}-${MODULE_VERSION}.so"));

    let release = workspace_source(".github/workflows/asterisk-module.yml");
    let compatibility = workspace_source(".github/workflows/asterisk-distro-compatibility.yml");
    for architecture in ["x86_64", "aarch64"] {
        let normal = format!("chan_sccp2-asterisk-linux-{architecture}-");
        let debug = format!("chan_sccp2-asterisk-debug-linux-{architecture}-");
        assert!(release.contains(&normal));
        assert!(release.contains(&debug));
    }
    assert!(release.contains("--features ${{ matrix.feature }},telemetry"));
    assert!(release.contains("cargo test --locked -p sccp-protocol --lib"));
    let protocol_manifest = workspace_source("sccp-protocol/Cargo.toml");
    assert!(!protocol_manifest.contains("telemetry ="));
    assert!(release.contains("steps.module.outputs.version"));
    assert!(compatibility.contains("steps.module.outputs.artifact"));
    assert!(compatibility.contains("chan_sccp2-asterisk-linux-${{ matrix.architecture }}-v"));

    assert!(release.contains("runs-on: ubuntu-24.04-arm"));
    assert!(release.contains("--platform linux/arm64"));
    assert!(release.contains("publish: false"));
    assert!(release.contains("if: matrix.publish"));
    assert!(compatibility.contains("runner: ubuntu-24.04-arm"));

    assert!(release.contains("tags:\n      - \"v*.*.*\""));
    assert!(release.contains("startsWith(github.ref, 'refs/tags/v')"));
    assert!(release.contains("release_tag=\"$GITHUB_REF_NAME\""));
    assert!(release.contains("expected_tag=\"v${manifest_version}\""));
    assert!(release.contains("--verify-tag"));
    assert!(release.contains("gh release upload"));
    assert!(release.contains("gh release delete-asset"));
    assert!(release.contains("--clobber"));
    assert!(release.contains("retention-days: 30"));
    assert!(!release.contains("release_tag=\"build-${GITHUB_SHA}\""));

    let manifest = source("Cargo.toml");
    assert!(manifest.contains(format!("version = \"{}\"", env!("CARGO_PKG_VERSION"))));
    assert!(manifest.contains("[package.metadata.release]"));
    assert!(manifest.contains("allow-branch = [\"master\"]"));
    assert!(manifest.contains("tag-name = \"v{{version}}\""));
    assert!(manifest.contains("publish = false"));

    let build = source("build.rs");
    assert!(build.contains("\"x86_64\" | \"aarch64\""));
}
