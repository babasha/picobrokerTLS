use crate::codec::frame::{write_packet, WriteError};
use crate::router::RetainedStore;
use crate::session::registry::SessionRegistry;
use crate::session::state::{SessionId, SessionState, Subscription};
use crate::transport::Transport;
use heapless::{String, Vec};
use heapless07::Vec as MqttrsVec;
use mqttrs::{
    Packet, Pid, Publish, QosPid, QoS, Suback, Subscribe, SubscribeReturnCodes, Unsubscribe,
};

#[derive(Debug, PartialEq)]
pub enum HandlerError<E> {
    SessionNotFound(SessionId),
    Write(WriteError<E>),
}

pub type SubscribeError<E> = HandlerError<E>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedDelivery {
    pub topic: String<128>,
    pub payload: Vec<u8, 512>,
    pub qos: QoS,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedSubscribe {
    pub pid: Pid,
    pub return_codes: MqttrsVec<SubscribeReturnCodes, 5>,
    pub retained_deliveries: Vec<RetainedDelivery, 16>,
}

impl<E> From<WriteError<E>> for HandlerError<E> {
    fn from(value: WriteError<E>) -> Self {
        Self::Write(value)
    }
}

pub async fn handle_subscribe<
    T: Transport,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
    const MAX_RETAINED: usize,
    const MAX_PACKET_SIZE: usize,
>(
    transport: &mut T,
    session: &mut SessionState<MAX_SUBS, MAX_INFLIGHT>,
    packet: &Subscribe,
    retained: &RetainedStore<MAX_RETAINED>,
    frame_buf: &mut [u8; MAX_PACKET_SIZE],
) -> Result<(), HandlerError<T::Error>> {
    let prepared = prepare_subscribe(session, packet, retained);
    write_prepared_subscribe(transport, &prepared, frame_buf).await
}

pub async fn handle_subscribe_for_session_id<
    T: Transport,
    const MAX_SESSIONS: usize,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
    const MAX_RETAINED: usize,
    const MAX_PACKET_SIZE: usize,
>(
    transport: &mut T,
    registry: &mut SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>,
    session_id: SessionId,
    packet: &Subscribe,
    retained: &RetainedStore<MAX_RETAINED>,
    frame_buf: &mut [u8; MAX_PACKET_SIZE],
) -> Result<(), HandlerError<T::Error>> {
    let session = registry
        .get_mut(session_id)
        .ok_or(HandlerError::SessionNotFound(session_id))?;

    handle_subscribe(transport, session, packet, retained, frame_buf).await
}

pub fn prepare_subscribe<
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
    const MAX_RETAINED: usize,
>(
    session: &mut SessionState<MAX_SUBS, MAX_INFLIGHT>,
    packet: &Subscribe,
    retained: &RetainedStore<MAX_RETAINED>,
) -> PreparedSubscribe {
    let mut return_codes = MqttrsVec::<SubscribeReturnCodes, 5>::new();
    let mut new_filters = Vec::<String<128>, 5>::new();

    for topic in &packet.topics {
        let (code, inserted) =
            upsert_subscription(&mut session.subscriptions, topic.topic_path.as_str(), topic.qos);
        let _ = return_codes.push(code);

        if inserted && matches!(code, SubscribeReturnCodes::Success(_)) {
            let _ = new_filters.push(String::<128>::try_from(topic.topic_path.as_str()).unwrap());
        }
    }

    let mut retained_deliveries = Vec::<RetainedDelivery, 16>::new();
    let mut next_pid = Pid::new();
    for filter in &new_filters {
        for retained_message in retained.matching(filter.as_str()) {
            let qos = retained_message.qos;
            let _ = next_pid;
            let Ok(topic) = String::<128>::try_from(retained_message.topic.as_str()) else {
                continue;
            };
            let Ok(payload) = Vec::<u8, 512>::from_slice(retained_message.payload.as_slice()) else {
                continue;
            };
            let _ = retained_deliveries.push(RetainedDelivery { topic, payload, qos });
            next_pid = next_pid + 1;
        }
    }

    PreparedSubscribe {
        pid: packet.pid,
        return_codes,
        retained_deliveries,
    }
}

pub async fn write_prepared_subscribe<
    T: Transport,
    const MAX_PACKET_SIZE: usize,
>(
    transport: &mut T,
    prepared: &PreparedSubscribe,
    frame_buf: &mut [u8; MAX_PACKET_SIZE],
) -> Result<(), HandlerError<T::Error>> {
    write_packet(
        transport,
        &Packet::Suback(Suback {
            pid: prepared.pid,
            return_codes: prepared.return_codes.clone(),
        }),
        frame_buf,
    )
    .await?;

    let mut next_pid = Pid::new();
    for retained_message in &prepared.retained_deliveries {
        let qospid = match retained_message.qos {
            QoS::AtMostOnce => QosPid::AtMostOnce,
            QoS::AtLeastOnce => {
                let pid = next_pid;
                next_pid = next_pid + 1;
                QosPid::AtLeastOnce(pid)
            }
            QoS::ExactlyOnce => {
                let pid = next_pid;
                next_pid = next_pid + 1;
                QosPid::ExactlyOnce(pid)
            }
        };

        write_packet(
            transport,
            &Packet::Publish(Publish {
                dup: false,
                qospid,
                retain: true,
                topic_name: retained_message.topic.as_str(),
                payload: retained_message.payload.as_slice(),
            }),
            frame_buf,
        )
        .await?;
    }

    Ok(())
}

pub async fn handle_unsubscribe<
    T: Transport,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
    const MAX_PACKET_SIZE: usize,
>(
    transport: &mut T,
    session: &mut SessionState<MAX_SUBS, MAX_INFLIGHT>,
    packet: &Unsubscribe,
    frame_buf: &mut [u8; MAX_PACKET_SIZE],
) -> Result<(), HandlerError<T::Error>> {
    apply_unsubscribe(session, packet);
    write_packet(transport, &Packet::Unsuback(packet.pid), frame_buf).await?;
    Ok(())
}

pub async fn handle_unsubscribe_for_session_id<
    T: Transport,
    const MAX_SESSIONS: usize,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
    const MAX_PACKET_SIZE: usize,
>(
    transport: &mut T,
    registry: &mut SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>,
    session_id: SessionId,
    packet: &Unsubscribe,
    frame_buf: &mut [u8; MAX_PACKET_SIZE],
) -> Result<(), HandlerError<T::Error>> {
    let session = registry
        .get_mut(session_id)
        .ok_or(HandlerError::SessionNotFound(session_id))?;

    handle_unsubscribe(transport, session, packet, frame_buf).await
}

pub fn apply_unsubscribe<const MAX_SUBS: usize, const MAX_INFLIGHT: usize>(
    session: &mut SessionState<MAX_SUBS, MAX_INFLIGHT>,
    packet: &Unsubscribe,
) {
    for filter in &packet.topics {
        if let Some(index) = session
            .subscriptions
            .iter()
            .position(|subscription| subscription.filter.as_str() == filter.as_str())
        {
            session.subscriptions.remove(index);
        }
    }
}

pub async fn deliver_retained<
    T: Transport,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
    const MAX_RETAINED: usize,
    const MAX_PACKET_SIZE: usize,
>(
    filter: &str,
    _session: &mut SessionState<MAX_SUBS, MAX_INFLIGHT>,
    retained: &RetainedStore<MAX_RETAINED>,
    transport: &mut T,
    frame_buf: &mut [u8; MAX_PACKET_SIZE],
) -> Result<(), HandlerError<T::Error>> {
    let mut next_pid = Pid::new();

    for retained_message in retained.matching(filter) {
        let qospid = match retained_message.qos {
            QoS::AtMostOnce => QosPid::AtMostOnce,
            QoS::AtLeastOnce => {
                let pid = next_pid;
                next_pid = next_pid + 1;
                QosPid::AtLeastOnce(pid)
            }
            QoS::ExactlyOnce => {
                let pid = next_pid;
                next_pid = next_pid + 1;
                QosPid::ExactlyOnce(pid)
            }
        };

        write_packet(
            transport,
            &Packet::Publish(Publish {
                dup: false,
                qospid,
                retain: true,
                topic_name: retained_message.topic.as_str(),
                payload: retained_message.payload.as_slice(),
            }),
            frame_buf,
        )
        .await?;
    }

    Ok(())
}

fn upsert_subscription<const MAX_SUBS: usize>(
    subscriptions: &mut Vec<Subscription, MAX_SUBS>,
    filter: &str,
    qos: QoS,
) -> (SubscribeReturnCodes, bool) {
    if !is_valid_filter(filter) {
        return (SubscribeReturnCodes::Failure, false);
    }

    if let Some(existing) = subscriptions
        .iter_mut()
        .find(|subscription| subscription.filter.as_str() == filter)
    {
        existing.qos = qos;
        return (SubscribeReturnCodes::Success(qos), false);
    }

    let Ok(filter) = String::<128>::try_from(filter) else {
        return (SubscribeReturnCodes::Failure, false);
    };

    match subscriptions.push(Subscription { filter, qos }) {
        Ok(()) => (SubscribeReturnCodes::Success(qos), true),
        Err(_) => (SubscribeReturnCodes::Failure, false),
    }
}

fn is_valid_filter(filter: &str) -> bool {
    !filter.is_empty() && filter.len() <= 128 && !filter.contains('#')
}

#[cfg(test)]
mod tests {
    use super::{
        handle_subscribe, handle_subscribe_for_session_id, handle_unsubscribe,
        handle_unsubscribe_for_session_id, HandlerError,
    };
    use crate::router::collect_subscribers;
    use crate::router::RetainedStore;
    use crate::session::registry::SessionRegistry;
    use crate::session::state::SessionState;
    use crate::transport::mock::MockTransport;
    use core::convert::TryFrom;
    use core::future::Future;
    use core::pin::{pin, Pin};
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    use heapless::String;
    use heapless07::{String as MqttrsString, Vec as MqttrsVec};
    use mqttrs::{Packet, Pid, Publish, QosPid, QoS, Subscribe, SubscribeTopic, Unsubscribe};
    use std::vec;

    const MAX_SESSIONS: usize = 8;
    const MAX_SUBS: usize = 32;
    const MAX_INFLIGHT: usize = 16;
    const MAX_RETAINED: usize = 8;
    const MAX_PACKET_SIZE: usize = 512;

    fn block_on<F: Future>(future: F) -> F::Output {
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

    fn registry_with_session(
    ) -> (SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>, usize) {
        let mut registry = SessionRegistry::new();
        let session_id = registry
            .insert(SessionState::new(
                String::<64>::try_from("mobile-app").unwrap(),
                60,
            ))
            .unwrap();
        (registry, session_id)
    }

    fn retained_store() -> RetainedStore<MAX_RETAINED> {
        RetainedStore::new()
    }

    fn subscribe_packet(filter: &str, qos: QoS, pid: u16) -> Subscribe {
        let mut topics = MqttrsVec::<SubscribeTopic, 5>::new();
        topics
            .push(SubscribeTopic {
                topic_path: MqttrsString::<256>::try_from(filter).unwrap(),
                qos,
            })
            .unwrap();
        Subscribe {
            pid: Pid::try_from(pid).unwrap(),
            topics,
        }
    }

    fn subscribe_packet_many(items: &[(&str, QoS)], pid: u16) -> Subscribe {
        let mut topics = MqttrsVec::<SubscribeTopic, 5>::new();
        for (filter, qos) in items {
            topics
                .push(SubscribeTopic {
                    topic_path: MqttrsString::<256>::try_from(*filter).unwrap(),
                    qos: *qos,
                })
                .unwrap();
        }
        Subscribe {
            pid: Pid::try_from(pid).unwrap(),
            topics,
        }
    }

    fn unsubscribe_packet(filter: &str, pid: u16) -> Unsubscribe {
        let mut topics = MqttrsVec::<MqttrsString<256>, 5>::new();
        topics.push(MqttrsString::<256>::try_from(filter).unwrap()).unwrap();
        Unsubscribe {
            pid: Pid::try_from(pid).unwrap(),
            topics,
        }
    }

    #[test]
    fn subscribe_stores_exact_filter_and_sends_suback() {
        let (mut registry, session_id) = registry_with_session();
        let packet = subscribe_packet("devices/kitchen/temp", QoS::AtMostOnce, 1);
        let retained = retained_store();
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        block_on(handle_subscribe(
            &mut transport,
            registry.get_mut(session_id).unwrap(),
            &packet,
            &retained,
            &mut frame_buf,
        ))
        .unwrap();

        let state = registry.get(session_id).unwrap();
        assert_eq!(state.subscriptions.len(), 1);
        assert_eq!(state.subscriptions[0].filter.as_str(), "devices/kitchen/temp");
        assert_eq!(state.subscriptions[0].qos, QoS::AtMostOnce);
        assert_eq!(transport.tx_log, vec![vec![0x90, 0x03, 0x00, 0x01, 0x00]]);
    }

    #[test]
    fn subscribe_single_topic_qos1_returns_qos1_suback() {
        let (mut registry, session_id) = registry_with_session();
        let packet = subscribe_packet("devices/+/temp", QoS::AtLeastOnce, 12);
        let retained = retained_store();
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        block_on(handle_subscribe(
            &mut transport,
            registry.get_mut(session_id).unwrap(),
            &packet,
            &retained,
            &mut frame_buf,
        ))
        .unwrap();

        assert_eq!(transport.tx_log, vec![vec![0x90, 0x03, 0x00, 0x0C, 0x01]]);
    }

    #[test]
    fn subscribe_three_topics_returns_three_codes() {
        let (mut registry, session_id) = registry_with_session();
        let retained = retained_store();
        let packet = subscribe_packet_many(
            &[
                ("a/one", QoS::AtMostOnce),
                ("a/two", QoS::AtLeastOnce),
                ("a/three", QoS::ExactlyOnce),
            ],
            1,
        );
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        block_on(handle_subscribe(
            &mut transport,
            registry.get_mut(session_id).unwrap(),
            &packet,
            &retained,
            &mut frame_buf,
        ))
        .unwrap();

        assert_eq!(transport.tx_log, vec![vec![0x90, 0x05, 0x00, 0x01, 0x00, 0x01, 0x02]]);
    }

    #[test]
    fn subscribe_with_plus_filter_is_matched_by_router() {
        let (mut registry, session_id) = registry_with_session();
        let packet = subscribe_packet("devices/+/temp", QoS::AtLeastOnce, 2);
        let retained = retained_store();
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        block_on(handle_subscribe(
            &mut transport,
            registry.get_mut(session_id).unwrap(),
            &packet,
            &retained,
            &mut frame_buf,
        ))
        .unwrap();

        let subscribers =
            collect_subscribers::<MAX_SESSIONS, MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>(
                &registry,
                "devices/kitchen/temp",
            );

        assert_eq!(subscribers.len(), 1);
        assert_eq!(subscribers[0], session_id);
    }

    #[test]
    fn subscribe_over_capacity_returns_failure_code_and_keeps_existing_entries() {
        let (mut registry, session_id) = registry_with_session();
        let retained = retained_store();
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        for idx in 0..MAX_SUBS {
            let filter = std::format!("devices/{idx}/temp");
            block_on(handle_subscribe(
                &mut transport,
                registry.get_mut(session_id).unwrap(),
                &subscribe_packet(&filter, QoS::AtMostOnce, (idx + 1) as u16),
                &retained,
                &mut frame_buf,
            ))
            .unwrap();
        }

        transport.tx_log.clear();
        block_on(handle_subscribe(
            &mut transport,
            registry.get_mut(session_id).unwrap(),
            &subscribe_packet("devices/c/temp", QoS::AtLeastOnce, 3),
            &retained,
            &mut frame_buf,
        ))
        .unwrap();

        let state = registry.get(session_id).unwrap();
        assert_eq!(state.subscriptions.len(), MAX_SUBS);
        assert_eq!(transport.tx_log, vec![vec![0x90, 0x03, 0x00, 0x03, 0x80]]);
    }

    #[test]
    fn duplicate_subscribe_updates_qos_without_duplicate_entries() {
        let (mut registry, session_id) = registry_with_session();
        let first = subscribe_packet("devices/kitchen/temp", QoS::AtMostOnce, 1);
        let second = subscribe_packet("devices/kitchen/temp", QoS::AtLeastOnce, 2);
        let retained = retained_store();
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        block_on(handle_subscribe(
            &mut transport,
            registry.get_mut(session_id).unwrap(),
            &first,
            &retained,
            &mut frame_buf,
        ))
        .unwrap();
        block_on(handle_subscribe(
            &mut transport,
            registry.get_mut(session_id).unwrap(),
            &second,
            &retained,
            &mut frame_buf,
        ))
        .unwrap();

        let state = registry.get(session_id).unwrap();
        assert_eq!(state.subscriptions.len(), 1);
        assert_eq!(state.subscriptions[0].qos, QoS::AtLeastOnce);
    }

    #[test]
    fn invalid_empty_filter_returns_failure_code() {
        let (mut registry, session_id) = registry_with_session();
        let retained = retained_store();
        let packet = subscribe_packet("", QoS::AtMostOnce, 4);
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        block_on(handle_subscribe(
            &mut transport,
            registry.get_mut(session_id).unwrap(),
            &packet,
            &retained,
            &mut frame_buf,
        ))
        .unwrap();

        assert_eq!(transport.tx_log, vec![vec![0x90, 0x03, 0x00, 0x04, 0x80]]);
    }

    #[test]
    fn invalid_hash_filter_returns_failure_code() {
        let (mut registry, session_id) = registry_with_session();
        let retained = retained_store();
        let packet = subscribe_packet("a/#/b", QoS::AtMostOnce, 5);
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        block_on(handle_subscribe(
            &mut transport,
            registry.get_mut(session_id).unwrap(),
            &packet,
            &retained,
            &mut frame_buf,
        ))
        .unwrap();

        assert_eq!(transport.tx_log, vec![vec![0x90, 0x03, 0x00, 0x05, 0x80]]);
    }

    #[test]
    fn retained_messages_are_delivered_after_subscribe() {
        let (mut registry, session_id) = registry_with_session();
        let mut retained = retained_store();
        retained
            .set("devices/kitchen/temp", b"21.5", QoS::AtMostOnce)
            .unwrap();
        retained
            .set("devices/living/humidity", b"45", QoS::AtMostOnce)
            .unwrap();

        let packet = subscribe_packet("devices/+/temp", QoS::AtMostOnce, 6);
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        block_on(handle_subscribe(
            &mut transport,
            registry.get_mut(session_id).unwrap(),
            &packet,
            &retained,
            &mut frame_buf,
        ))
        .unwrap();

        assert_eq!(transport.tx_log[0], vec![0x90, 0x03, 0x00, 0x06, 0x00]);

        let retained_publish = mqttrs::decode_slice(&transport.tx_log[1]).unwrap().unwrap();
        match retained_publish {
            Packet::Publish(Publish {
                retain,
                topic_name,
                payload,
                qospid: QosPid::AtMostOnce,
                ..
            }) => {
                assert!(retain);
                assert_eq!(topic_name, "devices/kitchen/temp");
                assert_eq!(payload, b"21.5");
            }
            other => panic!("expected retained publish, got {:?}", other),
        }
    }

    #[test]
    fn subscribe_to_missing_session_returns_error() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let packet = subscribe_packet("devices/kitchen/temp", QoS::AtMostOnce, 7);
        let retained = retained_store();
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        let err = block_on(handle_subscribe_for_session_id(
            &mut transport,
            &mut registry,
            99,
            &packet,
            &retained,
            &mut frame_buf,
        ))
        .unwrap_err();

        assert_eq!(err, HandlerError::SessionNotFound(99));
        assert!(transport.tx_log.is_empty());
    }

    #[test]
    fn unsubscribe_removes_filter_and_sends_unsuback() {
        let (mut registry, session_id) = registry_with_session();
        let retained = retained_store();
        let subscribe = subscribe_packet("devices/+/temp", QoS::AtLeastOnce, 2);
        let unsubscribe = unsubscribe_packet("devices/+/temp", 9);
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        block_on(handle_subscribe(
            &mut transport,
            registry.get_mut(session_id).unwrap(),
            &subscribe,
            &retained,
            &mut frame_buf,
        ))
        .unwrap();

        transport.tx_log.clear();

        block_on(handle_unsubscribe(
            &mut transport,
            registry.get_mut(session_id).unwrap(),
            &unsubscribe,
            &mut frame_buf,
        ))
        .unwrap();

        assert!(registry.get(session_id).unwrap().subscriptions.is_empty());
        assert_eq!(transport.tx_log, vec![vec![0xB0, 0x02, 0x00, 0x09]]);
    }

    #[test]
    fn unsubscribe_missing_filter_still_sends_unsuback() {
        let (mut registry, session_id) = registry_with_session();
        let unsubscribe = unsubscribe_packet("devices/missing/temp", 11);
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        block_on(handle_unsubscribe(
            &mut transport,
            registry.get_mut(session_id).unwrap(),
            &unsubscribe,
            &mut frame_buf,
        ))
        .unwrap();

        assert!(registry.get(session_id).unwrap().subscriptions.is_empty());
        assert_eq!(transport.tx_log, vec![vec![0xB0, 0x02, 0x00, 0x0B]]);
    }

    #[test]
    fn unsubscribe_stops_router_matching_for_topic() {
        let (mut registry, session_id) = registry_with_session();
        let retained = retained_store();
        let subscribe = subscribe_packet("devices/+/temp", QoS::AtLeastOnce, 2);
        let unsubscribe = unsubscribe_packet("devices/+/temp", 9);
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        block_on(handle_subscribe(
            &mut transport,
            registry.get_mut(session_id).unwrap(),
            &subscribe,
            &retained,
            &mut frame_buf,
        ))
        .unwrap();

        block_on(handle_unsubscribe_for_session_id(
            &mut transport,
            &mut registry,
            session_id,
            &unsubscribe,
            &mut frame_buf,
        ))
        .unwrap();

        let subscribers =
            collect_subscribers::<MAX_SESSIONS, MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>(
                &registry,
                "devices/kitchen/temp",
            );

        assert!(subscribers.is_empty());
    }
}
