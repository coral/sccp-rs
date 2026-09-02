mod support;
use support::{SourceContract, rust_item, rust_match_arm, source};

fn function_body(source: &str, signature: &str, next_signature: &str) -> SourceContract {
    let _ = next_signature;
    rust_item(source, signature)
}

#[test]
fn provisional_recording_start_is_stopped_until_session_ownership_commits() {
    let recording = source("src/asterisk/native/recording.rs");
    let guard = function_body(
        &recording,
        "impl Drop for ProvisionalMixMonitor",
        "fn direction_parameters(",
    );
    assert!(guard.contains("impl Drop for ProvisionalMixMonitor"));
    assert!(guard.contains("ast_stop_mixmonitor(channel.as_ptr(), id.as_ptr())"));
    assert!(!guard.contains("ptr::null()"));
    assert!(!guard.contains("ManuallyDrop"));
    assert!(!guard.contains("ptr::read"));

    let start = function_body(
        &recording,
        "pub fn start_recording(",
        "unsafe fn publish_mute_event(",
    );
    let native_start = start.find("ast_start_mixmonitor").unwrap();
    let id_extraction = start.find("pbx_builtin_getvar_helper").unwrap();
    let provisional = start.find("ProvisionalMixMonitor::new").unwrap();
    let commit = start.find("provisional.commit()").unwrap();
    let session = start.find("NativeRecordingSession {").unwrap();
    assert!(native_start < id_extraction);
    assert!(!start.contains("ChannelLock::acquire"));
    assert!(id_extraction < provisional && provisional < commit && commit < session);
}

#[test]
fn deferred_anchor_completion_is_registry_owned_and_shutdown_drained() {
    let backend = source("src/asterisk/runtime/backend.rs");
    let defer = function_body(
        &backend,
        "fn defer_conference_announcement_completion(",
        "fn finish_conference_announcement(",
    );
    assert!(defer.contains("announcement_generation_is_current"));
    assert!(defer.contains("active.completion.take()"));
    assert!(defer.contains("completion.abort()"));
    assert!(defer.contains("active.completion = Some("));

    let complete = function_body(
        &backend,
        "pub fn complete_conference_announcement(",
        "fn complete_conference_announcement_locked(",
    );
    assert!(complete.contains("MediaAnchorMutation::try_acquire"));
    assert!(complete.contains("defer_conference_announcement_completion("));
    assert!(!complete.contains(".spawn("));

    let cancel = function_body(
        &backend,
        "pub fn cancel_conference_announcement(",
        "fn cancel_conference_announcement_locked(",
    );
    assert!(cancel.contains("defer_conference_announcement_completion("));
    assert!(!cancel.contains(".spawn("));

    let shutdown = function_body(
        &backend,
        "pub async fn shutdown_conferences(",
        "pub async fn shutdown_remote_hangups(",
    );
    let mutation = shutdown
        .find("MediaAnchorMutation::acquire(access).await")
        .unwrap();
    let cancel = shutdown
        .find("cancel_conference_announcement_locked")
        .unwrap();
    let drain = shutdown
        .find("drain_conference_announcement_restores")
        .unwrap();
    assert!(mutation < cancel && cancel < drain);

    let channel = source("src/asterisk/runtime/channel.rs");
    let remove = function_body(
        &channel,
        "pub fn remove_channel(",
        "pub fn with_channel<T>(",
    );
    assert!(remove.contains("media_anchors"));
    assert!(remove.contains("media_anchor_restores"));
    assert!(remove.contains("remove_call(pbx_id)"));

    let lifecycle = source("src/asterisk/runtime/lifecycle.rs");
    let stop = function_body(&lifecycle, "pub fn stop(mut self)", "impl Access");
    let abort_events = stop.find("self.event_task.abort()").unwrap();
    let join_events = stop.find("&mut self.event_task").unwrap();
    let drain_conferences = stop
        .find("shutdown_conferences(&self.access).await")
        .unwrap();
    assert!(abort_events < join_events && join_events < drain_conferences);

    let services = source("src/asterisk/runtime/services.rs");
    let run_events = function_body(
        &services,
        "pub async fn run_events(",
        "pub async fn run_call_signals(",
    );
    assert!(run_events.contains("let mut recording_sessions = RuntimeRecordings::default()"));

    let recording_backend = source("src/asterisk/runtime/backend/recording.rs");
    let session = function_body(
        &recording_backend,
        "pub(in super::super) struct AnchoredRecordingSession",
        "pub(in super::super) struct PendingRecordingAnchor",
    );
    assert!(session.find("inner: RecordingSession").unwrap() < session.find("anchor:").unwrap());
}

