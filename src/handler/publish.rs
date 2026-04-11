use crate::codec::frame::write_packet;
use crate::config::GATOMQTT_CONFIG;
use crate::router::{find_all_subscribers, find_subscribers, topic_matches, RetainedStore};
use crate::session::registry::SessionRegistry;
use crate::session::state::{OutboundPacket, SessionId, SessionState, MAX_OUTBOUND_FRAME_SIZE};
use crate::transport::Transport;
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant};
use heapless::{String, Vec};
use mqttrs::{Pid, Publish, QosPid, QoS};
#[cfg(test)]
use core::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MqttIntent {
    pub topic: String<128>,
    pub payload: Vec<u8, 512>,
    pub qos: QoS,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MqttPublish {
    pub topic: String<128>,
    pub payload: Vec<u8, 512>,
    pub qos: QoS,
    pub retain: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PublishError {
    RetainedTopicTooLong,
    RetainedPayloadTooLarge,
    RetainedFull,
    SubscriberOutboxFull(SessionId),
    RateLimitDisconnect,
    SenderNotFound(SessionId),
    TopicTooLong,
    PayloadTooLarge,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RetryDisconnect<E> {
    MaxRetriesExceeded { packet_id: u16, retries: u8 },
    InvalidPacketId(u16),
    Write(crate::codec::frame::WriteError<E>),
}

pub async fn handle_publish<
    M: RawMutex,
    const MAX_SESSIONS: usize,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
    const MAX_RETAINED: usize,
    const INBOUND_N: usize,
>(
    sender_id: SessionId,
    registry: &mut SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>,
    retained: &mut RetainedStore<MAX_RETAINED>,
    inbound_queue: &Channel<M, MqttIntent, INBOUND_N>,
    packet: &Publish<'_>,
) -> Result<(), PublishError> {
    handle_publish_at(
        sender_id,
        registry,
        retained,
        inbound_queue,
        packet,
        Instant::now(),
        GATOMQTT_CONFIG.max_violations,
    )
    .await
}

async fn handle_publish_at<
    M: RawMutex,
    const MAX_SESSIONS: usize,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
    const MAX_RETAINED: usize,
    const INBOUND_N: usize,
>(
    sender_id: SessionId,
    registry: &mut SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>,
    retained: &mut RetainedStore<MAX_RETAINED>,
    inbound_queue: &Channel<M, MqttIntent, INBOUND_N>,
    packet: &Publish<'_>,
    now: Instant,
    max_violations: u8,
) -> Result<(), PublishError> {
    let incoming_qos = packet.qospid.qos();
    let Some(sender_session) = registry.get_mut(sender_id) else {
        return Err(PublishError::SenderNotFound(sender_id));
    };

    if !sender_session.rate.try_consume(now) {
        if sender_session.rate.violations() >= max_violations {
            return Err(PublishError::RateLimitDisconnect);
        }

        return Ok(());
    }

    if packet.retain {
        retained
            .set(packet.topic_name, packet.payload, incoming_qos)
            .map_err(map_retained_error)?;
    }

    let subscribers = find_subscribers(registry, packet.topic_name, sender_id);
    for (session_id, subscriber_qos) in subscribers {
        let effective_qos = min_qos(incoming_qos, subscriber_qos);
        let bytes = encode_publish(packet.topic_name, packet.payload, effective_qos, packet.retain)?;

        let Some(session) = registry.get_mut(session_id) else {
            continue;
        };

        session
            .outbox
            .push(OutboundPacket { bytes })
            .map_err(|_| PublishError::SubscriberOutboxFull(session_id))?;
    }

    if is_command_topic(packet.topic_name) {
        let intent = MqttIntent {
            topic: String::<128>::try_from(packet.topic_name).map_err(|_| PublishError::TopicTooLong)?,
            payload: Vec::<u8, 512>::from_slice(packet.payload)
                .map_err(|_| PublishError::PayloadTooLarge)?,
            qos: incoming_qos,
        };

        if inbound_queue.try_send(intent).is_err() {
            log_inbound_queue_drop();
        }
    }

    Ok(())
}

pub fn handle_puback<const MAX_SUBS: usize, const MAX_INFLIGHT: usize>(
    session: &mut SessionState<MAX_SUBS, MAX_INFLIGHT>,
    packet_id: u16,
) {
    if !session.inflight_ack(packet_id) {
        log_unknown_puback(packet_id);
    }
}

pub async fn process_inflight_retries<
    T: Transport,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
    const MAX_PACKET_SIZE: usize,
>(
    session: &mut SessionState<MAX_SUBS, MAX_INFLIGHT>,
    transport: &mut T,
    frame_buf: &mut [u8; MAX_PACKET_SIZE],
    retry_ms: u32,
    max_retries: u8,
) -> Result<(), RetryDisconnect<T::Error>> {
    process_inflight_retries_at(
        session,
        transport,
        frame_buf,
        retry_ms,
        max_retries,
        Instant::now(),
    )
    .await
}

pub fn encode_publish_qos0(
    topic: &str,
    payload: &[u8],
    retain: bool,
) -> Result<Vec<u8, MAX_OUTBOUND_FRAME_SIZE>, PublishError> {
    encode_publish(topic, payload, QoS::AtMostOnce, retain)
}

/// Route an outbound publish from the control domain to all matching subscribers.
///
/// Unlike `handle_publish`, this function applies no rate limiting and does not
/// exclude any sender — every session subscribed to the topic receives the message.
pub fn process_outbound_publish<
    const MAX_SESSIONS: usize,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
    const MAX_RETAINED: usize,
>(
    registry: &mut SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>,
    retained: &mut RetainedStore<MAX_RETAINED>,
    publish: &MqttPublish,
) -> Result<(), PublishError> {
    if publish.retain {
        retained
            .set(publish.topic.as_str(), publish.payload.as_slice(), publish.qos)
            .map_err(map_retained_error)?;
    }

    let subscribers = find_all_subscribers(registry, publish.topic.as_str());
    for (session_id, subscriber_qos) in subscribers {
        let effective_qos = min_qos(publish.qos, subscriber_qos);
        let bytes = encode_publish(
            publish.topic.as_str(),
            publish.payload.as_slice(),
            effective_qos,
            publish.retain,
        )?;

        let Some(session) = registry.get_mut(session_id) else {
            continue;
        };

        session
            .outbox
            .push(OutboundPacket { bytes })
            .map_err(|_| PublishError::SubscriberOutboxFull(session_id))?;
    }

    Ok(())
}

fn encode_publish(
    topic: &str,
    payload: &[u8],
    qos: QoS,
    retain: bool,
) -> Result<Vec<u8, MAX_OUTBOUND_FRAME_SIZE>, PublishError> {
    let mut frame = [0u8; MAX_OUTBOUND_FRAME_SIZE];
    let packet = Publish {
        dup: false,
        qospid: match qos {
            QoS::AtMostOnce => QosPid::AtMostOnce,
            QoS::AtLeastOnce => QosPid::AtLeastOnce(mqttrs::Pid::new()),
            QoS::ExactlyOnce => QosPid::ExactlyOnce(mqttrs::Pid::new()),
        },
        retain,
        topic_name: topic,
        payload,
    };

    let len = mqttrs::encode_slice(&mqttrs::Packet::Publish(packet), &mut frame)
        .map_err(|_| PublishError::PayloadTooLarge)?;
    Vec::<u8, MAX_OUTBOUND_FRAME_SIZE>::from_slice(&frame[..len])
        .map_err(|_| PublishError::PayloadTooLarge)
}

fn is_command_topic(topic: &str) -> bool {
    topic_matches("sb/+/device/+/set", topic)
}

fn min_qos(lhs: QoS, rhs: QoS) -> QoS {
    if qos_rank(lhs) <= qos_rank(rhs) {
        lhs
    } else {
        rhs
    }
}

const fn qos_rank(qos: QoS) -> u8 {
    match qos {
        QoS::AtMostOnce => 0,
        QoS::AtLeastOnce => 1,
        QoS::ExactlyOnce => 2,
    }
}

fn map_retained_error(error: crate::router::RetainedError) -> PublishError {
    match error {
        crate::router::RetainedError::TopicTooLong => PublishError::RetainedTopicTooLong,
        crate::router::RetainedError::PayloadTooLarge => PublishError::RetainedPayloadTooLarge,
        crate::router::RetainedError::Full => PublishError::RetainedFull,
    }
}

async fn process_inflight_retries_at<
    T: Transport,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
    const MAX_PACKET_SIZE: usize,
>(
    session: &mut SessionState<MAX_SUBS, MAX_INFLIGHT>,
    transport: &mut T,
    frame_buf: &mut [u8; MAX_PACKET_SIZE],
    retry_ms: u32,
    max_retries: u8,
    now: Instant,
) -> Result<(), RetryDisconnect<T::Error>> {
    let timeout = Duration::from_millis(retry_ms as u64);

    for index in 0..session.inflight.len() {
        let expired = {
            let entry = &session.inflight[index];
            now.checked_duration_since(entry.sent_at)
                .map(|elapsed| elapsed >= timeout)
                .unwrap_or(false)
        };

        if !expired {
            continue;
        }

        let (packet_id, topic, payload, retries) = {
            let entry = &mut session.inflight[index];
            entry.retries = entry.retries.saturating_add(1);
            if entry.retries > max_retries {
                return Err(RetryDisconnect::MaxRetriesExceeded {
                    packet_id: entry.packet_id,
                    retries: entry.retries,
                });
            }

            (
                entry.packet_id,
                entry.topic.clone(),
                entry.payload.clone(),
                entry.retries,
            )
        };

        let pid = Pid::try_from(packet_id).map_err(|_| RetryDisconnect::InvalidPacketId(packet_id))?;
        let packet = mqttrs::Packet::Publish(Publish {
            dup: true,
            qospid: QosPid::AtLeastOnce(pid),
            retain: false,
            topic_name: topic.as_str(),
            payload: payload.as_slice(),
        });

        write_packet(transport, &packet, frame_buf)
            .await
            .map_err(RetryDisconnect::Write)?;

        session.inflight[index].sent_at = now;
        session.inflight[index].retries = retries;
    }

    Ok(())
}

#[cfg(not(test))]
fn log_inbound_queue_drop() {
    defmt::warn!("mqtt inbound queue full; dropping intent");
}

#[cfg(test)]
fn log_inbound_queue_drop() {}

#[cfg(test)]
static UNKNOWN_PUBACK_WARNINGS: AtomicUsize = AtomicUsize::new(0);

#[cfg(not(test))]
fn log_unknown_puback(packet_id: u16) {
    defmt::warn!("mqtt puback for unknown packet_id={=u16}; ignoring", packet_id);
}

#[cfg(test)]
fn log_unknown_puback(_packet_id: u16) {
    UNKNOWN_PUBACK_WARNINGS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::{
        handle_puback, handle_publish, handle_publish_at, process_inflight_retries_at,
        process_outbound_publish, MqttIntent, MqttPublish, RetryDisconnect,
        UNKNOWN_PUBACK_WARNINGS,
    };
    use core::sync::atomic::Ordering;
    use crate::router::RetainedStore;
    use crate::session::rate_limit::TokenBucket;
    use crate::session::registry::SessionRegistry;
    use crate::session::state::{InflightEntry, SessionState, Subscription};
    use crate::transport::mock::MockTransport;
    use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
    use embassy_sync::channel::Channel;
    use embassy_time::Instant;
    use heapless::Vec;
    use heapless::String;
    use mqttrs::{Packet, Publish, QosPid, QoS};

    const MAX_SESSIONS: usize = 8;
    const MAX_SUBS: usize = 8;
    const MAX_INFLIGHT: usize = 4;
    const MAX_RETAINED: usize = 64;
    const MAX_PACKET_SIZE: usize = 512;

    fn session(client_id: &str, subs: &[(&str, QoS)]) -> SessionState<MAX_SUBS, MAX_INFLIGHT> {
        let mut session = SessionState::new(String::<64>::try_from(client_id).unwrap(), 60);
        for (filter, qos) in subs {
            session
                .subscriptions
                .push(Subscription {
                    filter: String::<128>::try_from(*filter).unwrap(),
                    qos: *qos,
                })
                .unwrap();
        }
        session
    }

    fn inflight_entry(packet_id: u16, sent_at: Instant, retries: u8) -> InflightEntry {
        InflightEntry {
            packet_id,
            payload: Vec::<u8, 512>::from_slice(b"hello").unwrap(),
            topic: String::<128>::try_from("devices/kitchen/temp").unwrap(),
            qos: QoS::AtLeastOnce,
            sent_at,
            retries,
        }
    }

    fn publish<'a>(topic: &'a str, payload: &'a [u8], retain: bool) -> Publish<'a> {
        Publish {
            dup: false,
            qospid: QosPid::AtMostOnce,
            retain,
            topic_name: topic,
            payload,
        }
    }

    fn publish_qos1<'a>(topic: &'a str, payload: &'a [u8]) -> Publish<'a> {
        Publish {
            dup: false,
            qospid: QosPid::AtLeastOnce(mqttrs::Pid::try_from(1).unwrap()),
            retain: false,
            topic_name: topic,
            payload,
        }
    }

    fn decode_publish(bytes: &[u8]) -> Publish<'_> {
        match mqttrs::decode_slice(bytes).unwrap().unwrap() {
            Packet::Publish(publish) => publish,
            other => panic!("expected PUBLISH, got {:?}", other),
        }
    }

    fn reset_unknown_puback_warnings() {
        UNKNOWN_PUBACK_WARNINGS.store(0, Ordering::Relaxed);
    }

    #[test]
    fn publish_routes_to_matching_subscriber() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let sender_id = registry.insert(session("a", &[])).unwrap();
        let receiver_id = registry
            .insert(session("b", &[("test/topic", QoS::AtMostOnce)]))
            .unwrap();
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, 8>::new();

        futures_lite();
        pollster_block_on(async {
            handle_publish(
                sender_id,
                &mut registry,
                &mut retained,
                &inbound,
                &publish("test/topic", b"hello", false),
            )
            .await
        })
        .unwrap();

        let bytes = &registry.get(receiver_id).unwrap().outbox[0].bytes;
        let routed = decode_publish(bytes.as_slice());
        assert_eq!(routed.topic_name, "test/topic");
        assert_eq!(routed.payload, b"hello");
        assert_eq!(registry.get(sender_id).unwrap().rate.violations(), 0);
    }

    #[test]
    fn sender_does_not_receive_own_publish() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let sender_id = registry
            .insert(session("a", &[("test/topic", QoS::AtMostOnce)]))
            .unwrap();
        let receiver_id = registry
            .insert(session("b", &[("test/topic", QoS::AtMostOnce)]))
            .unwrap();
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, 8>::new();

        pollster_block_on(async {
            handle_publish(
                sender_id,
                &mut registry,
                &mut retained,
                &inbound,
                &publish("test/topic", b"hello", false),
            )
            .await
        })
        .unwrap();

        assert!(registry.get(sender_id).unwrap().outbox.is_empty());
        assert_eq!(registry.get(receiver_id).unwrap().outbox.len(), 1);
    }

    #[test]
    fn publish_with_no_subscribers_is_ok() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let sender_id = registry.insert(session("a", &[])).unwrap();
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, 8>::new();

        pollster_block_on(async {
            handle_publish(
                sender_id,
                &mut registry,
                &mut retained,
                &inbound,
                &publish("test/topic", b"hello", false),
            )
            .await
        })
        .unwrap();

        assert!(registry.get(sender_id).unwrap().outbox.is_empty());
    }

    #[test]
    fn retain_true_sets_and_updates_retained_store() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let sender_id = registry.insert(session("a", &[])).unwrap();
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, 8>::new();

        pollster_block_on(async {
            handle_publish(
                sender_id,
                &mut registry,
                &mut retained,
                &inbound,
                &publish("test/topic", b"one", true),
            )
            .await
        })
        .unwrap();
        pollster_block_on(async {
            handle_publish(
                sender_id,
                &mut registry,
                &mut retained,
                &inbound,
                &publish("test/topic", b"two", true),
            )
            .await
        })
        .unwrap();

        let matches: std::vec::Vec<_> = retained.matching("test/topic").collect();
        assert_eq!(retained.len(), 1);
        assert_eq!(matches[0].payload.as_slice(), b"two");
    }

    #[test]
    fn retain_true_empty_payload_deletes_retained_entry() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let sender_id = registry.insert(session("a", &[])).unwrap();
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, 8>::new();

        retained.set("test/topic", b"one", QoS::AtMostOnce).unwrap();

        pollster_block_on(async {
            handle_publish(
                sender_id,
                &mut registry,
                &mut retained,
                &inbound,
                &publish("test/topic", b"", true),
            )
            .await
        })
        .unwrap();

        assert_eq!(retained.len(), 0);
    }

    #[test]
    fn retain_false_does_not_modify_retained_store() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let sender_id = registry.insert(session("a", &[])).unwrap();
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, 8>::new();

        pollster_block_on(async {
            handle_publish(
                sender_id,
                &mut registry,
                &mut retained,
                &inbound,
                &publish("test/topic", b"one", false),
            )
            .await
        })
        .unwrap();

        assert_eq!(retained.len(), 0);
    }

    #[test]
    fn command_topic_is_sent_to_inbound_queue() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let sender_id = registry.insert(session("a", &[])).unwrap();
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, 8>::new();

        pollster_block_on(async {
            handle_publish(
                sender_id,
                &mut registry,
                &mut retained,
                &inbound,
                &publish("sb/house1/device/relay-1/set", b"on", false),
            )
            .await
        })
        .unwrap();

        let intent = inbound.try_receive().unwrap();
        assert_eq!(intent.topic.as_str(), "sb/house1/device/relay-1/set");
        assert_eq!(intent.payload.as_slice(), b"on");
        assert_eq!(intent.qos, QoS::AtMostOnce);
    }

    #[test]
    fn state_topic_does_not_reach_inbound_queue() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let sender_id = registry.insert(session("a", &[])).unwrap();
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, 8>::new();

        pollster_block_on(async {
            handle_publish(
                sender_id,
                &mut registry,
                &mut retained,
                &inbound,
                &publish("sb/house1/device/relay-1/state", b"on", false),
            )
            .await
        })
        .unwrap();

        assert!(inbound.try_receive().is_err());
    }

    #[test]
    fn full_inbound_queue_drops_intent_without_panic() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let sender_id = registry.insert(session("a", &[])).unwrap();
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, 1>::new();
        inbound
            .try_send(MqttIntent {
                topic: String::<128>::try_from("sb/house1/device/relay-1/set").unwrap(),
                payload: Vec::<u8, 512>::from_slice(b"existing").unwrap(),
                qos: QoS::AtMostOnce,
            })
            .unwrap();

        pollster_block_on(async {
            handle_publish(
                sender_id,
                &mut registry,
                &mut retained,
                &inbound,
                &publish("sb/house1/device/relay-1/set", b"on", false),
            )
            .await
        })
        .unwrap();

        let existing = inbound.try_receive().unwrap();
        assert_eq!(existing.payload.as_slice(), b"existing");
        assert!(inbound.try_receive().is_err());
    }

    #[test]
    fn rate_limiter_drops_publish_without_disconnect_before_threshold() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let sender_id = registry.insert(session("a", &[])).unwrap();
        let receiver_id = registry
            .insert(session("b", &[("test/topic", QoS::AtMostOnce)]))
            .unwrap();
        registry.get_mut(sender_id).unwrap().rate = TokenBucket::new(0, 0);
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, 8>::new();

        pollster_block_on(async {
            handle_publish_at(
                sender_id,
                &mut registry,
                &mut retained,
                &inbound,
                &publish("test/topic", b"hello", false),
                Instant::from_secs(0),
                50,
            )
            .await
        })
        .unwrap();

        assert!(registry.get(receiver_id).unwrap().outbox.is_empty());
        assert_eq!(registry.get(sender_id).unwrap().rate.violations(), 1);
    }

    #[test]
    fn repeated_rate_limited_publishes_request_disconnect_at_max_violations() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let sender_id = registry.insert(session("a", &[])).unwrap();
        let receiver_id = registry
            .insert(session("b", &[("test/topic", QoS::AtMostOnce)]))
            .unwrap();
        registry.get_mut(sender_id).unwrap().rate = TokenBucket::new(0, 0);
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, 8>::new();

        for _ in 0..49 {
            pollster_block_on(async {
                handle_publish_at(
                    sender_id,
                    &mut registry,
                    &mut retained,
                    &inbound,
                    &publish("test/topic", b"hello", false),
                    Instant::from_secs(0),
                    50,
                )
                .await
            })
            .unwrap();
        }

        let err = pollster_block_on(async {
            handle_publish_at(
                sender_id,
                &mut registry,
                &mut retained,
                &inbound,
                &publish("test/topic", b"hello", false),
                Instant::from_secs(0),
                50,
            )
            .await
        })
        .unwrap_err();

        assert_eq!(err, super::PublishError::RateLimitDisconnect);
        assert!(registry.get(receiver_id).unwrap().outbox.is_empty());
        assert_eq!(registry.get(sender_id).unwrap().rate.violations(), 50);
    }

    #[test]
    fn qos1_publish_is_dropped_without_retry_ack_when_rate_limited() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let sender_id = registry.insert(session("a", &[])).unwrap();
        let receiver_id = registry
            .insert(session("b", &[("test/topic", QoS::AtLeastOnce)]))
            .unwrap();
        registry.get_mut(sender_id).unwrap().rate = TokenBucket::new(0, 0);
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, 8>::new();

        pollster_block_on(async {
            handle_publish_at(
                sender_id,
                &mut registry,
                &mut retained,
                &inbound,
                &publish_qos1("test/topic", b"hello"),
                Instant::from_secs(0),
                50,
            )
            .await
        })
        .unwrap();

        assert!(registry.get(receiver_id).unwrap().outbox.is_empty());
        assert_eq!(registry.get(sender_id).unwrap().rate.violations(), 1);
    }

    #[test]
    fn puback_for_existing_packet_id_removes_inflight_entry() {
        let mut session = session("a", &[]);
        session
            .inflight_add(inflight_entry(3, Instant::from_millis(100), 0))
            .unwrap();
        session
            .inflight_add(inflight_entry(5, Instant::from_millis(110), 0))
            .unwrap();

        handle_puback(&mut session, 3);

        assert_eq!(session.inflight.len(), 1);
        assert_eq!(session.inflight[0].packet_id, 5);
    }

    #[test]
    fn puback_for_missing_packet_id_logs_warning_without_panicking() {
        let mut session = session("a", &[]);
        reset_unknown_puback_warnings();

        handle_puback(&mut session, 999);

        assert_eq!(UNKNOWN_PUBACK_WARNINGS.load(Ordering::Relaxed), 1);
        assert!(session.inflight.is_empty());
    }

    #[test]
    fn expired_qos1_entry_is_retried_with_dup_bit_set() {
        let mut session = session("a", &[]);
        session
            .inflight_add(inflight_entry(7, Instant::from_millis(100), 0))
            .unwrap();
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        pollster_block_on(async {
            process_inflight_retries_at(
                &mut session,
                &mut transport,
                &mut frame_buf,
                50,
                3,
                Instant::from_millis(200),
            )
            .await
        })
        .unwrap();

        assert_eq!(session.inflight[0].retries, 1);
        assert_eq!(session.inflight[0].sent_at, Instant::from_millis(200));
        assert_eq!(transport.tx_log.len(), 1);

        let retried = decode_publish(&transport.tx_log[0]);
        assert!(retried.dup);
        assert_eq!(retried.qospid.pid().unwrap().get(), 7);
        assert_eq!(retried.payload, b"hello");
    }

    #[test]
    fn expired_entry_past_max_retries_requests_disconnect() {
        let mut session = session("a", &[]);
        session
            .inflight_add(inflight_entry(7, Instant::from_millis(100), 3))
            .unwrap();
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        let err = pollster_block_on(async {
            process_inflight_retries_at(
                &mut session,
                &mut transport,
                &mut frame_buf,
                50,
                3,
                Instant::from_millis(200),
            )
            .await
        })
        .unwrap_err();

        assert_eq!(
            err,
            RetryDisconnect::MaxRetriesExceeded {
                packet_id: 7,
                retries: 4,
            }
        );
        assert!(transport.tx_log.is_empty());
    }

    #[test]
    fn non_expired_entry_is_not_retried() {
        let mut session = session("a", &[]);
        session
            .inflight_add(inflight_entry(7, Instant::from_millis(180), 0))
            .unwrap();
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        pollster_block_on(async {
            process_inflight_retries_at(
                &mut session,
                &mut transport,
                &mut frame_buf,
                50,
                3,
                Instant::from_millis(200),
            )
            .await
        })
        .unwrap();

        assert_eq!(session.inflight[0].retries, 0);
        assert!(transport.tx_log.is_empty());
    }

    #[test]
    fn outbound_publish_routes_to_all_matching_subscribers() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let sub_a = registry
            .insert(session("a", &[("devices/+/temp", QoS::AtMostOnce)]))
            .unwrap();
        let sub_b = registry
            .insert(session("b", &[("devices/kitchen/temp", QoS::AtLeastOnce)]))
            .unwrap();
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let publish = MqttPublish {
            topic: String::<128>::try_from("devices/kitchen/temp").unwrap(),
            payload: Vec::<u8, 512>::from_slice(b"22").unwrap(),
            qos: QoS::AtMostOnce,
            retain: false,
        };

        process_outbound_publish(&mut registry, &mut retained, &publish).unwrap();

        assert_eq!(registry.get(sub_a).unwrap().outbox.len(), 1);
        assert_eq!(registry.get(sub_b).unwrap().outbox.len(), 1);
    }

    #[test]
    fn outbound_publish_includes_all_sessions_no_sender_exclusion() {
        // A single session subscribed to the topic: with handle_publish it would be
        // excluded as the sender, but process_outbound_publish has no sender concept.
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let only_id = registry
            .insert(session("a", &[("test/topic", QoS::AtMostOnce)]))
            .unwrap();
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let publish = MqttPublish {
            topic: String::<128>::try_from("test/topic").unwrap(),
            payload: Vec::<u8, 512>::from_slice(b"payload").unwrap(),
            qos: QoS::AtMostOnce,
            retain: false,
        };

        process_outbound_publish(&mut registry, &mut retained, &publish).unwrap();

        assert_eq!(registry.get(only_id).unwrap().outbox.len(), 1);
    }

    #[test]
    fn outbound_publish_with_retain_updates_retained_store() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let publish = MqttPublish {
            topic: String::<128>::try_from("devices/kitchen/temp").unwrap(),
            payload: Vec::<u8, 512>::from_slice(b"22").unwrap(),
            qos: QoS::AtMostOnce,
            retain: true,
        };

        process_outbound_publish(&mut registry, &mut retained, &publish).unwrap();

        assert_eq!(retained.len(), 1);
        let matches: std::vec::Vec<_> = retained.matching("devices/kitchen/temp").collect();
        assert_eq!(matches[0].payload.as_slice(), b"22");
    }

    #[test]
    fn outbound_publish_with_no_subscribers_is_ok() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let publish = MqttPublish {
            topic: String::<128>::try_from("devices/kitchen/temp").unwrap(),
            payload: Vec::<u8, 512>::from_slice(b"22").unwrap(),
            qos: QoS::AtMostOnce,
            retain: false,
        };

        assert!(process_outbound_publish(&mut registry, &mut retained, &publish).is_ok());
    }

    fn pollster_block_on<F: core::future::Future>(future: F) -> F::Output {
        use core::pin::{pin, Pin};
        use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        fn raw_waker() -> RawWaker {
            fn clone(_: *const ()) -> RawWaker {
                raw_waker()
            }
            fn wake(_: *const ()) {}
            fn wake_by_ref(_: *const ()) {}
            fn drop(_: *const ()) {}

            RawWaker::new(
                core::ptr::null(),
                &RawWakerVTable::new(clone, wake, wake_by_ref, drop),
            )
        }

        let waker = unsafe { Waker::from_raw(raw_waker()) };
        let mut future = pin!(future);
        let mut cx = Context::from_waker(&waker);

        match Pin::as_mut(&mut future).poll(&mut cx) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("test future unexpectedly returned Pending"),
        }
    }

    fn futures_lite() {}
}
