use std::ffi::{CStr, c_char, c_int, c_void};
use std::fmt;
use std::mem;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::super::StaticDescriptor;
use super::super::boundary::contain_panic as callback_guard;
use super::super::direct::channel_driver::technology_ptr;
use super::super::direct::module_info::module_self;
use super::super::sys;
use super::bridge::{acquire_barge_bridge, create_bridge, prepare_conference_destination};
use super::handles::{Ao2Object, ChannelLock, ChannelRef, ModuleReference};
use super::system::cli_write;
use crate::pbx::operations::{BridgeControl, CallFeatureError};
use crate::pbx::party::AsteriskChannel;

const SOURCE_FILE: &CStr = c"ci/live-tests/bridge.rs";
const SOURCE_FUNCTION: &CStr = c"live_bridge_harness";
const MODULE_RESOURCE: &CStr = c"chan_sccp2.so";
const COMMAND: &CStr = c"sccp test bridges";
const USAGE: &CStr = c"Usage: sccp test bridges\n";
const SUMMARY: &[u8] = b"Run isolated native bridge ownership tests\0";
const WAIT_LIMIT: Duration = Duration::from_secs(3);
const HARNESS_ONLY_REFS: c_int = 1;
const ALLOCATED_CHANNEL_REFS: c_int = 2;
const BRIDGED_CHANNEL_REFS: c_int = 3;
const BARGE_ANCHORED_CHANNEL_REFS: c_int = 4;

static SYNTHETIC_TECH: StaticDescriptor<sys::ast_channel_tech> = StaticDescriptor::uninit();
static NEXT_RUN: AtomicU64 = AtomicU64::new(1);
static ALLOCATED_CHANNELS: AtomicUsize = AtomicUsize::new(0);
static RELEASED_CHANNELS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct HarnessError(String);

impl HarnessError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for HarnessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<CallFeatureError> for HarnessError {
    fn from(error: CallFeatureError) -> Self {
        Self(error.to_string())
    }
}

type HarnessResult<T> = Result<T, HarnessError>;

struct SyntheticChannel {
    channel: Option<ChannelRef>,
}

impl SyntheticChannel {
    fn new(run: u64, label: &str) -> HarnessResult<Self> {
        let label = std::ffi::CString::new(format!("{run}-{label}"))
            .map_err(|_| HarnessError::new("synthetic channel label contains a NUL byte"))?;
        let module = unsafe { ModuleReference::acquire(module_self()) }
            .ok_or_else(|| HarnessError::new("unable to retain the loaded module"))?;
        let capabilities = unsafe {
            Ao2Object::from_owned(sys::__ast_format_cap_alloc(
                sys::AST_FORMAT_CAP_FLAG_DEFAULT,
                c"live bridge channel capabilities".as_ptr(),
                SOURCE_FILE.as_ptr(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr(),
            ))
        }
        .ok_or_else(|| HarnessError::new("unable to allocate channel capabilities"))?;
        let append = unsafe {
            sys::__ast_format_cap_append(
                capabilities.as_ptr(),
                sys::ast_format_ulaw,
                0,
                c"live bridge channel capabilities".as_ptr(),
                SOURCE_FILE.as_ptr(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr(),
            )
        };
        ensure(append == 0, "unable to append the synthetic audio format")?;

        let private = Box::new(SyntheticPrivate { _module: module });
        let channel = NonNull::new(unsafe {
            sys::__ast_channel_alloc(
                1,
                sys::AST_STATE_UP as c_int,
                c"".as_ptr(),
                c"".as_ptr(),
                c"".as_ptr(),
                c"".as_ptr(),
                c"default".as_ptr(),
                ptr::null(),
                ptr::null(),
                sys::AST_AMA_NONE,
                ptr::null_mut(),
                SOURCE_FILE.as_ptr(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr(),
                c"SCCPLive/%s".as_ptr(),
                label.as_ptr(),
            )
        })
        .ok_or_else(|| HarnessError::new("unable to allocate a synthetic channel"))?;
        let channel = unsafe { ChannelRef::from_owned(channel.as_ptr()) }
            .ok_or_else(|| HarnessError::new("channel allocation returned null"))?;
        let locked = unsafe { ChannelLock::from_locked(channel) };
        unsafe {
            sys::ast_channel_tech_set(locked.as_ptr(), synthetic_technology());
            sys::ast_channel_tech_pvt_set(locked.as_ptr(), Box::into_raw(private).cast());
            sys::ast_channel_nativeformats_set(locked.as_ptr(), capabilities.as_ptr());
            sys::ast_channel_set_writeformat(locked.as_ptr(), sys::ast_format_ulaw);
            sys::ast_channel_set_rawwriteformat(locked.as_ptr(), sys::ast_format_ulaw);
            sys::ast_channel_set_readformat(locked.as_ptr(), sys::ast_format_ulaw);
            sys::ast_channel_set_rawreadformat(locked.as_ptr(), sys::ast_format_ulaw);
        }
        ALLOCATED_CHANNELS.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            channel: Some(locked.unlock()),
        })
    }

    fn raw(&self) -> *mut sys::ast_channel {
        self.channel
            .as_ref()
            .map(ChannelRef::as_ptr)
            .unwrap_or(ptr::null_mut())
    }

    fn borrowed(&self) -> HarnessResult<AsteriskChannel<'_>> {
        unsafe { AsteriskChannel::from_raw(self.raw().cast()) }
            .map_err(|error| HarnessError::new(error.to_string()))
    }

