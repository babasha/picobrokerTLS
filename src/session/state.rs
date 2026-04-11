use crate::config::GATOMQTT_CONFIG;
use super::rate_limit::TokenBucket;
use embassy_time::{Duration, Instant};
use heapless::{String, Vec};
use mqttrs::QoS;

pub const MAX_OUTBOUND_FRAME_SIZE: usize = 192;
pub const MAX_OUTBOX_DEPTH: usize = 2;

pub type SessionId = usize;
pub type ClientId = String<64>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionState<const MAX_SUBS: usize, const MAX_INFLIGHT: usize> {
    pub client_id: ClientId,
    pub subscriptions: Vec<Subscription, MAX_SUBS>,
    pub inflight: Vec<InflightEntry, MAX_INFLIGHT>,
    pub outbox: Vec<OutboundPacket, MAX_OUTBOX_DEPTH>,
    pub rate: TokenBucket,
    pub keepalive_secs: u16,
    pub last_activity: Instant,
    pub lwt: Option<LwtMessage>,
    next_packet_id: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub filter: String<128>,
    pub qos: QoS,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InflightEntry {
    pub packet_id: u16,
    pub payload: Vec<u8, 512>,
    pub topic: String<128>,
    pub qos: QoS,
    pub sent_at: Instant,
    pub retries: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LwtMessage {
    pub topic: String<128>,
    pub payload: Vec<u8, 512>,
    pub qos: QoS,
    pub retain: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundPacket {
    pub bytes: Vec<u8, MAX_OUTBOUND_FRAME_SIZE>,
}

impl<const MAX_SUBS: usize, const MAX_INFLIGHT: usize> SessionState<MAX_SUBS, MAX_INFLIGHT> {
    pub fn new(client_id: ClientId, keepalive_secs: u16) -> Self {
        Self {
            client_id,
            subscriptions: Vec::new(),
            inflight: Vec::new(),
            outbox: Vec::new(),
            rate: TokenBucket::new(GATOMQTT_CONFIG.rate_capacity, GATOMQTT_CONFIG.rate_per_sec),
            keepalive_secs,
            last_activity: Instant::from_ticks(0),
            lwt: None,
            next_packet_id: 0,
        }
    }

    pub fn inflight_add(&mut self, entry: InflightEntry) -> Result<(), ()> {
        self.inflight.push(entry).map_err(|_| ())
    }

    pub fn inflight_ack(&mut self, packet_id: u16) -> bool {
        let Some(index) = self
            .inflight
            .iter()
            .position(|entry| entry.packet_id == packet_id)
        else {
            return false;
        };

        self.inflight.remove(index);
        true
    }

    pub fn inflight_expired<'a>(
        &'a mut self,
        timeout_ms: u32,
    ) -> impl Iterator<Item = &'a mut InflightEntry> {
        self.inflight_expired_at(Instant::now(), timeout_ms)
    }

    pub fn update_activity(&mut self) {
        self.update_activity_at(Instant::now());
    }

    pub fn keepalive_deadline(&self) -> Instant {
        if self.keepalive_secs == 0 {
            return Instant::MAX;
        }

        let timeout_secs = (u64::from(self.keepalive_secs) * 3) / 2;
        self.last_activity
            .checked_add(Duration::from_secs(timeout_secs))
            .unwrap_or(Instant::MAX)
    }

    pub fn is_keepalive_expired(&self, now: Instant) -> bool {
        if self.keepalive_secs == 0 {
            return false;
        }

        now > self.keepalive_deadline()
    }

    pub fn next_packet_id(&mut self) -> u16 {
        self.next_packet_id = self.next_packet_id.wrapping_add(1);
        if self.next_packet_id == 0 {
            self.next_packet_id = 1;
        }
        self.next_packet_id
    }

    fn inflight_expired_at<'a>(
        &'a mut self,
        now: Instant,
        timeout_ms: u32,
    ) -> impl Iterator<Item = &'a mut InflightEntry> {
        let timeout = Duration::from_millis(timeout_ms as u64);

        self.inflight.iter_mut().filter(move |entry| {
            now.checked_duration_since(entry.sent_at)
                .map(|elapsed| elapsed >= timeout)
                .unwrap_or(false)
        })
    }

    fn update_activity_at(&mut self, now: Instant) {
        self.last_activity = now;
    }
}

#[cfg(test)]
mod tests {
    use super::{InflightEntry, SessionState};
    use embassy_time::Instant;
    use heapless::{String, Vec};
    use mqttrs::QoS;

    fn inflight_entry(packet_id: u16, sent_at: Instant) -> InflightEntry {
        InflightEntry {
            packet_id,
            payload: Vec::<u8, 512>::from_slice(b"hello").unwrap(),
            topic: String::<128>::try_from("devices/kitchen/temp").unwrap(),
            qos: QoS::AtLeastOnce,
            sent_at,
            retries: 0,
        }
    }

