//! Native effects for the backend-neutral media state owner.

use super::{Access, ChannelBackend as _, DirectMediaCall, PbxCallId, remove_channel};
use crate::runtime::conference_announcement::AnnouncementGeneration;
use crate::runtime::media_ownership::MediaEffects;
use sccp_protocol::ConferenceId;

pub(super) type MediaHandle = crate::runtime::media_ownership::MediaHandle<Access, DirectMediaCall>;
pub(super) type MediaOwner = crate::runtime::media_ownership::MediaOwner<Access, DirectMediaCall>;
pub(super) type MediaReservation =
    crate::runtime::media_ownership::MediaReservation<Access, DirectMediaCall>;
pub(super) type AnchorLease = crate::runtime::media_ownership::AnchorLease<Access, DirectMediaCall>;
pub(super) type AnnouncementTimer = crate::runtime::media_ownership::AnnouncementTimer<Access>;

impl MediaEffects<DirectMediaCall> for Access {
    fn complete_announcement(&self, id: ConferenceId, generation: AnnouncementGeneration) -> bool {
        super::backend::complete_conference_announcement(self, id, generation)
    }

    fn restore_cancelled_anchor(&self, call_id: PbxCallId, restore: &DirectMediaCall) {
        if !super::media::retarget_to_direct(self, restore) {
            let _ = super::AsteriskBackend::new(self).hangup(call_id);
            remove_channel(self, call_id);
        }
    }
}
