use super::state::{LwtMessage, SessionId, SessionState, StoredPublishHandle};
use heapless::{Deque, String, Vec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryError {
    Full,
    LwtQueueFull,
    PublishStoreFull,
    PublishTopicTooLong,
    PublishPayloadTooLarge,
    PublishRefcountOverflow,
    PublishHandleInvalid,
}

#[derive(Debug, Clone)]
pub struct SessionRegistry<const N: usize, const MAX_SUBS: usize, const MAX_INFLIGHT: usize> {
    slots: [Option<SessionState<MAX_SUBS, MAX_INFLIGHT>>; N],
    published_lwts: Deque<LwtMessage, N>,
    stored_publishes: [Option<StoredPublish>; MAX_INFLIGHT],
    next_publish_generation: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPublish {
    pub topic: String<128>,
    pub payload: Vec<u8, 512>,
    pub retain: bool,
    generation: u32,
    refcount: u16,
}

impl<const N: usize, const MAX_SUBS: usize, const MAX_INFLIGHT: usize> PartialEq
    for SessionRegistry<N, MAX_SUBS, MAX_INFLIGHT>
{
    fn eq(&self, other: &Self) -> bool {
        self.slots == other.slots
            && self.published_lwts.iter().eq(other.published_lwts.iter())
            && self.stored_publishes == other.stored_publishes
            && self.next_publish_generation == other.next_publish_generation
    }
}

impl<const N: usize, const MAX_SUBS: usize, const MAX_INFLIGHT: usize> Eq
    for SessionRegistry<N, MAX_SUBS, MAX_INFLIGHT>
{
}

impl<const N: usize, const MAX_SUBS: usize, const MAX_INFLIGHT: usize> Default
    for SessionRegistry<N, MAX_SUBS, MAX_INFLIGHT>
{
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize, const MAX_SUBS: usize, const MAX_INFLIGHT: usize>
    SessionRegistry<N, MAX_SUBS, MAX_INFLIGHT>
{
    pub const fn new() -> Self {
        Self {
            slots: [const { None }; N],
            published_lwts: Deque::new(),
            stored_publishes: [const { None }; MAX_INFLIGHT],
            next_publish_generation: 1,
        }
    }

    pub fn insert(
        &mut self,
        state: SessionState<MAX_SUBS, MAX_INFLIGHT>,
    ) -> Result<SessionId, RegistryError> {
        if let Some((id, slot)) = self.slots.iter_mut().enumerate().find(|(_, slot)| slot.is_none()) {
            *slot = Some(state);
            Ok(id)
        } else {
            Err(RegistryError::Full)
        }
    }

    pub fn get(&self, id: SessionId) -> Option<&SessionState<MAX_SUBS, MAX_INFLIGHT>> {
        self.slots.get(id).and_then(Option::as_ref)
    }

    pub fn get_mut(&mut self, id: SessionId) -> Option<&mut SessionState<MAX_SUBS, MAX_INFLIGHT>> {
        self.slots.get_mut(id).and_then(Option::as_mut)
    }

    pub fn remove(&mut self, id: SessionId) -> Option<SessionState<MAX_SUBS, MAX_INFLIGHT>> {
        let removed = self.slots.get_mut(id).and_then(Option::take)?;
        for entry in &removed.inflight {
            self.release_stored_publish(entry.publish);
        }
        Some(removed)
    }

    pub fn find_by_client_id(&self, client_id: &str) -> Option<SessionId> {
        self.iter()
            .find(|(_, state)| state.client_id.as_str() == client_id)
            .map(|(id, _)| id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (SessionId, &SessionState<MAX_SUBS, MAX_INFLIGHT>)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(id, slot)| slot.as_ref().map(|state| (id, state)))
    }

    pub fn iter_mut(
        &mut self,
    ) -> impl Iterator<Item = (SessionId, &mut SessionState<MAX_SUBS, MAX_INFLIGHT>)> {
        self.slots
            .iter_mut()
            .enumerate()
            .filter_map(|(id, slot)| slot.as_mut().map(|state| (id, state)))
    }

    pub fn len(&self) -> usize {
        self.iter().count()
    }

    pub fn is_full(&self) -> bool {
        self.len() == N
    }

    pub fn record_published_lwt(&mut self, lwt: LwtMessage) -> Result<(), RegistryError> {
        self.published_lwts
            .push_back(lwt)
            .map_err(|_| RegistryError::LwtQueueFull)
    }

    pub fn published_lwts(&self) -> &Deque<LwtMessage, N> {
        &self.published_lwts
    }

    pub fn take_published_lwt(&mut self) -> Option<LwtMessage> {
        self.published_lwts.pop_front()
    }

    pub fn store_publish_ref(
        &mut self,
        topic: &str,
        payload: &[u8],
        retain: bool,
    ) -> Result<StoredPublishHandle, RegistryError> {
        let Some((slot, stored)) = self
            .stored_publishes
            .iter_mut()
            .enumerate()
            .find(|(_, stored)| stored.is_none())
        else {
            return Err(RegistryError::PublishStoreFull);
        };

        let generation = self.next_publish_generation;
        self.next_publish_generation = self.next_publish_generation.wrapping_add(1);
        if self.next_publish_generation == 0 {
            self.next_publish_generation = 1;
        }

        *stored = Some(StoredPublish {
            topic: String::<128>::try_from(topic).map_err(|_| RegistryError::PublishTopicTooLong)?,
            payload: Vec::<u8, 512>::from_slice(payload)
                .map_err(|_| RegistryError::PublishPayloadTooLarge)?,
            retain,
            generation,
            refcount: 1,
        });

        Ok(StoredPublishHandle { slot, generation })
    }

    pub fn acquire_stored_publish(
        &mut self,
        handle: StoredPublishHandle,
    ) -> Result<(), RegistryError> {
        let Some(stored) = self.stored_publish_mut(handle) else {
            return Err(RegistryError::PublishHandleInvalid);
        };
        stored.refcount = stored
            .refcount
            .checked_add(1)
            .ok_or(RegistryError::PublishRefcountOverflow)?;
        Ok(())
    }

    pub fn release_stored_publish(&mut self, handle: StoredPublishHandle) {
        let Some(stored) = self.stored_publish_mut(handle) else {
            return;
        };
        stored.refcount = stored.refcount.saturating_sub(1);
        if stored.refcount == 0 {
            self.stored_publishes[handle.slot] = None;
        }
    }

    pub fn stored_publish(&self, handle: StoredPublishHandle) -> Option<&StoredPublish> {
        let stored = self.stored_publishes.get(handle.slot)?.as_ref()?;
        (stored.generation == handle.generation).then_some(stored)
    }

    fn stored_publish_mut(&mut self, handle: StoredPublishHandle) -> Option<&mut StoredPublish> {
        let stored = self.stored_publishes.get_mut(handle.slot)?.as_mut()?;
        (stored.generation == handle.generation).then_some(stored)
    }
}

#[cfg(test)]
mod tests {
    use super::{RegistryError, SessionRegistry};
    use crate::session::state::SessionState;
    use heapless::String;
    use std::format;

    const MAX_SESSIONS: usize = 8;
    const MAX_SUBS: usize = 4;
    const MAX_INFLIGHT: usize = 2;

    fn session(client_id: &str, keepalive_secs: u16) -> SessionState<MAX_SUBS, MAX_INFLIGHT> {
        SessionState::new(String::<64>::try_from(client_id).unwrap(), keepalive_secs)
    }

    #[test]
    fn insert_eight_sessions_sets_len_to_eight() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();

        for idx in 0..MAX_SESSIONS {
            let client_id = format!("client-{idx}");
            registry.insert(session(&client_id, 30)).unwrap();
        }

        assert_eq!(registry.len(), MAX_SESSIONS);
        assert!(registry.is_full());
    }

    #[test]
    fn ninth_insert_returns_full_error() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();

        for idx in 0..MAX_SESSIONS {
            let client_id = format!("client-{idx}");
            registry.insert(session(&client_id, 30)).unwrap();
        }

        assert_eq!(
            registry.insert(session("client-overflow", 30)),
            Err(RegistryError::Full)
        );
    }

    #[test]
    fn remove_then_insert_reuses_same_slot() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let first_id = registry.insert(session("client-a", 30)).unwrap();
        let second_id = registry.insert(session("client-b", 30)).unwrap();

        let removed = registry.remove(first_id).unwrap();
        let reused_id = registry.insert(session("client-c", 30)).unwrap();

        assert_eq!(removed.client_id.as_str(), "client-a");
        assert_eq!(second_id, 1);
        assert_eq!(reused_id, first_id);
    }

    #[test]
    fn find_by_client_id_returns_matching_session_id() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let expected_id = registry.insert(session("mobile-app", 45)).unwrap();
        let _ = registry.insert(session("web-panel", 45)).unwrap();

        assert_eq!(registry.find_by_client_id("mobile-app"), Some(expected_id));
    }

    #[test]
    fn find_by_client_id_returns_none_for_missing_client() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let _ = registry.insert(session("mobile-app", 45)).unwrap();

        assert_eq!(registry.find_by_client_id("missing-client"), None);
    }

    #[test]
    fn duplicate_client_ids_are_allowed() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();

        let first_id = registry.insert(session("mobile-app", 45)).unwrap();
        let second_id = registry.insert(session("mobile-app", 90)).unwrap();

        assert_ne!(first_id, second_id);
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn iter_skips_empty_slots() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let id0 = registry.insert(session("client-0", 30)).unwrap();
        let id1 = registry.insert(session("client-1", 30)).unwrap();
        let id2 = registry.insert(session("client-2", 30)).unwrap();

        let _ = registry.remove(id1);

        let occupied: std::vec::Vec<_> = registry.iter().map(|(id, _)| id).collect();

        assert_eq!(occupied, std::vec![id0, id2]);
    }

    #[test]
    fn remove_nonexistent_id_returns_none() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let _ = registry.insert(session("client-0", 30)).unwrap();

        assert!(registry.remove(MAX_SESSIONS + 1).is_none());
        assert!(registry.remove(4).is_none());
    }

    #[test]
    fn get_mut_persists_changes_in_registry() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let id = registry.insert(session("mobile-app", 30)).unwrap();

        let state = registry.get_mut(id).unwrap();
        state.keepalive_secs = 120;

        assert_eq!(registry.get(id).unwrap().keepalive_secs, 120);
    }
}
