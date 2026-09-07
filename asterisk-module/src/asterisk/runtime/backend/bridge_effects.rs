//! bridge effects backend-effect translation.

use super::{
    AsteriskBackend, BargeOperation, BridgeBackend, BridgeOperation,
    ConferenceAnnouncementOperation, TransferCompletion, play_conference_announcement,
};

impl BridgeBackend for AsteriskBackend<'_> {
    fn transfer(&self, operation: &TransferCompletion) -> Result<(), Self::Error> {
        self.access.shared.bridge_runtime.execute(
            self.access,
            super::super::bridge_owner::BridgeAction::Transfer(operation.clone()),
        )
    }

    fn bridge(&self, operation: &BridgeOperation) -> Result<(), Self::Error> {
        self.access.shared.bridge_runtime.execute(
            self.access,
            super::super::bridge_owner::BridgeAction::Bridge(operation.clone()),
        )
    }

    fn barge(&self, operation: &BargeOperation) -> Result<(), Self::Error> {
        self.access.shared.bridge_runtime.execute(
            self.access,
            super::super::bridge_owner::BridgeAction::Barge(operation.clone()),
        )
    }

    fn announce(&self, operation: &ConferenceAnnouncementOperation) -> Result<(), Self::Error> {
        play_conference_announcement(self.access, operation)
    }
}