#[test]
fn rust_channel_private_takes_all_rtp_owners_on_every_teardown_path() {
    let driver = source("src/asterisk/direct/channel_driver.rs");
    let channel = source("src/asterisk/native/channel/allocation.rs");
    let handles = source("src/asterisk/native/handles.rs");
    assert_eq!(
        format!("{driver}\n{channel}")
            .matches("ast_rtp_instance_destroy(")
            .count(),
        1
    );

    let destroy = function_body(
        &channel,
        "fn destroy_channel_private(",
        "fn prepare_channel_private_teardown(",
    );
    assert!(destroy.contains("Box::from_raw(private.as_ptr())"));
    assert!(!destroy.contains("ast_rtp_instance_destroy"));
    assert!(channel.contains("identity: Option<ChannelIdentity>"));
    assert!(channel.contains("_module: ModuleReference"));
    assert!(!channel.contains("rust_state"));
    assert!(!channel.contains("c_void"));
    assert!(handles.contains("sys::__ast_module_running_ref("));
    assert!(handles.contains("sys::__ast_module_unref("));
    assert!(handles.contains("impl Drop for ModuleReference"));

    let rtp_drop = function_body(
        &channel,
        "impl Drop for OwnedRtpInstance",
        "pub unsafe fn channel_private(",
    );
    let stop = rtp_drop.find("ast_rtp_instance_stop").unwrap();
    let destroy_rtp = rtp_drop.find("ast_rtp_instance_destroy").unwrap();
    assert!(stop < destroy_rtp);

    let hangup = function_body(
        &driver,
        "unsafe extern \"C\" fn hangup(",
        "unsafe extern \"C\" fn answer(",
    );
    let rust_cleanup = hangup.find("hangup_channel(channel)").unwrap();
    let detach = hangup.find("ast_channel_tech_pvt_set").unwrap();
    let native_cleanup = hangup.find("destroy_channel_private(private)").unwrap();
    assert!(rust_cleanup < detach && detach < native_cleanup);

    let allocation_rollback =
        function_body(&channel, "fn allocate_channel(\n", "Ok(AllocatedChannel {");
    assert!(allocation_rollback.contains("UnpublishedChannel::new(channel)"));
    assert!(allocation_rollback.contains("Box::into_raw(private)"));
    assert!(!allocation_rollback.contains("destroy_channel_private(private)"));
    assert!(allocation_rollback.contains("format_cap_append(&capabilities, selected)"));
    assert!(!allocation_rollback.contains("let _ = format_cap_append"));
    let module = allocation_rollback
        .find("ModuleReference::acquire(")
        .unwrap();
    let rtp = allocation_rollback
        .find("OwnedRtpInstance::create(")
        .unwrap();
    assert!(module < rtp);
}

#[test]
fn masquerade_rebinds_the_owner_already_moved_by_the_core() {
    let driver = source("src/asterisk/direct/channel_driver.rs");
    let fixup = function_body(
        &driver,
        "unsafe extern \"C\" fn fixup(",
        "unsafe extern \"C\" fn device_state(",
    );
    assert!(!fixup.contains("ast_channel_tech_pvt_set"));
    let private = fixup
        .find("private(new_channel.as_ptr())")
        .expect("private state is read from the new channel");
    let validation = fixup
        .find("private_owner(private) != Some(old_channel)")
        .expect("the previous owner is validated");
    let owner = fixup
        .find("reassign_private_owner(private, new_channel)")
        .unwrap();
    let rtp_identity = fixup
        .find("ast_rtp_instance_set_channel_id")
        .expect("RTP identity follows the new channel");
    let rust_fixup = fixup
        .find("fixup_channel(old_channel, new_channel)")
        .unwrap();
    assert!(private < validation && validation < rust_fixup && rust_fixup < owner);
    assert!(owner < rtp_identity);
}

#[test]
fn rust_audio_rtp_glue_has_complete_owned_callbacks() {
    let driver = source("src/asterisk/direct/channel_driver.rs");
    let get_info = function_body(
        &driver,
        "unsafe extern \"C\" fn get_rtp_info(",
        "unsafe extern \"C\" fn update_peer(",
    );
    assert!(get_info.contains("let Some(private) = private(channel)"));
    assert!(get_info.contains("let rtp = retain_private_rtp(private)"));
    assert!(get_info.contains("if instance.is_null()"));
    assert!(!get_info.contains("sys::__ao2_ref("));
    assert!(get_info.contains("*instance = rtp"));
    assert!(get_info.contains("sys::AST_RTP_GLUE_RESULT_FORBID"));
    assert!(get_info.contains("sys::AST_RTP_GLUE_RESULT_REMOTE"));
    assert!(get_info.contains("sys::AST_RTP_GLUE_RESULT_LOCAL"));

    let update = function_body(
        &driver,
        "unsafe extern \"C\" fn update_peer(",
        "unsafe extern \"C\" fn get_codec(",
    );
    assert!(update.contains("NonNull::new(channel)"));
    assert!(update.contains("NonNull::new(instance)"));
    assert!(update.contains("update_peer_from_asterisk("));
    let update_helper = function_body(
        &driver,
        "unsafe fn update_peer_from_asterisk(",
        "unsafe extern \"C\" fn update_peer(",
    );
    assert!(update_helper.contains("ast_rtp_instance_get_requested_target_address"));
    assert!(update_helper.contains("optional_c_text(address, 64)"));
    assert!(update_helper.contains("update_rtp_peer("));

    let glue = function_body(&driver, "fn rtp_glue()", "pub(super) fn load()");
    for callback in [
        "glue.get_rtp_info = Some(get_rtp_info)",
        "glue.update_peer = Some(update_peer)",
        "glue.get_codec = Some(get_codec)",
    ] {
        assert!(glue.contains(callback));
    }
}

