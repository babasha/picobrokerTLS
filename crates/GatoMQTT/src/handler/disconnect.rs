use crate::qos::max_qos;
use crate::router::{topic_matches, RetainedError, RetainedStore};
use crate::session::registry::SessionRegistry;
use crate::session::state::{LwtMessage, SessionId, SessionState};
use heapless::Vec;
use mqttrs::QoS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LwtBroadcastError {
    DeliveryFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectError {
    Retained(RetainedError),
    Broadcast(LwtBroadcastError),
}

/// Why a client session ended. Drives LWT publication and observability logging.
///
/// MQTT 3.1.1 rule: LWT fires for every reason *except* `ClientDisconnect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    /// Client sent a clean DISCONNECT packet — no LWT.
    ClientDisconnect,
    /// TCP connection closed (EOF) without a DISCONNECT packet.
    ConnectionClosed,
    /// Transport read or write error.
    TransportError,
    /// Keepalive timer expired with no activity from the client.
    KeepaliveTimeout,
    /// Client exceeded the publish rate limit.
    RateLimitExceeded,
    /// QoS1 command intent reached the handler while it was overloaded.
    CommandHandlerOverloaded,
    /// QoS1 inflight retry limit exhausted — remote is unresponsive.
    RetryLimitExceeded,
    /// Subscriber outbox backpressure crossed the quarantine threshold.
    OutboxQuarantine,
}

impl DisconnectReason {
    /// Returns `true` when the LWT must *not* be published (clean disconnect).
    pub fn is_clean(self) -> bool {
        matches!(self, DisconnectReason::ClientDisconnect)
    }
}

pub trait LwtBroadcaster {
    fn broadcast(
        &self,
        deliveries: &[(SessionId, QoS)],
        lwt: &LwtMessage,
    ) -> Result<(), LwtBroadcastError>;
}

pub async fn publish_lwt_if_needed<
    const MAX_SESSIONS: usize,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
    const MAX_RETAINED: usize,
>(
    session: &SessionState<MAX_SUBS, MAX_INFLIGHT>,
    registry: &SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>,
    retained: &mut RetainedStore<MAX_RETAINED>,
    broadcaster: &dyn LwtBroadcaster,
    reason: DisconnectReason,
) -> Result<(), DisconnectError> {
    if reason.is_clean() {
        return Ok(());
    }

    let Some(lwt) = session.lwt.as_ref() else {
        return Ok(());
    };

    publish_lwt_message_if_needed(
        session.client_id.as_str(),
        lwt,
        registry,
        retained,
        broadcaster,
        reason,
    )
    .await
}

pub async fn publish_lwt_message_if_needed<
    const MAX_SESSIONS: usize,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
    const MAX_RETAINED: usize,
>(
    disconnecting_client_id: &str,
    lwt: &LwtMessage,
    registry: &SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>,
    retained: &mut RetainedStore<MAX_RETAINED>,
    broadcaster: &dyn LwtBroadcaster,
    reason: DisconnectReason,
) -> Result<(), DisconnectError> {
    if reason.is_clean() {
        return Ok(());
    }

    if lwt.retain {
        retained
            .set(lwt.topic.as_str(), lwt.payload.as_slice(), lwt.qos)
            .map_err(DisconnectError::Retained)?;
    }

    let deliveries = find_lwt_subscribers(registry, disconnecting_client_id, lwt);
    if deliveries.is_empty() {
        return Ok(());
    }

    broadcaster
        .broadcast(deliveries.as_slice(), lwt)
        .map_err(DisconnectError::Broadcast)
}

fn find_lwt_subscribers<
    const MAX_SESSIONS: usize,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
