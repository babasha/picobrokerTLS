pub mod rate_limit;
pub mod registry;
pub mod state;

use crate::client::ClientId;
use crate::error::BrokerError;
use crate::protocol::heapless::HeaplessVec;
use crate::protocol::packets::Packet;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    #[default]
    Connecting,
    Connected,
    Disconnected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session<
    const MAX_TOPIC_NAME_LENGTH: usize,
    const MAX_PAYLOAD_SIZE: usize,
    const QUEUE_SIZE: usize,
> {
    pub session_id: u128,

    pub client_id: Option<ClientId>,

    pub state: SessionState,

    pub keep_alive_secs: u16,

    pub last_activity: u128,

    pub rx_queue: HeaplessVec<Option<Packet<MAX_TOPIC_NAME_LENGTH, MAX_PAYLOAD_SIZE>>, QUEUE_SIZE>,

    pub tx_queue: HeaplessVec<Option<Packet<MAX_TOPIC_NAME_LENGTH, MAX_PAYLOAD_SIZE>>, QUEUE_SIZE>,
}

pub struct ExpirationInfo {
    pub session_id: u128,
    pub current_time: u128,
    pub last_activity: u128,
    pub keep_alive_secs: u16,
}

impl ExpirationInfo {
    pub fn is_expired(&self) -> bool {
        // Per MQTT 3.1.1 spec, keep_alive=0 means disable keep-alive mechanism.
        // Session should only be cleaned up when TCP connection closes.
        if self.keep_alive_secs == 0 {
            return false;
        }
        let timeout_secs = (self.keep_alive_secs as u128) * 3 / 2;
        let timeout_nanos = timeout_secs * 1_000_000_000;
        let elapsed = self.current_time.saturating_sub(self.last_activity);
        elapsed > timeout_nanos
    }
}