#[test]
fn video_rtp_has_independent_transactional_ownership_and_glue() {
    let allocation = source("src/asterisk/native/channel/allocation.rs");
    let video = source("src/asterisk/native/channel/video.rs");
    let driver = source("src/asterisk/direct/channel_driver.rs");
    let runtime = source("src/asterisk/runtime/channel.rs");
    let calls = source("src/asterisk/phone/calls/media_events.rs");
    let formats = source("src/media/formats/video.rs");

    assert!(allocation.contains("audio: OwnedAudioRtp"));
    assert!(allocation.contains("video: Option<OwnedVideoRtp>"));
    assert!(allocation.contains("retain_private_video_rtp"));
    assert!(allocation.contains("pub video: Option<VideoRtpConfiguration<'a>>"));
    assert!(allocation.contains("pub video: VideoRtpAllocation"));
    assert!(allocation.contains("pub enum VideoRtpAllocation"));
    assert!(allocation.contains("Active(MediaSocketQosReport)"));
    assert!(allocation.contains("Unavailable(VideoRtpError)"));

    let teardown = function_body(
        &allocation,
        "fn prepare_channel_private_teardown(",
        "pub unsafe fn private_rtp(",
    );
    for fd in 0..=3 {
        assert!(teardown.contains(format!("ast_channel_set_fd(channel.as_ptr(), {fd}, -1)")));
    }

    let prepare = function_body(
        &video,
        "pub(super) unsafe fn prepare_video(",
        "pub unsafe fn disable_video(",
    );
    let candidate = prepare.find("OwnedRtpInstance::create").unwrap();
    let configure = prepare.find("configure_format(").unwrap();
    let install = prepare.find("OwnedVideoRtp").unwrap();
    assert!(candidate < configure && configure < install);
    assert!(prepare.contains("apply_media_socket_qos(&instance, configuration.policy)"));
    assert!(video.contains("pub payload_type: RtpPayloadNumber"));
    assert!(formats.contains("RtpPayloadNumber::new"));
    assert!(video.contains("ast_rtp_codecs_payload_replace_format"));
    assert!(video.contains("c_int::from(payload_type.get())"));

    let lock_scope = function_body(
        &video,
        "unsafe fn with_locked_video_rtp<T>(",
        "pub unsafe fn set_remote_video(",
    );
    assert!(lock_scope.contains("ChannelLock::acquire(channel)"));
    assert!(lock_scope.contains("private_video_rtp(private)"));
    assert!(!lock_scope.contains("private_rtp(private).as_ptr()"));

    let remote = function_body(
        &video,
        "pub unsafe fn set_remote_video(",
        "pub unsafe fn local_video_endpoint(",
    );
    assert!(remote.contains("endpoint.port() == 0"));
    assert!(remote.contains("endpoint.ip().is_unspecified()"));
    assert!(remote.contains("endpoint.ip().is_multicast()"));
    assert!(remote.contains("with_locked_video_rtp(channel"));
    assert!(!remote.contains("private_rtp(private).as_ptr()"));

    let local = function_body(
        &video,
        "pub unsafe fn local_video_endpoint(",
        "pub unsafe fn disable_video(",
    );
    assert!(local.contains("with_locked_video_rtp(channel"));
    assert!(!local.contains("private_rtp(private).as_ptr()"));

    let allocate = function_body(
        &allocation,
        "pub unsafe fn allocate_channel(",
        "Ok(AllocatedChannel {",
    );
    let prepare = allocate.find("prepare_video(configuration)").unwrap();
    let private = allocate.find("Box::new(ChannelPrivate").unwrap();
    assert!(prepare < private);
    assert!(
        allocate.contains("Some(Err(error)) => (None, VideoRtpAllocation::Unavailable(error))")
    );
    let take_video = allocate
        .find("(*private.as_ptr()).video.take()")
        .expect("video owner must be taken before capability installation");
    let restore_video = allocate
        .find("(*private.as_ptr()).video = Some(video)")
        .expect("video owner must be restored only after installation");
    assert!(take_video < restore_video);
    assert!(allocate.contains("VideoRtpError::CapabilitiesUnavailable"));
    assert!(allocate.contains(
        "ast_channel_set_fd(\n                    channel.as_ptr(),\n                    2,"
    ));
    assert!(allocate.contains(
        "ast_channel_set_fd(\n                    channel.as_ptr(),\n                    3,"
    ));

    let read = function_body(
        &driver,
        "unsafe extern \"C\" fn read(",
        "unsafe extern \"C\" fn write(",
    );
    assert!(read.contains("2 => private_video_rtp(private)"));
    assert!(read.contains("3 => private_video_rtp(private)"));
    let write = function_body(
        &driver,
        "unsafe extern \"C\" fn write(",
        "unsafe extern \"C\" fn get_rtp_info(",
    );
    assert!(write.contains("sys::AST_FRAME_VIDEO"));
    assert!(write.contains("private_video_rtp(private)"));
    let technology = function_body(&driver, "fn channel_technology()", "fn rtp_glue()");
    assert!(technology.contains("technology.write_video = Some(write)"));
    assert!(driver.contains("sys::AST_CONTROL_VIDUPDATE"));
    assert!(calls.contains("ChannelControl::VideoUpdate"));
    assert!(calls.contains("MultimediaTransmitStarted"));
    let backend = source("src/asterisk/runtime/backend/handset.rs");
    assert!(backend.contains("MultimediaTransmitControl::FastPictureUpdate"));

    let disable = function_body(
        &video,
        "pub unsafe fn disable_video(",
        "pub unsafe fn local_video_endpoint(",
    );
    assert!(disable.contains("take_private_video(private)"));
    assert!(allocation.contains("pub(super) unsafe fn take_private_video("));
    assert!(allocation.contains("(*private.as_ptr()).video.take()"));
    assert!(disable.contains("ast_channel_set_fd(channel.as_ptr(), 2, -1)"));
    assert!(disable.contains("ast_channel_set_fd(channel.as_ptr(), 3, -1)"));
    assert!(disable.contains("ast_channel_nativeformats_set"));
    assert!(disable.find("drop(lock)").unwrap() < disable.find("drop(video)").unwrap());
    assert!(runtime.contains("video_allocated") && runtime.contains("!keep_video"));
    assert!(runtime.contains("native_channel::video::disable_video"));

    let glue = function_body(&driver, "fn rtp_glue()", "unsafe fn technology_formats()");
    assert!(glue.contains("glue.get_vrtp_info = Some(get_vrtp_info)"));

    let video_info = function_body(
        &driver,
        "unsafe extern \"C\" fn get_vrtp_info(",
        "unsafe fn update_peer_from_asterisk(",
    );
    assert!(video_info.contains("retain_private_video_rtp(private)"));
    assert!(video_info.contains("sys::AST_RTP_GLUE_RESULT_LOCAL"));
    assert!(!video_info.contains("direct_media_allowed"));

    let update = function_body(
        &driver,
        "unsafe extern \"C\" fn update_peer(",
        "unsafe extern \"C\" fn get_codec(",
    );
    let audio = update.find("NonNull::new(instance)").unwrap();
    let video_only = update.find("else if !video.is_null()").unwrap();
    let anchor = update.find("MediaPeerUpdate::Anchor").unwrap();
    assert!(audio < video_only && video_only < anchor);
    assert!(update.contains_between(
        "else if !video.is_null()",
        "MediaPeerUpdate::Anchor",
        "Ok(())"
    ));

    assert!(runtime.contains("video: selected_video.ready().map"));
    assert!(runtime.contains("VideoFallbackReason::DescriptorUnavailable"));
    assert!(runtime.contains("VideoFallbackReason::NativeRtpUnavailable"));
    assert!(runtime.contains("VideoFallbackReason::LocalEndpointUnavailable"));
    assert!(runtime.contains_literal("unable to apply configured video socket QoS"));
    assert!(runtime.contains_literal("unable to allocate optional video RTP"));
    assert!(runtime.contains("local_video_endpoint(access, pbx_id, &binding.device_id)"));

    let receive = rust_match_arm(
        &calls,
        "PhoneDeviceEventKind::MultimediaReceiveChannelOpened",
    );
    assert!(receive.contains("normalize_phone_video_endpoint"));
    assert!(receive.contains("set_remote_video_endpoint"));
    assert!(receive.contains("AmiMediaKind::Video"));
    assert!(!receive.contains("media_opened_for_device"));

    let unload = function_body(&driver, "pub(super) fn unload()", "pub(super) fn reload()");
    assert!(unload.contains("has_active_channels()"));
    assert!(unload.contains("native_registration().take()"));
}