>(
    registry: &SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>,
    disconnecting_client_id: &str,
    lwt: &LwtMessage,
) -> Vec<(SessionId, QoS), MAX_SESSIONS> {
    let mut deliveries = Vec::<(SessionId, QoS), MAX_SESSIONS>::new();

    for (session_id, state) in registry.iter() {
        if state.client_id.as_str() == disconnecting_client_id {
            continue;
        }

        let mut matched_qos = None;
        for subscription in &state.subscriptions {
            if topic_matches(subscription.filter.as_str(), lwt.topic.as_str()) {
                matched_qos = Some(match matched_qos {
                    Some(current) => max_qos(current, subscription.qos),
                    None => subscription.qos,
                });
            }
        }

        if let Some(qos) = matched_qos {
            let _ = deliveries.push((session_id, qos));
        }
    }

    deliveries
}

#[cfg(test)]
mod tests {
    use super::{publish_lwt_if_needed, DisconnectError, DisconnectReason, LwtBroadcaster, LwtBroadcastError};
    use crate::router::RetainedStore;
    use crate::session::registry::SessionRegistry;
    use crate::session::state::{LwtMessage, SessionId, SessionState, Subscription};
    use heapless::{String, Vec};
    use mqttrs::QoS;
    use std::cell::RefCell;

    const MAX_SESSIONS: usize = 8;
    const MAX_SUBS: usize = 8;
    const MAX_INFLIGHT: usize = 4;
    const MAX_RETAINED: usize = 64;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct BroadcastRecord {
        deliveries: std::vec::Vec<(SessionId, QoS)>,
        topic: String<128>,
        payload: Vec<u8, 512>,
        qos: QoS,
        retain: bool,
    }

    #[derive(Default)]
    struct RecordingBroadcaster {
        records: RefCell<std::vec::Vec<BroadcastRecord>>,
    }

    impl RecordingBroadcaster {
        fn records(&self) -> std::vec::Vec<BroadcastRecord> {
            self.records.borrow().clone()
        }
    }

    impl LwtBroadcaster for RecordingBroadcaster {
        fn broadcast(
            &self,
            deliveries: &[(SessionId, QoS)],
            lwt: &LwtMessage,
        ) -> Result<(), LwtBroadcastError> {
            self.records.borrow_mut().push(BroadcastRecord {
                deliveries: deliveries.to_vec(),
                topic: lwt.topic.clone(),
                payload: lwt.payload.clone(),
                qos: lwt.qos,
                retain: lwt.retain,
            });
            Ok(())
        }
    }

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

    fn lwt(retain: bool) -> LwtMessage {
        LwtMessage {
            topic: String::<128>::try_from("sb/house1/device/relay-1/state").unwrap(),
            payload: Vec::<u8, 512>::from_slice(b"offline").unwrap(),
            qos: QoS::AtLeastOnce,
            retain,
        }
    }

