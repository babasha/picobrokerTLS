// Size limits (sessions, subscriptions, inflight, retained, topic, payload) are
// const-generic parameters on SessionRegistry / RetainedStore and the heapless
// types inside SessionState — they can only be set at compile time. See the
// example's MAX_SESSIONS / MAX_SUBS / MAX_INFLIGHT / MAX_RETAINED constants.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrokerConfig {
    pub house_token_username: &'static str,
    pub house_token_password: &'static str,
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
    rate_capacity: 20,
    rate_per_sec: 10,
    max_violations: 50,
    max_outbox_drops: 16,
    qos1_retry_ms: 5_000,
    qos1_max_retries: 3,
};