#[test]
fn native_hangup_retires_the_binding_until_serialized_cleanup() {
    let exports = source("src/asterisk/exports.rs");
    let hangup = rust_item(&exports, "pub unsafe fn hangup_channel");
    assert!(hangup.contains(".get(&state.pbx_id)"));
    assert!(hangup.contains("binding.close()"));
    assert!(!hangup.contains(".remove(&state.pbx_id)"));

    let backend = source("src/asterisk/runtime/backend.rs");
    let execute = rust_item(&backend, "pub async fn execute_one_effect");
    assert!(execute.contains("ChannelAvailability::Retiring"));
    assert!(execute.contains("discard_stale_media_effect"));

    let services = source("src/asterisk/runtime/services.rs");
    let cleanup = rust_item(&services, "pub async fn handle_runtime_hangup_signal");
    assert!(cleanup.contains("remove_channel(access, pbx_id)"));
}

#[test]
fn shared_media_owner_selects_format_before_remote_endpoint() {
    let media = source("src/asterisk/runtime/backend/media_effects.rs");
    let configure = rust_item(&media, "fn configure_media");
    let format = configure.find("native_channel::set_audio_format").unwrap();
    let endpoint = configure.find("native_channel::set_remote_media").unwrap();
    assert!(format < endpoint);
    assert!(configure.contains("device_id: &DeviceId"));
    assert!(configure.contains("local_media_endpoint(self.access, call_id, device_id, codec)"));
}