    fn reference_count(&self) -> c_int {
        unsafe {
            sys::__ao2_ref(
                self.raw().cast(),
                0,
                ptr::null(),
                SOURCE_FILE.as_ptr(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr(),
            )
        }
    }

    fn private_released(&self) -> bool {
        unsafe { sys::ast_channel_tech_pvt(self.raw()).is_null() }
    }
}

impl Drop for SyntheticChannel {
    fn drop(&mut self) {
        let Some(channel) = self.channel.take() else {
            return;
        };
        if unsafe { sys::ast_channel_tech_pvt(channel.as_ptr()).is_null() } {
            drop(channel);
        } else {
            unsafe { sys::ast_hangup(channel.into_raw()) };
        }
    }
}

struct SyntheticPrivate {
    _module: ModuleReference,
}

impl Drop for SyntheticPrivate {
    fn drop(&mut self) {
        RELEASED_CHANNELS.fetch_add(1, Ordering::Relaxed);
    }
}

unsafe extern "C" fn synthetic_hangup(channel: *mut sys::ast_channel) -> c_int {
    if channel.is_null() {
        return -1;
    }
    let private = unsafe { sys::ast_channel_tech_pvt(channel) }.cast::<SyntheticPrivate>();
    unsafe { sys::ast_channel_tech_pvt_set(channel, ptr::null_mut()) };
    if !private.is_null() {
        drop(unsafe { Box::from_raw(private) });
    }
    0
}

unsafe extern "C" fn synthetic_read(_: *mut sys::ast_channel) -> *mut sys::ast_frame {
    ptr::addr_of_mut!(sys::ast_null_frame)
}

unsafe extern "C" fn synthetic_write(_: *mut sys::ast_channel, _: *mut sys::ast_frame) -> c_int {
    0
}

fn synthetic_technology() -> *mut sys::ast_channel_tech {
    static INITIALIZE: std::sync::Once = std::sync::Once::new();
    INITIALIZE.call_once(|| unsafe {
        let mut technology = mem::zeroed::<sys::ast_channel_tech>();
        technology.type_ = c"SCCPLive".as_ptr();
        technology.description = c"Live bridge test channel".as_ptr();
        technology.hangup = Some(synthetic_hangup);
        technology.read = Some(synthetic_read);
        technology.write = Some(synthetic_write);
        SYNTHETIC_TECH.write(technology);
    });
    unsafe { SYNTHETIC_TECH.as_ptr() }
}

#[derive(Clone, Copy)]
struct Snapshot {
    module_uses: c_int,
    bridges: c_int,
    allocated: usize,
    released: usize,
}

impl Snapshot {
    fn capture() -> HarnessResult<Self> {
        Ok(Self {
            module_uses: module_use_count()?,
            bridges: bridge_count()?,
            allocated: ALLOCATED_CHANNELS.load(Ordering::Relaxed),
            released: RELEASED_CHANNELS.load(Ordering::Relaxed),
        })
    }