impl<
        const MAX_TOPIC_NAME_LENGTH: usize,
        const MAX_PAYLOAD_SIZE: usize,
        const QUEUE_SIZE: usize,
    > Session<MAX_TOPIC_NAME_LENGTH, MAX_PAYLOAD_SIZE, QUEUE_SIZE>
{
    pub fn new(session_id: u128, keep_alive_secs: u16, current_time: u128) -> Self {
        Self {
            session_id,
            client_id: None,
            state: SessionState::Connecting,
            keep_alive_secs,
            last_activity: current_time,
            rx_queue: HeaplessVec::new(),
            tx_queue: HeaplessVec::new(),
        }
    }

    pub fn expiration_info(&self, current_time: u128) -> ExpirationInfo {
        ExpirationInfo {
            session_id: self.session_id,
            current_time,
            last_activity: self.last_activity,
            keep_alive_secs: self.keep_alive_secs,
        }
    }

    pub fn update_activity(&mut self, current_time: u128) {
        self.last_activity = current_time;
    }

    pub fn queue_tx_packet(
        &mut self,
        packet: Packet<MAX_TOPIC_NAME_LENGTH, MAX_PAYLOAD_SIZE>,
    ) -> Result<(), BrokerError> {
        self.tx_queue
            .push(Some(packet))
            .map_err(|_| BrokerError::BufferFull)
    }

    pub fn dequeue_tx_packet(&mut self) -> Option<Packet<MAX_TOPIC_NAME_LENGTH, MAX_PAYLOAD_SIZE>> {
        self.tx_queue.dequeue_front().flatten()
    }

    pub fn queue_rx_packet(
        &mut self,
        packet: Packet<MAX_TOPIC_NAME_LENGTH, MAX_PAYLOAD_SIZE>,
    ) -> Result<(), BrokerError> {
        self.rx_queue
            .push(Some(packet))
            .map_err(|_| BrokerError::BufferFull)
    }

    pub fn dequeue_rx_packet(&mut self) -> Option<Packet<MAX_TOPIC_NAME_LENGTH, MAX_PAYLOAD_SIZE>> {
        self.rx_queue.dequeue_front().flatten()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct SessionRegistry<
    const MAX_TOPIC_NAME_LENGTH: usize,
    const MAX_PAYLOAD_SIZE: usize,
    const QUEUE_SIZE: usize,
    const MAX_SESSIONS: usize,
    const MAX_TOPICS: usize,
    const MAX_SUBSCRIBERS_PER_TOPIC: usize,
> {
    sessions: HeaplessVec<
        Option<Session<MAX_TOPIC_NAME_LENGTH, MAX_PAYLOAD_SIZE, QUEUE_SIZE>>,
        MAX_SESSIONS,
    >,
}

impl<
        const MAX_TOPIC_NAME_LENGTH: usize,
        const MAX_PAYLOAD_SIZE: usize,
        const QUEUE_SIZE: usize,
        const MAX_SESSIONS: usize,
        const MAX_TOPICS: usize,
        const MAX_SUBSCRIBERS_PER_TOPIC: usize,
    > Default
    for SessionRegistry<
        MAX_TOPIC_NAME_LENGTH,
        MAX_PAYLOAD_SIZE,
        QUEUE_SIZE,
        MAX_SESSIONS,
        MAX_TOPICS,
        MAX_SUBSCRIBERS_PER_TOPIC,
    >
{
    fn default() -> Self {
        Self {
            sessions: HeaplessVec::new(),
        }
    }
}

impl<
        const MAX_TOPIC_NAME_LENGTH: usize,
        const MAX_PAYLOAD_SIZE: usize,
        const QUEUE_SIZE: usize,
        const MAX_SESSIONS: usize,
        const MAX_TOPICS: usize,
        const MAX_SUBSCRIBERS_PER_TOPIC: usize,
    >
    SessionRegistry<
        MAX_TOPIC_NAME_LENGTH,
        MAX_PAYLOAD_SIZE,
        QUEUE_SIZE,
        MAX_SESSIONS,
        MAX_TOPICS,
        MAX_SUBSCRIBERS_PER_TOPIC,
    >
{
    pub fn get_all_sessions(&self) -> [Option<u128>; MAX_SESSIONS] {
        let mut session_ids = [const { None }; MAX_SESSIONS];
        let mut count = 0usize;

        for sess in (&self.sessions).into_iter().flatten() {
            if count < session_ids.len() {
                session_ids[count] = Some(sess.session_id);
                count += 1;
            }
        }

        session_ids
    }

    pub fn get_expired_sessions(
        &self,
        current_time: u128,
    ) -> [Option<ExpirationInfo>; MAX_SESSIONS] {
        let mut expired_sessions = [const { None }; MAX_SESSIONS];
        let mut count = 0usize;

        for session in (&self.sessions).into_iter().flatten() {
            let expiration_info = session.expiration_info(current_time);
            if expiration_info.is_expired() && count < expired_sessions.len() {
                expired_sessions[count] = Some(expiration_info);
                count += 1;
            }
        }

        expired_sessions
    }

    pub fn get_disconnected_sessions(&self) -> [Option<u128>; MAX_SESSIONS] {
        let mut session_ids = [const { None }; MAX_SESSIONS];
        let mut count = 0usize;

        for session in (&self.sessions).into_iter().flatten() {
            if session.state == SessionState::Disconnected && count < session_ids.len() {
                session_ids[count] = Some(session.session_id);
                count += 1;
            }
        }

        session_ids
    }

    pub fn mark_connected(&mut self, session_id: u128) -> Result<(), BrokerError> {
        if let Some(session) = self.find_session(session_id) {
            session.state = SessionState::Connected;
            Ok(())
        } else {
            Err(BrokerError::SessionNotFound { session_id })
        }
    }

    pub fn mark_disconnected(&mut self, session_id: u128) -> Result<(), BrokerError> {
        if let Some(session) = self.find_session(session_id) {
            session.state = SessionState::Disconnected;
            Ok(())
        } else {
            Err(BrokerError::SessionNotFound { session_id })
        }
    }

    pub fn register_new_session(
        &mut self,
        session_id: u128,
        keep_alive_secs: u16,
        current_time: u128,
    ) -> Result<(), BrokerError> {
        let current = self.sessions.len();
        if current >= MAX_SESSIONS {
            return Err(BrokerError::MaxSessionsReached {
                current,
                max: MAX_SESSIONS,
            });
        }

        if self.find_session(session_id).is_some() {
            return Err(BrokerError::SessionAlreadyExists { session_id });
        }

        let session = Session::new(session_id, keep_alive_secs, current_time);
        self.sessions
            .push(Some(session))
            .map_err(|_| BrokerError::BufferFull)?;
        Ok(())
    }

    pub fn remove_session(&mut self, session_id: u128) -> bool {
        let session_idx = self.sessions.iter().position(|s| {
            s.as_ref()
                .map(|session| session.session_id == session_id)
                .unwrap_or(false)
        });

        if let Some(idx) = session_idx {
            self.sessions.remove(idx);
            true
        } else {
            false
        }
    }

    pub fn update_session_activity(
        &mut self,
        session_id: u128,
        current_time: u128,
    ) -> Result<(), BrokerError> {
        if let Some(session) = self.find_session(session_id) {
            session.update_activity(current_time);
            Ok(())
        } else {
            Err(BrokerError::SessionNotFound { session_id })
        }
    }

    pub fn update_client_id(
        &mut self,
        session_id: u128,
        client_id: ClientId,
    ) -> Result<(), BrokerError> {
        if let Some(session) = self.find_session(session_id) {
            session.client_id = Some(client_id);
            Ok(())
        } else {
            Err(BrokerError::SessionNotFound { session_id })
        }
    }

    pub fn update_keep_alive(
        &mut self,
        session_id: u128,
        keep_alive_secs: u16,
    ) -> Result<(), BrokerError> {
        if let Some(session) = self.find_session(session_id) {
            session.keep_alive_secs = keep_alive_secs;
            Ok(())
        } else {
            Err(BrokerError::SessionNotFound { session_id })
        }
    }

    pub fn queue_rx_packet(
        &mut self,
        session_id: u128,
        packet: Packet<MAX_TOPIC_NAME_LENGTH, MAX_PAYLOAD_SIZE>,
    ) -> Result<(), BrokerError> {
        if let Some(session) = self.find_session(session_id) {
            session
                .queue_rx_packet(packet)
                .map_err(|_| BrokerError::SessionQueueFull {
                    session_id,
                    queue_size: QUEUE_SIZE,
                })
        } else {
            Err(BrokerError::SessionNotFound { session_id })
        }
    }

    pub fn queue_tx_packet(
        &mut self,
        session_id: u128,
        packet: Packet<MAX_TOPIC_NAME_LENGTH, MAX_PAYLOAD_SIZE>,
    ) -> Result<(), BrokerError> {
        if let Some(session) = self.find_session(session_id) {
            session
                .queue_tx_packet(packet)
                .map_err(|_| BrokerError::SessionQueueFull {
                    session_id,
                    queue_size: QUEUE_SIZE,
                })
        } else {
            Err(BrokerError::SessionNotFound { session_id })
        }
    }

    pub fn dequeue_rx_packet(
        &mut self,
        session_id: u128,
    ) -> Result<Option<Packet<MAX_TOPIC_NAME_LENGTH, MAX_PAYLOAD_SIZE>>, BrokerError> {
        if let Some(session) = self.find_session(session_id) {
            Ok(session.dequeue_rx_packet())
        } else {
            Err(BrokerError::SessionNotFound { session_id })
        }
    }

    pub fn dequeue_tx_packet(
        &mut self,
        session_id: u128,
    ) -> Result<Option<Packet<MAX_TOPIC_NAME_LENGTH, MAX_PAYLOAD_SIZE>>, BrokerError> {
        if let Some(session) = self.find_session(session_id) {
            Ok(session.dequeue_tx_packet())
        } else {
            Err(BrokerError::SessionNotFound { session_id })
        }
    }

    pub fn has_pending_tx_packets(&self, session_id: u128) -> bool {
        self.sessions
            .iter()
            .find(|s_opt| {
                s_opt
                    .as_ref()
                    .map(|s| s.session_id == session_id)
                    .unwrap_or(false)
            })
            .and_then(|s_opt| s_opt.as_ref())
            .map(|s| !s.tx_queue.is_empty())
            .unwrap_or(false)
    }

    fn find_session(
        &mut self,
        session_id: u128,
    ) -> Option<&mut Session<MAX_TOPIC_NAME_LENGTH, MAX_PAYLOAD_SIZE, QUEUE_SIZE>> {
        self.sessions
            .iter_mut()
            .find(|s_opt| {
                s_opt
                    .as_ref()
                    .map(|s| s.session_id == session_id)
                    .unwrap_or(false)
            })
            .and_then(|s_opt| s_opt.as_mut())
    }
}