#[test]
fn video_remote_target_rejects_unusable_endpoints_before_native_mutation() {
    let video = source("src/asterisk/native/channel/video.rs");
    let remote = function_body(
        &video,
        "pub unsafe fn set_remote_video(",
        "pub unsafe fn local_video_endpoint(",
    );
    let zero_port = remote.find("endpoint.port() == 0").unwrap();
    let unspecified = remote.find("endpoint.ip().is_unspecified()").unwrap();
    let multicast = remote.find("endpoint.ip().is_multicast()").unwrap();
    let lock = remote.find("with_locked_video_rtp(channel").unwrap();
    let mutation = remote
        .find("ast_rtp_instance_set_requested_target_address")
        .unwrap();
    assert!(zero_port < lock);
    assert!(unspecified < lock);
    assert!(multicast < lock);
    assert!(lock < mutation);
    let lock_scope = function_body(
        &video,
        "unsafe fn with_locked_video_rtp<T>(",
        "pub unsafe fn set_remote_video(",
    );
    assert!(lock_scope.contains("ChannelLock::acquire(channel)"));
    assert!(lock_scope.contains("private_video_rtp(private)"));
}

#[test]
fn video_qos_is_independent_nonfatal_and_owned_by_video_rtp() {
    let allocation = source("src/asterisk/native/channel/allocation.rs");
    let video = source("src/asterisk/native/channel/video.rs");
    let runtime = source("src/asterisk/runtime/channel.rs");

    assert!(video.contains("apply_media_socket_qos(&instance, configuration.policy)"));
    assert!(runtime.contains("let audio_qos = network"));
    assert!(video.contains("pub policy: RtpPolicy"));
    assert!(allocation.contains("pub rtp_policy: RtpPolicy"));
    assert!(runtime.contains("video: selected_video.ready().map"));
    assert!(
        allocation.contains("Some(Err(error)) => (None, VideoRtpAllocation::Unavailable(error))")
    );
    assert!(allocation.contains("video: Option<OwnedVideoRtp>"));
    assert!(allocation.contains("impl Drop for OwnedRtpInstance"));
    assert!(allocation.contains("ast_rtp_instance_stop(self.as_ptr())"));
    assert!(allocation.contains("ast_rtp_instance_destroy(self.as_ptr())"));
}

#[test]
fn rust_rtp_glue_registration_and_typed_endpoint_are_transactional_and_bounded() {
    let driver = source("src/asterisk/direct/channel_driver.rs");
    let handles = source("src/asterisk/direct/handles.rs");
    let load = function_body(&driver, "pub(super) fn load()", "pub(super) fn unload()");
    assert!(load.contains("NativeChannelRegistration::register("));
    assert!(load.contains("*native_registration = Some(registration)"));

    let unload = function_body(&driver, "pub(super) fn unload()", "pub(super) fn reload()");
    assert!(unload.contains("native_registration().take()"));
    let registration = function_body(
        &handles,
        "impl NativeChannelRegistration",
        "/// Temporarily releases a channel lock",
    );
    let technology = registration
        .find("RegisteredChannelTechnology::register")
        .unwrap();
    let rtp = registration.find("RegisteredRtpGlue::register").unwrap();
    let cli = registration.find("RegisteredCli::register").unwrap();
    assert!(technology < rtp && rtp < cli);
    let owned = function_body(
        &handles,
        "struct NativeChannelRegistration",
        "impl NativeChannelRegistration",
    );
    let cli = owned.find("_cli: RegisteredCli").unwrap();
    let rtp = owned.find("_rtp: RegisteredRtpGlue").unwrap();
    let technology = owned
        .find("_technology: RegisteredChannelTechnology")
        .unwrap();
    assert!(cli < rtp && rtp < technology);

    let channel = source("src/asterisk/native/channel/media.rs");
    assert!(channel.contains("struct LocalMediaEndpoint"));
    assert!(channel.contains("address: IpAddr"));
    assert!(channel.contains("fn set_remote_media("));
    assert!(channel.contains("fn local_media_endpoint("));
    assert!(!channel.contains("callback_guard"));
    assert!(!channel.contains("-> c_int"));
}