    fn assert_restored(self, bridge_ids: &[String]) -> HarnessResult<()> {
        wait_for("native resources to return to their baseline", || {
            let resources_match = module_use_count().is_ok_and(|uses| uses == self.module_uses)
                && bridge_count().is_ok_and(|count| count == self.bridges)
                && ALLOCATED_CHANNELS.load(Ordering::Relaxed) - self.allocated
                    == RELEASED_CHANNELS.load(Ordering::Relaxed) - self.released;
            resources_match && bridge_ids.iter().all(|id| bridge_absent(id))
        })
    }
}

fn ensure(condition: bool, message: impl Into<String>) -> HarnessResult<()> {
    condition
        .then_some(())
        .ok_or_else(|| HarnessError::new(message))
}

fn wait_for(description: &str, mut ready: impl FnMut() -> bool) -> HarnessResult<()> {
    let deadline = Instant::now() + WAIT_LIMIT;
    while Instant::now() < deadline {
        if ready() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(HarnessError::new(format!(
        "timed out waiting for {description}"
    )))
}

fn bridge_id(run: u64, suffix: &str) -> String {
    format!("sccp-live-{run}-{suffix}")
}

fn bridge_absent(id: &str) -> bool {
    let Ok(id) = std::ffi::CString::new(id) else {
        return false;
    };
    let bridge = unsafe { sys::ast_bridge_find_by_id(id.as_ptr()) };
    if bridge.is_null() {
        true
    } else {
        unsafe {
            sys::__ao2_ref(
                bridge.cast(),
                -1,
                ptr::null(),
                SOURCE_FILE.as_ptr(),
                line!() as c_int,
                SOURCE_FUNCTION.as_ptr(),
            );
        }
        false
    }
}

fn channel_in_bridge(channel: &SyntheticChannel, id: &str) -> bool {
    let Ok(id) = std::ffi::CString::new(id) else {
        return false;
    };
    let Some(channel) = (unsafe { ChannelRef::acquire(channel.raw()) }) else {
        return false;
    };
    let Ok(lock) = ChannelLock::acquire(channel) else {
        return false;
    };
    let current = unsafe { sys::ast_channel_get_bridge(lock.as_ptr().cast()) };
    drop(lock);
    let Some(current) = (unsafe { Ao2Object::from_owned(current) }) else {
        return false;
    };
    let expected = unsafe { sys::ast_bridge_find_by_id(id.as_ptr()) };
    let Some(expected) = (unsafe { Ao2Object::from_owned(expected) }) else {
        return false;
    };
    current.as_ptr() == expected.as_ptr()
}

fn channel_unbridged(channel: &SyntheticChannel) -> bool {
    let Some(channel) = (unsafe { ChannelRef::acquire(channel.raw()) }) else {
        return false;
    };
    let Ok(lock) = ChannelLock::acquire(channel) else {
        return false;
    };
    let bridge = unsafe { sys::ast_channel_get_bridge(lock.as_ptr().cast()) };
    drop(lock);
    let Some(bridge) = (unsafe { Ao2Object::from_owned(bridge) }) else {
        return true;
    };
    drop(bridge);
    false
}

fn bridge_count() -> HarnessResult<c_int> {
    let bridges = unsafe { Ao2Object::from_owned(sys::ast_bridges()) }
        .ok_or_else(|| HarnessError::new("unable to inspect the bridge container"))?;
    Ok(unsafe { sys::ao2_container_count(bridges.as_ptr()) })
}

#[derive(Default)]
struct ModuleUseCount {
    found: bool,
    value: c_int,
}

unsafe extern "C" fn capture_module_use_count(
    module: *const c_char,
    _: *const c_char,
    use_count: c_int,
    _: *const c_char,
    _: *const c_char,
    _: sys::ast_module_support_level,
    data: *mut c_void,
) -> c_int {
    if module.is_null() || data.is_null() {
        return 0;
    }
    if unsafe { CStr::from_ptr(module) } != MODULE_RESOURCE {
        return 0;
    }
    let count = unsafe { &mut *data.cast::<ModuleUseCount>() };
    count.found = true;
    count.value = use_count;
    1
}

fn module_use_count() -> HarnessResult<c_int> {
    let mut count = ModuleUseCount::default();
    unsafe {
        sys::ast_update_module_list_data(
            Some(capture_module_use_count),
            MODULE_RESOURCE.as_ptr(),
            (&mut count as *mut ModuleUseCount).cast(),
        );
    }
    count
        .found
        .then_some(count.value)
        .ok_or_else(|| HarnessError::new("unable to inspect the module reference count"))
}

fn expect_conflict(result: Result<(), CallFeatureError>, context: &str) -> HarnessResult<()> {
    ensure(
        matches!(result, Err(CallFeatureError::Conflict { .. })),
        format!("{context} did not reject the conflicting state"),
    )
}

fn expect_not_found(result: Result<(), CallFeatureError>, context: &str) -> HarnessResult<()> {
    ensure(
        matches!(result, Err(CallFeatureError::NotFound { .. })),
        format!("{context} did not reject the missing state"),
    )
}

fn expect_invalid(result: Result<(), CallFeatureError>, context: &str) -> HarnessResult<()> {
    ensure(
        matches!(result, Err(CallFeatureError::InvalidInput { .. })),
        format!("{context} did not reject invalid input"),
    )
}

fn admit_two_party_source(
    bridge: &mut dyn BridgeControl,
    bridge_id: &str,
    members: [&SyntheticChannel; 2],
) -> HarnessResult<()> {
    ensure(
        members.iter().all(|channel| {
            channel.reference_count() == ALLOCATED_CHANNEL_REFS && channel_unbridged(channel)
        }),
        format!(
            "source bridge {bridge_id} members did not start with harness and registry ownership"
        ),
    )?;
    for channel in members {
        bridge.add(&channel.borrowed()?)?;
    }
    wait_for(
        &format!("two-party source bridge {bridge_id} admission"),
        || {
            members.iter().all(|channel| {
                channel.reference_count() == BRIDGED_CHANNEL_REFS
                    && channel_in_bridge(channel, bridge_id)
            })
        },
    )
}

fn wait_for_merged_members(bridge_id: &str, members: &[&SyntheticChannel]) -> HarnessResult<()> {
    wait_for(&format!("merged members in bridge {bridge_id}"), || {
        members.iter().all(|channel| {
            channel.reference_count() == BRIDGED_CHANNEL_REFS
                && channel_in_bridge(channel, bridge_id)
        })
    })
}

fn wait_for_owned_release(members: &[&SyntheticChannel]) -> HarnessResult<()> {
    wait_for("merged channel ownership release", || {
        members.iter().all(|channel| {
            channel.private_released()
                && channel.reference_count() == HARNESS_ONLY_REFS
                && channel_unbridged(channel)
        })
    })
}

fn create_add_remove(run: u64) -> HarnessResult<()> {
    let snapshot = Snapshot::capture()?;
    let id = bridge_id(run, "add-remove");
    {
        let channel = SyntheticChannel::new(run, "add-remove")?;
        ensure(
            channel.reference_count() == ALLOCATED_CHANNEL_REFS,
            "new channel did not have exactly one harness and one registry reference",
        )?;
        let mut bridge = create_bridge(&id)?;
        bridge.add(&channel.borrowed()?)?;
        wait_for("channel bridge admission", || {
            channel_in_bridge(&channel, &id)
        })?;
        ensure(
            channel.reference_count() == BRIDGED_CHANNEL_REFS,
            "bridge admission did not consume exactly one transferred reference",
        )?;
        bridge.remove(&channel.borrowed()?)?;
        wait_for("removed channel release", || {
            channel.private_released()
                && channel.reference_count() == HARNESS_ONLY_REFS
                && channel_unbridged(&channel)
        })?;
        bridge.destroy()?;
    }
    snapshot.assert_restored(&[id])
}

fn explicit_destruction(run: u64) -> HarnessResult<()> {
    let snapshot = Snapshot::capture()?;
    let id = bridge_id(run, "destroy");
    {
        let channel = SyntheticChannel::new(run, "destroy")?;
        let mut bridge = create_bridge(&id)?;
        bridge.add(&channel.borrowed()?)?;
        wait_for("participant before bridge destruction", || {
            channel_in_bridge(&channel, &id)
        })?;
        bridge.destroy()?;
        wait_for("participant release after bridge destruction", || {
            channel.private_released()
                && channel.reference_count() == HARNESS_ONLY_REFS
                && channel_unbridged(&channel)
        })?;
    }
    snapshot.assert_restored(&[id])
}

fn consultation_merge(run: u64) -> HarnessResult<()> {
    let snapshot = Snapshot::capture()?;
    let ids = [
        bridge_id(run, "consult-source-a"),
        bridge_id(run, "consult-source-b"),
        bridge_id(run, "consult-target"),
    ];
    {
        let original = SyntheticChannel::new(run, "consult-original")?;
        let original_peer = SyntheticChannel::new(run, "consult-original-peer")?;
        let consultation = SyntheticChannel::new(run, "consult-second")?;
        let consultation_peer = SyntheticChannel::new(run, "consult-second-peer")?;
        let mut source_a = create_bridge(&ids[0])?;
        let mut source_b = create_bridge(&ids[1])?;
        let mut target = create_bridge(&ids[2])?;
        admit_two_party_source(source_a.as_mut(), &ids[0], [&original, &original_peer])?;
        admit_two_party_source(
            source_b.as_mut(),
            &ids[1],
            [&consultation, &consultation_peer],
        )?;
        target.merge_consultation(&original.borrowed()?, &consultation.borrowed()?)?;
        let members = [&original, &original_peer, &consultation, &consultation_peer];
        wait_for_merged_members(&ids[2], &members)?;
        source_a.destroy()?;
        source_b.destroy()?;
        target.destroy()?;
        wait_for_owned_release(&members)?;
    }
    snapshot.assert_restored(&ids)
}

fn multiple_call_merge(run: u64) -> HarnessResult<()> {
    let snapshot = Snapshot::capture()?;
    let source_ids = [
        bridge_id(run, "multi-source-a"),
        bridge_id(run, "multi-source-b"),
        bridge_id(run, "multi-source-c"),
    ];
    let target_id = bridge_id(run, "multi-target");
    {
        let calls = [
            SyntheticChannel::new(run, "multi-a")?,
            SyntheticChannel::new(run, "multi-b")?,
            SyntheticChannel::new(run, "multi-c")?,
        ];
        let peers = [
            SyntheticChannel::new(run, "multi-a-peer")?,
            SyntheticChannel::new(run, "multi-b-peer")?,
            SyntheticChannel::new(run, "multi-c-peer")?,
        ];
        let mut sources = Vec::<Box<dyn BridgeControl>>::new();
        for ((call, peer), id) in calls.iter().zip(&peers).zip(&source_ids) {
            let mut source = create_bridge(id)?;
            admit_two_party_source(source.as_mut(), id, [call, peer])?;
            sources.push(source);
        }
        let mut target = create_bridge(&target_id)?;
        let borrowed = calls
            .iter()
            .map(SyntheticChannel::borrowed)
            .collect::<HarnessResult<Vec<_>>>()?;
        target.merge_calls(&borrowed)?;
        let members = calls.iter().chain(&peers).collect::<Vec<_>>();
        wait_for_merged_members(&target_id, &members)?;
        for source in sources {
            source.destroy()?;
        }
        target.destroy()?;
        wait_for_owned_release(&members)?;
    }
    let ids = source_ids
        .into_iter()
        .chain(std::iter::once(target_id))
        .collect::<Vec<_>>();
    snapshot.assert_restored(&ids)
}

fn participant_merge(run: u64) -> HarnessResult<()> {
    let snapshot = Snapshot::capture()?;
    let source_id = bridge_id(run, "participant-source");
    let target_id = bridge_id(run, "participant-target");
    {
        let existing = SyntheticChannel::new(run, "participant-existing")?;
        let participant = SyntheticChannel::new(run, "participant-new")?;
        let participant_peer = SyntheticChannel::new(run, "participant-peer")?;
        let mut source = create_bridge(&source_id)?;
        let mut target = create_bridge(&target_id)?;
        admit_two_party_source(
            source.as_mut(),
            &source_id,
            [&participant, &participant_peer],
        )?;
        ensure(
            existing.reference_count() == ALLOCATED_CHANNEL_REFS && channel_unbridged(&existing),
            "target participant did not start with harness and registry ownership",
        )?;
        target.add(&existing.borrowed()?)?;
        wait_for("existing target participant admission", || {
            existing.reference_count() == BRIDGED_CHANNEL_REFS
                && channel_in_bridge(&existing, &target_id)
        })?;
        target.merge_participant(&participant.borrowed()?)?;
        let members = [&existing, &participant, &participant_peer];
        wait_for_merged_members(&target_id, &members)?;
        source.destroy()?;
        target.destroy()?;
        wait_for_owned_release(&members)?;
    }
    snapshot.assert_restored(&[source_id, target_id])
}

fn participant_controls(run: u64) -> HarnessResult<()> {
    let snapshot = Snapshot::capture()?;
    let id = bridge_id(run, "participant-controls");
    {
        let removed = SyntheticChannel::new(run, "participant-removed")?;
        let held = SyntheticChannel::new(run, "participant-held")?;
        let mut bridge = create_bridge(&id)?;
        bridge.add(&removed.borrowed()?)?;
        bridge.add(&held.borrowed()?)?;
        wait_for("controlled participants to enter", || {
            channel_in_bridge(&removed, &id) && channel_in_bridge(&held, &id)
        })?;
        bridge.set_participant_muted(&removed.borrowed()?, true)?;
        bridge.set_participant_muted(&removed.borrowed()?, false)?;
        bridge.set_participant_music_on_hold(&held.borrowed()?, "default", true)?;
        bridge.set_participant_music_on_hold(&held.borrowed()?, "default", false)?;
        bridge.remove_participant_and_hangup(&removed.borrowed()?)?;
        wait_for("queued participant removal", || {
            removed.private_released()
                && removed.reference_count() == HARNESS_ONLY_REFS
                && channel_unbridged(&removed)
        })?;
        bridge.remove(&held.borrowed()?)?;
        wait_for("explicit participant removal", || {
            held.private_released()
                && held.reference_count() == HARNESS_ONLY_REFS
                && channel_unbridged(&held)
        })?;
        bridge.destroy()?;
    }
    snapshot.assert_restored(&[id])
}

fn pbx_hold_indication_controls_music_on_hold(run: u64) -> HarnessResult<()> {
    let snapshot = Snapshot::capture()?;
    {
        let channel = SyntheticChannel::new(run, "pbx-hold-moh")?;
        let indicate = unsafe { (*technology_ptr()).indicate }
            .ok_or_else(|| HarnessError::new("SCCP technology has no indication callback"))?;
        let music_class = c"default";
        let held = unsafe {
            indicate(
                channel.raw(),
                sys::AST_CONTROL_HOLD as c_int,
                music_class.as_ptr().cast(),
                music_class.to_bytes_with_nul().len(),
            )
        };
        ensure(held == 0, "PBX hold indication rejected music on hold")?;
        ensure(
            !unsafe { sys::ast_channel_generator(channel.raw()) }.is_null(),
            "PBX hold indication did not start the Asterisk media generator",
        )?;

        let unheld = unsafe {
            indicate(
                channel.raw(),
                sys::AST_CONTROL_UNHOLD as c_int,
                ptr::null(),
                0,
            )
        };
        ensure(unheld == 0, "PBX unhold indication was rejected")?;
        ensure(
            unsafe { sys::ast_channel_generator(channel.raw()) }.is_null(),
            "PBX unhold indication did not stop the Asterisk media generator",
        )?;

        let malformed_class = b"default";
        let malformed = unsafe {
            indicate(
                channel.raw(),
                sys::AST_CONTROL_HOLD as c_int,
                malformed_class.as_ptr().cast(),
                malformed_class.len(),
            )
        };
        ensure(
            malformed == -1,
            "PBX hold indication accepted an unterminated music class",
        )?;
        ensure(
            unsafe { sys::ast_channel_generator(channel.raw()) }.is_null(),
            "rejected PBX hold indication changed the channel generator",
        )?;
    }
    snapshot.assert_restored(&[])
}

fn barge_acquire_release(run: u64) -> HarnessResult<()> {
    let snapshot = Snapshot::capture()?;
    let owned_id = bridge_id(run, "barge-owned");
    let existing_id = bridge_id(run, "barge-existing");
    let borrowed_id = bridge_id(run, "barge-borrowed");
    {
        let target = SyntheticChannel::new(run, "barge-owned-target")?;
        let barger = SyntheticChannel::new(run, "barge-owned-joiner")?;
        let mut barge = acquire_barge_bridge(&owned_id, &target.borrowed()?)?;
        wait_for("owned barge anchor", || {
            channel_in_bridge(&target, &owned_id)
        })?;
        ensure(
            target.reference_count() == BARGE_ANCHORED_CHANNEL_REFS,
            "owned barge did not retain one anchor and one transferred reference",
        )?;
        barge.add(&barger.borrowed()?)?;
        barge.remove(&barger.borrowed()?)?;
        wait_for("owned barge participant release", || {
            barger.private_released() && barger.reference_count() == HARNESS_ONLY_REFS
        })?;
        barge.release()?;
        wait_for("owned barge release", || {
            target.private_released()
                && target.reference_count() == HARNESS_ONLY_REFS
                && channel_unbridged(&target)
        })?;

        let existing_target = SyntheticChannel::new(run, "barge-existing-target")?;
        let borrowed_barger = SyntheticChannel::new(run, "barge-borrowed-joiner")?;
        let mut owner = create_bridge(&existing_id)?;
        owner.add(&existing_target.borrowed()?)?;
        let mut borrowed = acquire_barge_bridge(&borrowed_id, &existing_target.borrowed()?)?;
        ensure(
            existing_target.reference_count() == BARGE_ANCHORED_CHANNEL_REFS,
            "borrowed barge did not retain the anchor separately",
        )?;
        borrowed.add(&borrowed_barger.borrowed()?)?;
        borrowed.remove(&borrowed_barger.borrowed()?)?;
        wait_for("borrowed barge participant release", || {
            borrowed_barger.private_released()
                && borrowed_barger.reference_count() == HARNESS_ONLY_REFS
        })?;
        borrowed.release()?;
        ensure(
            existing_target.reference_count() == BRIDGED_CHANNEL_REFS
                && channel_in_bridge(&existing_target, &existing_id),
            "borrowed barge release disturbed the existing bridge",
        )?;
        ensure(
            bridge_absent(&borrowed_id),
            "borrowed barge created an unnecessary bridge",
        )?;
        owner.destroy()?;
    }
    snapshot.assert_restored(&[owned_id, existing_id, borrowed_id])
}

fn recoverable_failures(run: u64) -> HarnessResult<()> {
    let snapshot = Snapshot::capture()?;
    let id = bridge_id(run, "failure-primary");
    let other_id = bridge_id(run, "failure-other");
    {
        let member = SyntheticChannel::new(run, "failure-member")?;
        let unbridged = SyntheticChannel::new(run, "failure-unbridged")?;
        let mut bridge = create_bridge(&id)?;
        let duplicate = create_bridge(&id);
        ensure(
            matches!(duplicate, Err(CallFeatureError::Conflict { .. })),
            "duplicate bridge creation did not fail without consuming the original",
        )?;
        bridge.add(&member.borrowed()?)?;
        wait_for("failure-test participant admission", || {
            channel_in_bridge(&member, &id)
        })?;
        expect_conflict(bridge.add(&member.borrowed()?), "duplicate participant add")?;

        let mut other = create_bridge(&other_id)?;
        expect_conflict(
            other.remove(&member.borrowed()?),
            "wrong-bridge participant removal",
        )?;
        expect_not_found(
            bridge.merge_consultation(&unbridged.borrowed()?, &member.borrowed()?),
            "consultation merge with an unbridged channel",
        )?;
        expect_invalid(
            bridge.merge_calls(&[member.borrowed()?]),
            "single-call merge",
        )?;
        expect_conflict(
            bridge.merge_participant(&member.borrowed()?),
            "participant merge into its current bridge",
        )?;
        expect_conflict(
            other.set_participant_muted(&member.borrowed()?, true),
            "wrong-bridge mute",
        )?;
        expect_conflict(
            other.set_participant_music_on_hold(&member.borrowed()?, "default", true),
            "wrong-bridge music on hold",
        )?;
        expect_conflict(
            other.remove_participant_and_hangup(&member.borrowed()?),
            "wrong-bridge terminal removal",
        )?;
        ensure(
            member.reference_count() == BRIDGED_CHANNEL_REFS && channel_in_bridge(&member, &id),
            "failed operations changed the successfully transferred channel reference",
        )?;
        ensure(
            unbridged.reference_count() == ALLOCATED_CHANNEL_REFS && channel_unbridged(&unbridged),
            "failed operations consumed an unbridged channel reference",
        )?;
        other.destroy()?;
        bridge.destroy()?;
    }
    snapshot.assert_restored(&[id, other_id])
}

fn conference_destination_teardown(run: u64) -> HarnessResult<()> {
    let snapshot = Snapshot::capture()?;
    {
        let channel = SyntheticChannel::new(run, "destination-live")?;
        let (application, cancellation) = prepare_conference_destination(
            &channel.borrowed()?,
            &format!("sccp-live-{run},default_bridge,default_user"),
        )?;
        ensure(
            module_use_count()? == snapshot.module_uses + 2,
            "conference destination did not retain its channel and module ownership",
        )?;
        let task = thread::spawn(move || application.run());
        wait_for("conference destination bridge admission", || {
            !channel_unbridged(&channel)
        })?;
        cancellation.cancel();
        let result = task
            .join()
            .map_err(|_| HarnessError::new("conference destination worker panicked"))?;
        result?;
        wait_for("conference destination bridge teardown", || {
            channel_unbridged(&channel)
        })?;
        ensure(
            module_use_count()? == snapshot.module_uses + 1,
            "conference destination retained its module reference after completion",
        )?;
    }
    snapshot.assert_restored(&[])
}

fn cancelled_destination_teardown(run: u64) -> HarnessResult<()> {
    let snapshot = Snapshot::capture()?;
    {
        let channel = SyntheticChannel::new(run, "destination-cancelled")?;
        let (application, cancellation) = prepare_conference_destination(
            &channel.borrowed()?,
            &format!("sccp-live-{run}-cancelled,default_bridge,default_user"),
        )?;
        cancellation.cancel();
        application.run()?;
        ensure(
            channel_unbridged(&channel),
            "cancelled destination entered a bridge",
        )?;
        ensure(
            module_use_count()? == snapshot.module_uses + 1,
            "cancelled destination retained its module reference",
        )?;
    }
    snapshot.assert_restored(&[])
}

fn run_harness() -> HarnessResult<usize> {
    let run = NEXT_RUN
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .map_err(|_| HarnessError::new("live bridge run identifier space is exhausted"))?;
    let scenarios: [fn(u64) -> HarnessResult<()>; 11] = [
        create_add_remove,
        explicit_destruction,
        consultation_merge,
        multiple_call_merge,
        participant_merge,
        participant_controls,
        pbx_hold_indication_controls_music_on_hold,
        barge_acquire_release,
        recoverable_failures,
        conference_destination_teardown,
        cancelled_destination_teardown,
    ];
    for scenario in scenarios {
        scenario(run)?;
    }
    Ok(scenarios.len())
}

unsafe extern "C" fn cli_handler(
    entry: *mut sys::ast_cli_entry,
    command: c_int,
    arguments: *mut sys::ast_cli_args,
) -> *mut c_char {
    callback_guard(ptr::null_mut(), || match command {
        -2 => {
            if let Some(entry) = NonNull::new(entry) {
                unsafe {
                    (*entry.as_ptr()).command = COMMAND.as_ptr().cast_mut();
                    (*entry.as_ptr()).usage = USAGE.as_ptr();
                }
            }
            ptr::null_mut()
        }
        -3 => ptr::null_mut(),
        _ => {
            let Some(arguments) = NonNull::new(arguments) else {
                return 1usize as *mut c_char;
            };
            let arguments = unsafe { arguments.as_ref() };
            if arguments.argc != 3 {
                return 1usize as *mut c_char;
            }
            match run_harness() {
                Ok(scenarios) => cli_write(
                    arguments.fd,
                    &format!("CONF-020 PASS scenarios={scenarios}\n"),
                ),
                Err(error) => cli_write(arguments.fd, &format!("CONF-020 FAIL {error}\n")),
            }
            ptr::null_mut()
        }
    })
}

pub(super) fn cli_entry() -> sys::ast_cli_entry {
    let mut entry = unsafe { mem::zeroed::<sys::ast_cli_entry>() };
    entry.summary = SUMMARY.as_ptr().cast();
    entry.handler = Some(cli_handler);
    entry
}
