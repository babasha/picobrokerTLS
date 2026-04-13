use crate::router::topic_matches;

// TODO: Topic schema is preliminary and subject to change once the mobile app design is finalised.
//
// Current schema:
//   sb/{house_id}/device/{device_id}/set    — inbound command from client → control domain
//   sb/{house_id}/device/{device_id}/state  — outbound state from control domain → clients (retain=true)
//
// Examples:
//   sb/house1/device/relay-1/set    → {"on": true}
//   sb/house1/device/relay-1/state  → {"on": true}
//   sb/house1/device/temp-1/state   → {"value": 22.5}

const COMMAND_FILTER: &str = "sb/+/device/+/set";
const STATE_FILTER: &str = "sb/+/device/+/state";

/// Returns true if the topic is a device command (client → control domain).
/// These messages are forwarded to the inbound intent queue.
pub fn is_command_topic(topic: &str) -> bool {
    topic_matches(COMMAND_FILTER, topic)
}

/// Returns true if the topic is a device state update (control domain → clients).
/// These messages should be published with retain=true.
pub fn is_state_topic(topic: &str) -> bool {
    topic_matches(STATE_FILTER, topic)
}
