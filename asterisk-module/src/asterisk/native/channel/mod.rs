//! Typed raw-edge operations for Asterisk channels.
//!
//! Each child owns one native resource concern.  Raw pointers enter only at
//! this edge; ordinary composition code receives `Option`, `Result`, and
//! explicit reference/ownership wrappers instead of C status integers.

mod allocation;
mod completion;
mod control;
mod media;
mod metadata;
mod ownership;
mod party_metadata;
pub mod video;

pub use allocation::{
    ChannelAllocation, ChannelIdentity, NativeAudioFormat, NativeChannelSecurity, RtpPolicy,
    VideoRtpAllocation, allocate_channel, channel_private, destroy_channel_private,
    handoff_channel_to_asterisk, prepare_channel_private_teardown, private_owner, private_rtp,
    private_video_rtp, reassign_private_owner, retain_private_rtp, retain_private_video_rtp,
};
pub use completion::{accept_completion_request, configure_generic_completion};
pub use control::{
    AttendedTransferResult, ChannelControl, TonePair, attended_transfer, hangup, queue_control,
    queue_digit, start_dialplan, start_music_on_hold, start_ringing, start_tone_pair,
    stop_music_on_hold, stop_tone_pair, uniqueid_in_use,
};
pub use media::{
    audio_capability_mask, audio_framing, best_translated_audio_format, change_source,
    identify_audio_format, local_media_endpoint, release_format_cap, send_digit_begin,
    send_digit_end, set_audio_format, set_private_audio_codec, set_remote_media, update_source,
    video_capability_mask,
};
pub use metadata::{channel_identity, channel_pbx_id, channel_security, take_channel_identity};
pub use party_metadata::{NativeChannelMetadataAdapter, NativePartyAdapter, copy_channel_variable};
