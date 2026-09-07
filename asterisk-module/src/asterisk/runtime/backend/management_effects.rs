//! management effects backend-effect translation.

use super::{AsteriskBackend, AsteriskBackendError, ManagementBackend, ManagementEvent};

impl ManagementBackend for AsteriskBackend<'_> {
    fn publish_management_event(&self, event: &ManagementEvent) -> Result<(), Self::Error> {
        self.access
            .shared
            .ami_events
            .enqueue(event)
            .map(|_| ())
            .map_err(AsteriskBackendError::Management)
    }
}
