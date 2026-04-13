#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrokerConfig {
    pub house_token_username: &'static str,
    pub house_token_password: &'static str,
    pub max_sessions: usize,
    pub max_subscriptions: usize,
    pub max_inflight: usize,
    pub max_retained: usize,
    pub max_topic_len: usize,
    pub max_payload_len: usize,
    pub rate_capacity: u8,
    pub rate_per_sec: u8,
    pub max_violations: u8,
    /// How many consecutive outbox-full drops quarantine a slow subscriber.
    /// Once reached the subscriber's connection loop will disconnect it cleanly.
    pub max_outbox_drops: u8,
    pub qos1_retry_ms: u32,
    pub qos1_max_retries: u8,
}

pub const GATOMQTT_CONFIG: BrokerConfig = BrokerConfig {
    house_token_username: "house",
    house_token_password: "secret",
    max_sessions: 8,
    max_subscriptions: 32,
    max_inflight: 16,
    max_retained: 64,
    max_topic_len: 128,
    max_payload_len: 512,
    rate_capacity: 20,
    rate_per_sec: 10,
    max_violations: 50,
    max_outbox_drops: 16,
    qos1_retry_ms: 5_000,
    qos1_max_retries: 3,
};
