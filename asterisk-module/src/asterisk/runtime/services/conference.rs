//! Conference service operations and service-error projection.

use super::{
    Access, AmiConferenceCommand, ConferenceEndRejection, ConferenceId,
    ConferenceParticipantRejection, ConferencePhase, ParticipantId, ServiceOutcome,
    ServiceProviderError, cancel_conference_announcement, execute_service_cleanup,
    remove_conference_participant, set_conference_participant_moderator,
    set_conference_participant_muted,
};

pub async fn conference_service_operation(
    access: &Access,
    command: AmiConferenceCommand,
    conference_id: ConferenceId,
    participant_id: Option<ParticipantId>,
) -> Result<ServiceOutcome, ServiceProviderError> {
    let session = access
        .shared
        .controller
        .snapshot()
        .conference_session_by_id(conference_id)
        .cloned()
        .filter(|session| session.phase == ConferencePhase::Active)
        .ok_or(ServiceProviderError::ConferenceNotFound)?;
    match command {
        AmiConferenceCommand::End => {
            let effects = access
                .shared
                .controller
                .end_conference_by_moderator(&session.device_id, conference_id)
                .unwrap_or_else(|_| {
                    Err(crate::runtime::controller::ConferenceEndRejection::Unavailable)
                })
                .map_err(conference_end_service_error)?;
            cancel_conference_announcement(access, conference_id);
            execute_service_cleanup(access, effects).await?;
        }
        AmiConferenceCommand::Kick => {
            remove_conference_participant(
                access,
                session,
                participant_id.ok_or(ServiceProviderError::ParticipantNotFound)?,
            )
            .await?;
        }
        AmiConferenceCommand::Mute => {
            let participant_id = participant_id.ok_or(ServiceProviderError::ParticipantNotFound)?;
            let muted = session
                .participants
                .get(participant_id)
                .map(|participant| !participant.muted)
                .ok_or(ServiceProviderError::ParticipantNotFound)?;
            set_conference_participant_muted(access, session, participant_id, muted).await?;
        }
        AmiConferenceCommand::Moderate => {
            let participant_id = participant_id.ok_or(ServiceProviderError::ParticipantNotFound)?;
            let moderator = session
                .participants
                .get(participant_id)
                .map(|participant| !participant.moderator)
                .ok_or(ServiceProviderError::ParticipantNotFound)?;
            set_conference_participant_moderator(access, session, participant_id, moderator)
                .await?;
        }
        AmiConferenceCommand::Invite => return Err(ServiceProviderError::Unsupported),
    }
    Ok(ServiceOutcome::Conference {
        command,
        conference_id,
        participant_id,
    })
}

pub fn conference_participant_service_error(
    error: ConferenceParticipantRejection,
) -> ServiceProviderError {
    match error {
        ConferenceParticipantRejection::Unavailable => ServiceProviderError::ConferenceNotFound,
        ConferenceParticipantRejection::NotModerator => {
            ServiceProviderError::ConferenceAuthorization
        }
        ConferenceParticipantRejection::InvalidParticipant => {
            ServiceProviderError::ParticipantNotFound
        }
        ConferenceParticipantRejection::Moderator
        | ConferenceParticipantRejection::LastModerator
        | ConferenceParticipantRejection::Conflict => ServiceProviderError::ConferenceConflict,
    }
}

pub fn conference_end_service_error(error: ConferenceEndRejection) -> ServiceProviderError {
    match error {
        ConferenceEndRejection::Unavailable => ServiceProviderError::ConferenceNotFound,
        ConferenceEndRejection::NotModerator => ServiceProviderError::ConferenceAuthorization,
        ConferenceEndRejection::Conflict => ServiceProviderError::ConferenceConflict,
    }
}