#[test]
fn audio_qos_marks_both_owned_media_sockets_without_disrupting_lifecycle() {
    let allocation = source("src/asterisk/native/channel/allocation.rs");
    let media = source("src/asterisk/native/channel/media.rs");
    let runtime = source("src/asterisk/runtime/channel.rs");

    let marking = function_body(
        &allocation,
        "unsafe fn apply_media_socket_qos(",
        "pub unsafe fn channel_private(",
    );
    assert!(marking.contains("(MediaSocketKind::Rtp, 0)"));
    assert!(marking.contains("(MediaSocketKind::Rtcp, 1)"));
    assert!(marking.contains("BorrowedFd::borrow_raw(fd)"));
    assert!(marking.contains("apply_platform_socket_qos(&borrowed, policy)"));
    assert!(marking.contains("MediaSocketQosFailure::Unavailable"));
    assert!(marking.contains("MediaSocketQosFailure::Inspection"));
    assert!(!allocation.contains("ast_rtp_instance_set_qos"));

    let allocate = function_body(
        &allocation,
        "pub unsafe fn allocate_channel(\n",
        "Ok(AllocatedChannel {",
    );
    assert!(allocate.contains("AST_RTP_PROPERTY_RTCP"));
    assert!(allocate.contains("let qos = unsafe { apply_media_socket_qos("));
    assert!(!allocate.contains("qos.is_complete()"));

    let allocate_runtime = function_body(
        &runtime,
        "pub fn allocate_channel(\n",
        "pub fn configured_channel_metadata(",
    );
    let report = allocate_runtime
        .find("for failure in allocation.qos.failures()")
        .unwrap();
    let publish = allocate_runtime
        .find("allocation.channel.as_ptr()")
        .unwrap();
    assert!(report < publish);
    assert!(allocate_runtime.contains_literal("unable to apply configured audio socket QoS"));

    let retarget = function_body(
        &media,
        "pub unsafe fn set_remote_media(",
        "pub unsafe fn local_media_endpoint(",
    );
    assert!(retarget.contains("with_locked_rtp(channel"));
    assert!(retarget.contains("ast_rtp_instance_set_requested_target_address"));
    assert!(!retarget.contains("ast_rtp_instance_new"));

    let drop_rtp = function_body(
        &allocation,
        "impl Drop for OwnedRtpInstance",
        "unsafe fn apply_media_socket_qos(",
    );
    assert!(drop_rtp.contains("ast_rtp_instance_stop"));
    assert!(drop_rtp.contains("ast_rtp_instance_destroy"));
}

#[test]
fn cli_device_controls_are_bounded_and_share_exact_raii_registration() {
    let driver = source("src/asterisk/direct/cli.rs");
    let handles = source("src/asterisk/direct/handles.rs");
    let exports = source("src/asterisk/exports.rs");

    assert!(driver.contains("const CLI_ENTRY_COUNT: usize = 16"));
    assert!(driver.contains("StaticDescriptor<[sys::ast_cli_entry; CLI_ENTRY_COUNT]>"));
    assert!(driver.contains("c\"sccp version\""));
    assert!(driver.contains("execute_version_cli(invocation.fd)"));
    assert!(exports.contains("concat!(env!(\"CARGO_PKG_VERSION\"), \"\\n\")"));
    assert!(driver.contains("ResetMode::Reset"));
    assert!(driver.contains("ResetMode::Restart"));
    assert!(driver.contains("required_c_text("));
    assert!(driver.contains("MAX_DEVICE_SELECTOR_BYTES"));
    assert!(driver.contains("complete_device_control_cli("));
    for command in [
        "c\"sccp show media\"",
        "c\"sccp show media statistics\"",
        "c\"sccp show sessions\"",
        "c\"sccp dnd\"",
        "c\"sccp message\"",
        "c\"sccp answer\"",
        "c\"sccp end\"",
        "c\"sccp originate\"",
    ] {
        assert!(driver.contains(command));
    }
    assert!(driver.contains("operation.accepts_argument_count(count)"));
    assert!(driver.contains("operation.argument_bound(index)"));
    assert!(
        driver.contains("execute_control_cli(invocation.fd, operation, &invocation.arguments)")
    );
    assert!(driver.contains("MAX_CLI_DIAGNOSTIC_ARGUMENTS"));
    assert!(driver.contains("MAX_CLI_DIAGNOSTIC_ARGUMENT_BYTES"));
    assert!(driver.contains("complete_diagnostic_cli("));
    assert!(
        driver.contains("execute_diagnostic_cli(invocation.fd, operation, &invocation.arguments)")
    );
    assert!(!driver.contains("sccp applyconfig"));

    let controls = function_body(
        &exports,
        "fn execute_control_cli_with_access(",
        "fn format_control_cli_outcome(",
    );
    assert!(controls.contains("access.control_provider()"));
    assert!(controls.contains("execute_cli_dnd(&access.feature_control_provider()"));
    for typed in [
        "execute_cli_message(",
        "execute_cli_answer(",
        "execute_cli_end(",
        "execute_cli_originate(",
    ] {
        assert!(controls.contains(typed));
    }

    let registration = function_body(
        &handles,
        "impl RegisteredCli",
        "/// Owns every native registration",
    );
    let registration_drop = function_body(
        &handles,
        "impl Drop for RegisteredCli",
        "/// Owns every native registration",
    );
    assert!(registration.contains("unsafe fn register<const N: usize>"));
    assert!(registration.contains("c_int::try_from(N)"));
    assert!(
        registration_drop
            .contains("ast_cli_unregister_multiple(self.entries.as_ptr(), self.count)")
    );
}

