use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigSlotId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActiveSlotError {
    InvalidSlot,
    CandidateAlreadyOpen,
    NoCandidateOpen,
    CandidateVersionNotNewer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveSlotState {
    active_slot: ConfigSlotId,
    active_config_version: u32,
    candidate: Option<(ConfigSlotId, u32)>,
}

impl ActiveSlotState {
    pub const fn new(active_slot: ConfigSlotId, active_config_version: u32) -> Self {
        Self {
            active_slot,
            active_config_version,
            candidate: None,
        }
    }

    pub const fn active_slot(&self) -> ConfigSlotId {
        self.active_slot
    }

    pub const fn active_config_version(&self) -> u32 {
        self.active_config_version
    }

    pub const fn candidate(&self) -> Option<(ConfigSlotId, u32)> {
        self.candidate
    }

    pub fn begin_candidate(
        &mut self,
        slot: ConfigSlotId,
        config_version: u32,
    ) -> Result<(), ActiveSlotError> {
        if slot.0 == 0 {
            return Err(ActiveSlotError::InvalidSlot);
        }
        if self.candidate.is_some() {
            return Err(ActiveSlotError::CandidateAlreadyOpen);
        }
        if config_version <= self.active_config_version {
            return Err(ActiveSlotError::CandidateVersionNotNewer);
        }

        self.candidate = Some((slot, config_version));
        Ok(())
    }

    pub fn commit_candidate(&mut self) -> Result<ConfigSlotId, ActiveSlotError> {
        let Some((slot, version)) = self.candidate.take() else {
            return Err(ActiveSlotError::NoCandidateOpen);
        };

        self.active_slot = slot;
        self.active_config_version = version;
        Ok(slot)
    }

    pub fn discard_candidate(&mut self) -> Result<(), ActiveSlotError> {
        if self.candidate.take().is_none() {
            return Err(ActiveSlotError::NoCandidateOpen);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{ActiveSlotError, ActiveSlotState, ConfigSlotId};

    #[test]
    fn commit_advances_active_pointer() {
        let mut state = ActiveSlotState::new(ConfigSlotId(1), 7);

        state.begin_candidate(ConfigSlotId(2), 8).unwrap();
        assert_eq!(state.commit_candidate(), Ok(ConfigSlotId(2)));

        assert_eq!(state.active_slot(), ConfigSlotId(2));
        assert_eq!(state.active_config_version(), 8);
        assert_eq!(state.candidate(), None);
    }

    #[test]
    fn discard_keeps_active_pointer() {
        let mut state = ActiveSlotState::new(ConfigSlotId(1), 7);

        state.begin_candidate(ConfigSlotId(2), 8).unwrap();
        state.discard_candidate().unwrap();

        assert_eq!(state.active_slot(), ConfigSlotId(1));
        assert_eq!(state.active_config_version(), 7);
    }

    #[test]
    fn stale_candidate_is_rejected() {
        let mut state = ActiveSlotState::new(ConfigSlotId(1), 7);

        assert_eq!(
            state.begin_candidate(ConfigSlotId(2), 7),
            Err(ActiveSlotError::CandidateVersionNotNewer)
        );
    }
}