    fn block_on<F: core::future::Future>(future: F) -> F::Output {
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

    #[test]
    fn clean_disconnect_does_not_publish_lwt() {
        let registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let broadcaster = RecordingBroadcaster::default();
        let mut disconnecting = session("mobile-app", &[]);
        disconnecting.lwt = Some(lwt(false));

        block_on(publish_lwt_if_needed(
            &disconnecting,
            &registry,
            &mut retained,
            &broadcaster,
            DisconnectReason::ClientDisconnect,
        ))
        .unwrap();

        assert!(broadcaster.records().is_empty());
        assert_eq!(retained.len(), 0);
    }

    #[test]
    fn keepalive_timeout_disconnect_publishes_lwt_to_matching_subscribers() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let exact_id = registry
            .insert(session(
                "dashboard",
                &[("sb/house1/device/relay-1/state", QoS::AtMostOnce)],
            ))
            .unwrap();
        let wildcard_id = registry
            .insert(session(
                "mobile",
                &[("sb/+/device/+/state", QoS::AtLeastOnce)],
            ))
            .unwrap();
        let _other_id = registry
            .insert(session("other", &[("sb/+/device/+/set", QoS::ExactlyOnce)]))
            .unwrap();
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let broadcaster = RecordingBroadcaster::default();
        let mut disconnecting = session("sensor-1", &[]);
        disconnecting.lwt = Some(lwt(false));

        block_on(publish_lwt_if_needed(
            &disconnecting,
            &registry,
            &mut retained,
            &broadcaster,
            DisconnectReason::KeepaliveTimeout,
        ))
        .unwrap();

        let records = broadcaster.records();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].deliveries,
            std::vec![(exact_id, QoS::AtMostOnce), (wildcard_id, QoS::AtLeastOnce)]
        );
        assert_eq!(records[0].topic.as_str(), "sb/house1/device/relay-1/state");
        assert_eq!(records[0].payload.as_slice(), b"offline");
    }

    #[test]
    fn disconnect_without_lwt_does_not_publish_anything() {
        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let _ = registry
            .insert(session(
                "dashboard",
                &[("sb/house1/device/relay-1/state", QoS::AtMostOnce)],
            ))
            .unwrap();
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let broadcaster = RecordingBroadcaster::default();
        let disconnecting = session("sensor-1", &[]);

        block_on(publish_lwt_if_needed(
            &disconnecting,
            &registry,
            &mut retained,
            &broadcaster,
            DisconnectReason::ConnectionClosed,
        ))
        .unwrap();

        assert!(broadcaster.records().is_empty());
        assert_eq!(retained.len(), 0);
    }

    #[test]
    fn retained_lwt_updates_retained_store() {
        let registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let broadcaster = RecordingBroadcaster::default();
        let mut disconnecting = session("sensor-1", &[]);
        disconnecting.lwt = Some(lwt(true));

        block_on(publish_lwt_if_needed(
            &disconnecting,
            &registry,
            &mut retained,
            &broadcaster,
            DisconnectReason::ConnectionClosed,
        ))
        .unwrap();

        let retained_entries: std::vec::Vec<_> =
            retained.matching("sb/house1/device/relay-1/state").collect();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained_entries[0].payload.as_slice(), b"offline");
        assert_eq!(retained_entries[0].qos, QoS::AtLeastOnce);
    }

    #[test]
    fn broadcaster_error_is_returned() {
        struct FailingBroadcaster;

        impl LwtBroadcaster for FailingBroadcaster {
            fn broadcast(
                &self,
                _deliveries: &[(SessionId, QoS)],
                _lwt: &LwtMessage,
            ) -> Result<(), LwtBroadcastError> {
                Err(LwtBroadcastError::DeliveryFailed)
            }
        }

        let mut registry = SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        let _ = registry
            .insert(session(
                "dashboard",
                &[("sb/house1/device/relay-1/state", QoS::AtMostOnce)],
            ))
            .unwrap();
        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let mut disconnecting = session("sensor-1", &[]);
        disconnecting.lwt = Some(lwt(false));

        let err = block_on(publish_lwt_if_needed(
            &disconnecting,
            &registry,
            &mut retained,
            &FailingBroadcaster,
            DisconnectReason::ConnectionClosed,
        ))
        .unwrap_err();

        assert_eq!(err, DisconnectError::Broadcast(LwtBroadcastError::DeliveryFailed));
    }

    #[test]
    fn lwt_reaches_more_than_sixteen_matching_sessions() {
        const MANY_SESSIONS: usize = 20;
        let mut registry = SessionRegistry::<MANY_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new();
        for index in 0..MANY_SESSIONS {
            let client_id = std::format!("dashboard-{index}");
            registry
                .insert(session(
                    &client_id,
                    &[("sb/house1/device/relay-1/state", QoS::AtMostOnce)],
                ))
                .unwrap();
        }

        let mut retained = RetainedStore::<MAX_RETAINED>::new();
        let broadcaster = RecordingBroadcaster::default();
        let mut disconnecting = session("sensor-1", &[]);
        disconnecting.lwt = Some(lwt(false));

        block_on(publish_lwt_if_needed(
            &disconnecting,
            &registry,
            &mut retained,
            &broadcaster,
            DisconnectReason::ConnectionClosed,
        ))
        .unwrap();

        let records = broadcaster.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].deliveries.len(), MANY_SESSIONS);
    }
}