#[test]
fn channel_security_options_report_the_registered_transport_without_mutation() {
    let driver = source("src/asterisk/direct/channel_driver.rs");
    let technology = function_body(&driver, "fn channel_technology()", "fn rtp_glue()");
    assert!(technology.contains("technology.setoption = Some(set_option)"));
    assert!(technology.contains("technology.queryoption = Some(query_option)"));
    assert!(driver.contains("impl TryFrom<c_int> for SecurityOption"));
    assert!(driver.contains("AST_OPTION_SECURE_SIGNALING"));
    assert!(driver.contains("AST_OPTION_SECURE_MEDIA"));

    let exports = source("src/asterisk/exports.rs");
    let security = function_body(
        &exports,
        "pub unsafe fn channel_security(",
        "pub unsafe fn update_rtp_peer(",
    );
    assert!(security.contains("native_channel::channel_security(channel)"));
    assert!(security.contains("map(ChannelSecurity::from)"));

    let runtime = source("src/asterisk/runtime/channel.rs");
    assert!(runtime.contains("device.registration.transport == StationTransport::Secure"));
    assert!(runtime.contains("security: native_channel::NativeChannelSecurity"));
    assert!(runtime.contains("media: false"));
    let allocation = source("src/asterisk/native/channel/allocation.rs");
    assert!(allocation.contains("security: NativeChannelSecurity"));
}

#[test]
fn successful_endpoint_presentation_publishes_native_ringing() {
    let exports = source("src/asterisk/exports.rs");
    let place_call = function_body(
        &exports,
        "pub unsafe fn place_call(",
        "pub unsafe fn hangup_channel(",
    );
    let accepted = place_call.find("if offered == 0").unwrap();
    let ringing = place_call
        .find("native_channel::start_ringing(channel)")
        .unwrap();
    let timers = place_call
        .find("if let Some(route) = no_answer_plan")
        .unwrap();
    assert!(accepted < ringing && ringing < timers);

    let control = source("src/asterisk/native/channel/control.rs");
    let ringing = function_body(&control, "fn start_ringing(", "pub unsafe fn hangup(");
    assert!(ringing.contains("sys::AST_STATE_RINGING"));
    assert!(ringing.contains("ChannelControl::Ringing"));
}

#[test]
fn pbx_hold_indications_drive_asterisk_moh_without_locally_holding_the_handset() {
    let driver = source("src/asterisk/direct/channel_driver.rs");
    let indicate = function_body(
        &driver,
        "unsafe extern \"C\" fn indicate(",
        "fn music_on_hold_class(",
    );
    assert!(indicate.contains("sys::AST_CONTROL_HOLD"));
    assert!(indicate.contains("start_music_on_hold(channel"));
    assert!(indicate.contains("sys::AST_CONTROL_UNHOLD"));
    assert!(indicate.contains("stop_music_on_hold(channel"));
    assert!(!indicate.contains("ChannelIndication::Hold"));
    assert!(!indicate.contains("ChannelIndication::Unhold"));

    let control = source("src/asterisk/native/channel/control.rs");
    let start = function_body(
        &control,
        "pub unsafe fn start_music_on_hold(",
        "pub unsafe fn stop_music_on_hold(",
    );
    assert!(start.contains("sys::ast_moh_start"));
    let stop = function_body(
        &control,
        "pub unsafe fn stop_music_on_hold(",
        "pub unsafe fn uniqueid_in_use(",
    );
    assert!(stop.contains("sys::ast_moh_stop"));

    let services = source("src/asterisk/runtime/services.rs");
    assert!(!services.contains("handle_hold_or_resume(access, call_id, hold, true)"));
}