    #[test]
    fn session_state_new_initializes_empty_fields() {
        let client_id = String::<64>::try_from("mobile-app").unwrap();
        let state = SessionState::<32, 16>::new(client_id.clone(), 60);

        assert_eq!(state.client_id, client_id);
        assert!(state.subscriptions.is_empty());
        assert!(state.inflight.is_empty());
        assert!(state.outbox.is_empty());
        assert_eq!(state.keepalive_secs, 60);
        assert_eq!(state.last_activity, Instant::from_ticks(0));
        assert!(state.lwt.is_none());
    }

    #[test]
    fn update_activity_sets_last_activity() {
        let mut state = SessionState::<32, 16>::new(String::<64>::try_from("mobile-app").unwrap(), 60);

        state.update_activity_at(Instant::from_secs(42));

        assert_eq!(state.last_activity, Instant::from_secs(42));
    }

    #[test]
    fn keepalive_deadline_uses_one_and_a_half_keepalive_window() {
        let mut state = SessionState::<32, 16>::new(String::<64>::try_from("mobile-app").unwrap(), 60);
        state.update_activity_at(Instant::from_secs(100));

        assert_eq!(state.keepalive_deadline(), Instant::from_secs(190));
    }

    #[test]
    fn keepalive_expiration_is_false_before_deadline_and_true_after() {
        let mut state = SessionState::<32, 16>::new(String::<64>::try_from("mobile-app").unwrap(), 60);
        state.update_activity_at(Instant::from_secs(100));

        assert!(!state.is_keepalive_expired(Instant::from_secs(190)));
        assert!(state.is_keepalive_expired(Instant::from_secs(191)));
    }

    #[test]
    fn keepalive_zero_disables_expiration() {
        let mut state = SessionState::<32, 16>::new(String::<64>::try_from("mobile-app").unwrap(), 0);
        state.update_activity_at(Instant::from_secs(100));

        assert_eq!(state.keepalive_deadline(), Instant::MAX);
        assert!(!state.is_keepalive_expired(Instant::from_secs(10_000)));
    }

    #[test]
    fn inflight_add_accepts_up_to_capacity() {
        let mut state = SessionState::<32, 16>::new(String::<64>::try_from("mobile-app").unwrap(), 60);

        for packet_id in 1..=16 {
            assert!(state
                .inflight_add(inflight_entry(packet_id, Instant::from_millis(packet_id as u64)))
                .is_ok());
        }

        assert_eq!(state.inflight.len(), 16);
        assert!(state
            .inflight_add(inflight_entry(17, Instant::from_millis(17)))
            .is_err());
    }

    #[test]
    fn inflight_ack_removes_matching_entry() {
        let mut state = SessionState::<32, 16>::new(String::<64>::try_from("mobile-app").unwrap(), 60);
        state
            .inflight_add(inflight_entry(1, Instant::from_millis(1)))
            .unwrap();
        state
            .inflight_add(inflight_entry(3, Instant::from_millis(3)))
            .unwrap();
        state
            .inflight_add(inflight_entry(5, Instant::from_millis(5)))
            .unwrap();

        assert!(state.inflight_ack(3));
        assert_eq!(state.inflight.len(), 2);
        assert!(state.inflight.iter().all(|entry| entry.packet_id != 3));
    }

    #[test]
    fn inflight_ack_returns_false_for_missing_packet_id() {
        let mut state = SessionState::<32, 16>::new(String::<64>::try_from("mobile-app").unwrap(), 60);
        state
            .inflight_add(inflight_entry(1, Instant::from_millis(1)))
            .unwrap();

        assert!(!state.inflight_ack(999));
        assert_eq!(state.inflight.len(), 1);
    }

    #[test]
    fn inflight_expired_returns_only_entries_older_than_timeout() {
        let mut state = SessionState::<32, 16>::new(String::<64>::try_from("mobile-app").unwrap(), 60);
        state
            .inflight_add(inflight_entry(1, Instant::from_millis(100)))
            .unwrap();
        state
            .inflight_add(inflight_entry(2, Instant::from_millis(180)))
            .unwrap();

        let expired: std::vec::Vec<u16> = state
            .inflight_expired_at(Instant::from_millis(200), 50)
            .map(|entry| entry.packet_id)
            .collect();

        assert_eq!(expired, std::vec![1]);
    }

    #[test]
    fn next_packet_id_wraps_and_skips_zero() {
        let mut state = SessionState::<32, 16>::new(String::<64>::try_from("mobile-app").unwrap(), 60);

        state.next_packet_id = u16::MAX - 1;

        assert_eq!(state.next_packet_id(), u16::MAX);
        assert_eq!(state.next_packet_id(), 1);
    }
}