#[test]
fn rust_native_format_contract_covers_audio_and_supported_video_families() {
    let allocation = source("src/asterisk/native/channel/allocation.rs");
    for format in [
        "G711Ulaw", "G711Alaw", "G722", "G723", "G729", "G726Aal2", "Gsm", "Slin16", "Ilbc",
        "Siren7", "Opus",
    ] {
        assert!(allocation.contains(format));
    }
    assert!(allocation.contains("ast_rtp_codecs_payload_replace_format"));
    assert!(allocation.contains("25,"));
    assert!(allocation.contains("ast_format_slin16"));

    let channel = source("src/asterisk/native/channel/media.rs");
    for format in [
        "AudioCapabilityMask::ULAW",
        "AudioCapabilityMask::ALAW",
        "AudioCapabilityMask::G722",
        "AudioCapabilityMask::G723",
        "AudioCapabilityMask::G729",
        "AudioCapabilityMask::G726_AAL2",
        "AudioCapabilityMask::GSM",
        "AudioCapabilityMask::SLIN16",
        "AudioCapabilityMask::ILBC",
        "AudioCapabilityMask::SIREN7",
        "AudioCapabilityMask::OPUS",
    ] {
        assert!(channel.contains(format));
    }
    for format in [
        "PbxVideoFormat::H261",
        "PbxVideoFormat::H263",
        "PbxVideoFormat::H263Plus",
        "PbxVideoFormat::H264",
        "PbxVideoFormat::H265",
    ] {
        assert!(channel.contains(format));
    }
    assert!(channel.contains("PbxVideoFormat::ALL"));
    assert!(channel.contains("video_format.native_mask()"));

    let video_mask = function_body(
        &channel,
        "fn video_capability_mask(\n",
        "fn audio_framing(\n",
    );
    for format in [
        "ast_format_h261",
        "ast_format_h263",
        "ast_format_h263p",
        "ast_format_h264",
        "ast_format_h265",
    ] {
        assert!(video_mask.contains(format));
    }
    assert!(video_mask.contains("VideoCapabilityMask::all()"));

    let driver = source("src/asterisk/direct/channel_driver.rs");
    assert!(driver.contains("requester_with_stream_topology"));
    assert!(driver.contains("ast_stream_topology_get_formats"));
    let load = function_body(&driver, "pub(super) fn load()", "pub(super) fn unload()");
    for format in [
        "ast_format_h261",
        "ast_format_h263",
        "ast_format_h263p",
        "ast_format_h264",
        "ast_format_h265",
    ] {
        assert!(!load.contains(format));
    }
    let technology_formats = function_body(
        &driver,
        "unsafe fn technology_formats()",
        "pub(super) fn load()",
    );
    assert!(!technology_formats.contains("PbxVideoFormat"));
    assert!(!technology_formats.contains("video_format"));
    assert!(driver.contains("AST_CHAN_TP_WANTSJITTER"));
    assert!(driver.contains("AST_CHAN_TP_CREATESJITTER"));
    assert!(driver.contains("technology.exception = Some(read)"));
    let exports = source("src/asterisk/exports.rs");
    assert!(exports.contains("pub video_capabilities: u32"));
    assert!(exports.contains(
        "// let _peer_video_formats = pbx_video_formats_from_mask(peer.video_capabilities);"
    ));
    let codec = function_body(
        &driver,
        "unsafe extern \"C\" fn get_codec(",
        "unsafe extern \"C\" fn indicate(",
    );
    assert!(codec.contains("sys::AST_MEDIA_TYPE_UNKNOWN"));
}

#[test]
fn rust_rtp_registers_the_skinny_telephone_event_payload_in_both_directions() {
    let allocation = source("src/asterisk/native/channel/allocation.rs");
    let audio = function_body(
        &allocation,
        "unsafe fn configure_audio_payload(",
        "pub unsafe fn allocate_channel(\n",
    );
    assert!(audio.contains("sys::ast_rtp_codecs_payload_replace_format("));
    assert!(audio.contains("format.skinny_rtp_payload()"));
    assert!(audio.contains("sys::ast_rtp_codecs_set_preferred_format(codecs, selected)"));

    let configure = function_body(
        &allocation,
        "unsafe fn configure_rfc2833(",
        "unsafe fn configure_audio_payload(",
    );

    assert!(configure.contains("sys::AST_RTP_PROPERTY_DTMF"));
    assert!(configure.contains("sys::AST_RTP_DTMF_MODE_RFC2833"));
    assert!(configure.contains("sys::ast_rtp_codecs_payloads_set_rtpmap_type_rate("));
    assert!(configure.contains("TELEPHONE_EVENT_PAYLOAD"));
    assert!(configure.contains("c\"telephone-event\""));
    assert!(configure.contains("TELEPHONE_EVENT_SAMPLE_RATE"));
    assert!(configure.contains("sys::ast_rtp_codecs_set_preferred_dtmf_format("));
    assert!(configure.contains("sys::ast_rtp_codecs_payloads_xover(codecs, codecs, rtp)"));

    let allocate = function_body(
        &allocation,
        "pub unsafe fn allocate_channel(\n",
        "Ok(AllocatedChannel {",
    );
    let instance = allocate.find("OwnedRtpInstance::create(").unwrap();
    let phone_payload = allocate
        .find("configure_audio_payload(private.audio.instance.as_ptr(), request.format)")
        .unwrap();
    let dtmf = allocate
        .find("configure_rfc2833(private.audio.instance.as_ptr())")
        .unwrap();
    assert!(instance < phone_payload && phone_payload < dtmf);
}

#[test]
fn every_native_audio_format_uses_its_skinny_rtp_payload() {
    let allocation = source("src/asterisk/native/channel/allocation.rs");
    let payloads = function_body(
        &allocation,
        "const fn skinny_rtp_payload(",
        "pub enum ChannelAllocationError",
    );
    for mapping in [
        "Self::G711Ulaw => 0",
        "Self::G711Alaw => 8",
        "Self::G722 => 9",
        "Self::G723 => 4",
        "Self::G729 => 18",
        "Self::G726Aal2 => 112",
        "Self::Gsm => 3",
        "Self::Slin16 => 25",
        "Self::Ilbc => 97",
        "Self::Siren7 => 102",
        "Self::Opus => 107",
    ] {
        assert!(payloads.contains(mapping), "missing {mapping}");
    }
}
