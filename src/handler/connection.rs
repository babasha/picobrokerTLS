use crate::codec::frame::{read_packet, ReadError};
use crate::config::BrokerConfig;
use crate::handler::connect::{prepare_connect, write_connack, HouseToken, PreparedConnect};
use crate::handler::disconnect::{publish_lwt_if_needed, LwtBroadcaster, LwtBroadcastError};
use crate::handler::pingreq::touch_pingreq;
use crate::handler::publish::{
    handle_puback, handle_publish, process_inflight_retries, MqttIntent, PublishError,
};
use crate::handler::subscribe::{
    apply_unsubscribe, prepare_subscribe, write_prepared_subscribe,
};
use crate::router::RetainedStore;
use crate::session::rate_limit::TokenBucket;
use crate::session::registry::SessionRegistry;
use crate::session::state::{LwtMessage, OutboundPacket, SessionId, MAX_OUTBOUND_FRAME_SIZE};
use crate::transport::Transport;
use core::cell::RefCell;
use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer};
use heapless::Vec;
use mqttrs::{Packet, Publish, QosPid};

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedLwt<const MAX_DELIVERIES: usize> {
    deliveries: Vec<(SessionId, mqttrs::QoS), MAX_DELIVERIES>,
}

struct RecordingLwtBroadcaster<const MAX_DELIVERIES: usize> {
    deliveries: RefCell<Option<RecordedLwt<MAX_DELIVERIES>>>,
}

impl<const MAX_DELIVERIES: usize> RecordingLwtBroadcaster<MAX_DELIVERIES> {
    const fn new() -> Self {
        Self {
            deliveries: RefCell::new(None),
        }
    }

    fn take(self) -> Option<RecordedLwt<MAX_DELIVERIES>> {
        self.deliveries.into_inner()
    }
}

impl<const MAX_DELIVERIES: usize> LwtBroadcaster for RecordingLwtBroadcaster<MAX_DELIVERIES> {
    fn broadcast(
        &self,
        deliveries: &[(SessionId, mqttrs::QoS)],
        _lwt: &LwtMessage,
    ) -> Result<(), LwtBroadcastError> {
        let mut recorded = Vec::new();
        for delivery in deliveries {
            recorded
                .push(*delivery)
                .map_err(|_| LwtBroadcastError::DeliveryFailed)?;
        }

        *self.deliveries.borrow_mut() = Some(RecordedLwt { deliveries: recorded });
        Ok(())
    }
}

pub async fn connection_loop<
    T: Transport,
    M: RawMutex,
    const MAX_SESSIONS: usize,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
    const MAX_RETAINED: usize,
    const INBOUND_N: usize,
    const MAX_PACKET_SIZE: usize,
>(
    transport: &mut T,
    registry: &Mutex<M, SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>>,
    retained: &Mutex<M, RetainedStore<MAX_RETAINED>>,
    inbound_queue: &Channel<M, MqttIntent, INBOUND_N>,
    config: &BrokerConfig,
    frame_buf: &mut [u8; MAX_PACKET_SIZE],
) {
    let mut clean_disconnect = false;
    let mut write_buf = [0u8; MAX_PACKET_SIZE];

    defmt::info!("mqtt connection_loop: waiting for CONNECT");
    let first_packet = match select(
        read_packet(transport, frame_buf),
        Timer::after(Duration::from_secs(10)),
    )
    .await
    {
        Either::First(Ok(packet)) => packet,
        Either::First(Err(_)) | Either::Second(_) => {
            defmt::warn!("mqtt connection_loop: failed before CONNECT");
            transport.close().await;
            return;
        }
    };

    let connect = match first_packet {
        Packet::Connect(connect) => {
            defmt::info!("mqtt connection_loop: CONNECT received");
            connect
        }
        _ => {
            defmt::warn!("mqtt connection_loop: first packet is not CONNECT");
            transport.close().await;
            return;
        }
    };

    let session_id = {
        let mut registry = registry.lock().await;
        let token = HouseToken {
            username: config.house_token_username,
            password: config.house_token_password,
        };

        match prepare_connect(&mut registry, &connect, &token) {
            Ok(PreparedConnect::Accepted(outcome)) => {
                let session_id = outcome.session_id;
                if let Some(session) = registry.get_mut(session_id) {
                    session.rate = TokenBucket::new(config.rate_capacity, config.rate_per_sec);
                }
                drop(registry);

                if write_connack(transport, mqttrs::ConnectReturnCode::Accepted, &mut write_buf)
                    .await
                    .is_err()
                {
                    defmt::warn!("mqtt connection_loop: CONNECT ack failed");
                    transport.close().await;
                    return;
                }

                defmt::info!("mqtt connection_loop: CONNECT accepted");
                session_id
            }
            Ok(PreparedConnect::Rejected(code)) => {
                drop(registry);
                let _ = write_connack(transport, code, &mut write_buf).await;
                transport.close().await;
                defmt::warn!("mqtt connection_loop: CONNECT rejected");
                return;
            }
            Err(_) => {
                defmt::warn!("mqtt connection_loop: CONNECT rejected");
                return;
            }
        }
    };

    loop {
        if flush_outbox_for_session(session_id, registry, transport).await.is_err() {
            break;
        }

        {
            let mut registry = registry.lock().await;
            let Some(session) = registry.get_mut(session_id) else {
                break;
            };

            if process_inflight_retries(
                session,
                transport,
                &mut write_buf,
                config.qos1_retry_ms,
                config.qos1_max_retries,
            )
            .await
            .is_err()
            {
                break;
            }

            if session.is_keepalive_expired(embassy_time::Instant::now()) {
                break;
            }
        }

        let timer_after = {
            let registry = registry.lock().await;
            let Some(session) = registry.get(session_id) else {
                break;
            };
            session.next_wakeup_after(
                embassy_time::Instant::now(),
                config.qos1_retry_ms,
                Duration::from_millis(50),
            )
        };

        match select(
            read_packet(transport, frame_buf),
            Timer::after(timer_after),
        )
        .await
        {
            Either::First(Ok(packet)) => {
                if touch_session_activity(session_id, registry).await.is_err() {
                    break;
                }

                match packet {
                    Packet::Publish(publish) => {
                        let mut registry = registry.lock().await;
                        let mut retained = retained.lock().await;
                        if matches!(
                            handle_publish(
                                session_id,
                            &mut registry,
                            &mut retained,
                            inbound_queue,
                            &publish,
                            )
                            .await,
                            Err(PublishError::RateLimitDisconnect)
                        ) {
                            break;
                        }
                    }
                    Packet::Puback(pid) => {
                        let mut registry = registry.lock().await;
                        if let Some(session) = registry.get_mut(session_id) {
                            handle_puback(session, pid.get());
                        } else {
                            break;
                        }
                    }
                    Packet::Subscribe(subscribe) => {
                        let prepared = {
                            let mut registry = registry.lock().await;
                            let retained = retained.lock().await;
                            let Some(session) = registry.get_mut(session_id) else {
                                break;
                            };
                            prepare_subscribe(session, &subscribe, &retained)
                        };
                        if write_prepared_subscribe(transport, &prepared, &mut write_buf)
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Packet::Unsubscribe(unsubscribe) => {
                        {
                            let mut registry = registry.lock().await;
                            let Some(session) = registry.get_mut(session_id) else {
                                break;
                            };
                            apply_unsubscribe(session, &unsubscribe);
                        }
                        if crate::codec::frame::write_packet(
                            transport,
                            &Packet::Unsuback(unsubscribe.pid),
                            &mut write_buf,
                        )
                        .await
                        .is_err()
                        {
                            break;
                        }
                    }
                    Packet::Pingreq => {
                        {
                            let mut registry = registry.lock().await;
                            let Some(session) = registry.get_mut(session_id) else {
                                break;
                            };
                            touch_pingreq(session);
                        }
                        if crate::codec::frame::write_packet(
                            transport,
                            &Packet::Pingresp,
                            &mut write_buf,
                        )
                        .await
                        .is_err()
                        {
                            break;
                        }
                    }
                    Packet::Disconnect => {
                        defmt::info!("mqtt connection_loop: DISCONNECT received");
                        clean_disconnect = true;
                        break;
                    }
                    _ => {}
                }
            }
            Either::First(Err(ReadError::Eof)) => {
                defmt::warn!("mqtt connection_loop: EOF");
                break;
            }
            Either::First(Err(_)) => {
                defmt::warn!("mqtt connection_loop: read error");
                break;
            }
            Either::Second(_) => continue,
        }
    }

    defmt::info!("mqtt connection_loop: cleanup");
    cleanup_connection(
        session_id,
        clean_disconnect,
        transport,
        registry,
        retained,
    )
    .await;
}

async fn touch_session_activity<
    M: RawMutex,
    const MAX_SESSIONS: usize,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
>(
    session_id: SessionId,
    registry: &Mutex<M, SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>>,
) -> Result<(), ()> {
    let mut registry = registry.lock().await;
    let Some(session) = registry.get_mut(session_id) else {
        return Err(());
    };
    session.update_activity();
    Ok(())
}

async fn flush_outbox_for_session<
    T: Transport,
    M: RawMutex,
    const MAX_SESSIONS: usize,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
>(
    session_id: SessionId,
    registry: &Mutex<M, SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>>,
    transport: &mut T,
) -> Result<(), T::Error> {
    loop {
        let next_packet = {
            let mut registry = registry.lock().await;
            let Some(session) = registry.get_mut(session_id) else {
                return Ok(());
            };

            if session.outbox.is_empty() {
                None
            } else {
                Some(session.outbox.remove(0))
            }
        };

        let Some(packet) = next_packet else {
            return Ok(());
        };

        transport.write(packet.bytes.as_slice()).await?;
    }
}

async fn cleanup_connection<
    T: Transport,
    M: RawMutex,
    const MAX_SESSIONS: usize,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
    const MAX_RETAINED: usize,
>(
    session_id: SessionId,
    clean_disconnect: bool,
    transport: &mut T,
    registry: &Mutex<M, SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>>,
    retained: &Mutex<M, RetainedStore<MAX_RETAINED>>,
) {
    let session_snapshot = {
        let registry = registry.lock().await;
        registry.get(session_id).cloned()
    };

    if let Some(session) = session_snapshot {
        let broadcaster = RecordingLwtBroadcaster::<16>::new();
        {
            let registry_guard = registry.lock().await;
            let mut retained_guard = retained.lock().await;
            let _ = publish_lwt_if_needed(
                &session,
                &registry_guard,
                &mut retained_guard,
                &broadcaster,
                clean_disconnect,
            )
            .await;
        }

        if let Some(recorded) = broadcaster.take() {
            let mut registry = registry.lock().await;
            for (target_session_id, qos) in recorded.deliveries {
                let Some(target) = registry.get_mut(target_session_id) else {
                    continue;
                };
                let bytes = match encode_publish_from_lwt(&session.lwt, qos) {
                    Some(bytes) => bytes,
                    None => continue,
                };
                let _ = target.outbox.push(OutboundPacket { bytes });
            }
        }
    }

    {
        let mut registry = registry.lock().await;
        let _ = registry.remove(session_id);
    }

    transport.close().await;
}

fn encode_publish_from_lwt(
    lwt: &Option<LwtMessage>,
    qos: mqttrs::QoS,
) -> Option<Vec<u8, MAX_OUTBOUND_FRAME_SIZE>> {
    let lwt = lwt.as_ref()?;
    let mut frame = [0u8; MAX_OUTBOUND_FRAME_SIZE];
    let qospid = match qos {
        mqttrs::QoS::AtMostOnce => QosPid::AtMostOnce,
        mqttrs::QoS::AtLeastOnce => QosPid::AtLeastOnce(mqttrs::Pid::new()),
        mqttrs::QoS::ExactlyOnce => QosPid::ExactlyOnce(mqttrs::Pid::new()),
    };
    let packet = Packet::Publish(Publish {
        dup: false,
        qospid,
        retain: lwt.retain,
        topic_name: lwt.topic.as_str(),
        payload: lwt.payload.as_slice(),
    });
    let len = mqttrs::encode_slice(&packet, &mut frame).ok()?;
    Vec::from_slice(&frame[..len]).ok()
}

#[cfg(test)]
mod tests {
    use super::connection_loop;
    use crate::config::BrokerConfig;
    use crate::handler::publish::MqttIntent;
    use crate::router::RetainedStore;
    use crate::session::registry::SessionRegistry;
    use crate::session::state::{InflightEntry, SessionState, Subscription};
    use crate::transport::Transport;
    use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
    use embassy_sync::channel::Channel;
    use embassy_sync::mutex::Mutex;
    use embassy_time::{Duration, Instant};
    use futures::executor::block_on;
    use heapless::{String, Vec};
    use heapless07::{String as MqttrsString, Vec as MqttrsVec};
    use mqttrs::{
        Connect, LastWill, Packet, Pid, Protocol, Publish, QosPid, QoS, Subscribe, SubscribeTopic,
    };
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::task::Waker;
    use std::thread;
    use std::time::Duration as StdDuration;
    use std::vec;
    use std::vec::Vec as StdVec;

    const MAX_SESSIONS: usize = 8;
    const MAX_SUBS: usize = 32;
    const MAX_INFLIGHT: usize = 16;
    const MAX_RETAINED: usize = 64;
    const INBOUND_N: usize = 8;
    const MAX_PACKET_SIZE: usize = 512;

    #[derive(Default)]
    struct TransportState {
        rx_queue: VecDeque<StdVec<u8>>,
        tx_log: StdVec<StdVec<u8>>,
        eof: bool,
        closed: bool,
        waker: Option<Waker>,
    }

    #[derive(Clone, Default)]
    struct AsyncMockTransport {
        state: Arc<StdMutex<TransportState>>,
    }

    impl AsyncMockTransport {
        fn new() -> Self {
            Self::default()
        }

        fn feed(&self, data: &[u8]) {
            let mut state = self.state.lock().unwrap();
            state.rx_queue.push_back(data.to_vec());
            if let Some(waker) = state.waker.take() {
                waker.wake();
            }
        }

        fn finish(&self) {
            let mut state = self.state.lock().unwrap();
            state.eof = true;
            if let Some(waker) = state.waker.take() {
                waker.wake();
            }
        }

        fn tx_log(&self) -> StdVec<StdVec<u8>> {
            self.state.lock().unwrap().tx_log.clone()
        }

        fn is_closed(&self) -> bool {
            self.state.lock().unwrap().closed
        }
    }

    impl Transport for AsyncMockTransport {
        type Error = ();

        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            core::future::poll_fn(|cx| {
                let mut state = self.state.lock().unwrap();
                if let Some(mut chunk) = state.rx_queue.pop_front() {
                    let read_len = core::cmp::min(buf.len(), chunk.len());
                    buf[..read_len].copy_from_slice(&chunk[..read_len]);
                    if read_len < chunk.len() {
                        let rest = chunk.split_off(read_len);
                        state.rx_queue.push_front(rest);
                    }
                    return core::task::Poll::Ready(Ok(read_len));
                }

                if state.eof {
                    return core::task::Poll::Ready(Ok(0));
                }

                state.waker = Some(cx.waker().clone());
                core::task::Poll::Pending
            })
            .await
        }

        async fn write(&mut self, buf: &[u8]) -> Result<(), Self::Error> {
            let mut state = self.state.lock().unwrap();
            state.tx_log.push(buf.to_vec());
            Ok(())
        }

        async fn close(&mut self) {
            let mut state = self.state.lock().unwrap();
            state.closed = true;
            state.eof = true;
            if let Some(waker) = state.waker.take() {
                waker.wake();
            }
        }
    }

    fn encode_packet(packet: &Packet<'_>) -> StdVec<u8> {
        let mut frame = [0u8; MAX_PACKET_SIZE];
        let len = mqttrs::encode_slice(packet, &mut frame).unwrap();
        frame[..len].to_vec()
    }

    fn config() -> BrokerConfig {
        BrokerConfig {
            house_token_username: "house",
            house_token_password: "secret",
            max_sessions: MAX_SESSIONS,
            max_subscriptions: MAX_SUBS,
            max_inflight: MAX_INFLIGHT,
            max_retained: MAX_RETAINED,
            max_topic_len: 128,
            max_payload_len: 512,
            rate_capacity: 20,
            rate_per_sec: 10,
            max_violations: 50,
            qos1_retry_ms: 5_000,
            qos1_max_retries: 3,
        }
    }

    fn connect_packet<'a>(client_id: &'a str, keep_alive: u16) -> Packet<'a> {
        Packet::Connect(Connect {
            protocol: Protocol::MQTT311,
            keep_alive,
            client_id,
            clean_session: true,
            last_will: Some(LastWill {
                topic: "sb/house1/device/relay-1/state",
                message: b"offline",
                qos: QoS::AtLeastOnce,
                retain: true,
            }),
            username: Some("house"),
            password: Some(b"secret"),
        })
    }

    fn subscribe_packet(filter: &str, pid: u16) -> Packet<'_> {
        let mut topics = MqttrsVec::<SubscribeTopic, 5>::new();
        topics
            .push(SubscribeTopic {
                topic_path: MqttrsString::<256>::try_from(filter).unwrap(),
                qos: QoS::AtMostOnce,
            })
            .unwrap();
        Packet::Subscribe(Subscribe {
            pid: Pid::try_from(pid).unwrap(),
            topics,
        })
    }

    fn publish_packet<'a>(topic: &'a str, payload: &'a [u8], qos: QosPid) -> Packet<'a> {
        Packet::Publish(Publish {
            dup: false,
            qospid: qos,
            retain: false,
            topic_name: topic,
            payload,
        })
    }

    fn session_with_subscription(client_id: &str, filter: &str) -> SessionState<MAX_SUBS, MAX_INFLIGHT> {
        let mut session = SessionState::new(String::<64>::try_from(client_id).unwrap(), 60);
        session
            .subscriptions
            .push(Subscription {
                filter: String::<128>::try_from(filter).unwrap(),
                qos: QoS::AtMostOnce,
            })
            .unwrap();
        session
    }

    #[test]
    fn non_connect_first_packet_closes_transport() {
        let mut transport = AsyncMockTransport::new();
        transport.feed(encode_packet(&Packet::Pingreq).as_slice());
        transport.finish();

        let registry = Mutex::<CriticalSectionRawMutex, _>::new(SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new());
        let retained = Mutex::<CriticalSectionRawMutex, _>::new(RetainedStore::<MAX_RETAINED>::new());
        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, INBOUND_N>::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        block_on(connection_loop(
            &mut transport,
            &registry,
            &retained,
            &inbound,
            &config(),
            &mut frame_buf,
        ));

        assert!(transport.is_closed());
        assert_eq!(block_on(registry.lock()).len(), 0);
    }

    #[test]
    fn connect_with_invalid_token_leaves_no_session() {
        let mut bad_connect = connect_packet("mobile-app", 60);
        if let Packet::Connect(ref mut connect) = bad_connect {
            connect.password = Some(b"wrong");
        }

        let mut transport = AsyncMockTransport::new();
        transport.feed(encode_packet(&bad_connect).as_slice());
        transport.finish();

        let registry = Mutex::<CriticalSectionRawMutex, _>::new(SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new());
        let retained = Mutex::<CriticalSectionRawMutex, _>::new(RetainedStore::<MAX_RETAINED>::new());
        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, INBOUND_N>::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        block_on(connection_loop(
            &mut transport,
            &registry,
            &retained,
            &inbound,
            &config(),
            &mut frame_buf,
        ));

        let tx = transport.tx_log();
        assert_eq!(tx[0], vec![0x20, 0x02, 0x00, 0x04]);
        assert_eq!(block_on(registry.lock()).len(), 0);
    }

    #[test]
    fn disconnect_removes_session_from_registry() {
        let mut transport = AsyncMockTransport::new();
        transport.feed(encode_packet(&connect_packet("mobile-app", 60)).as_slice());
        transport.feed(encode_packet(&Packet::Disconnect).as_slice());
        transport.finish();

        let registry = Mutex::<CriticalSectionRawMutex, _>::new(SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new());
        let retained = Mutex::<CriticalSectionRawMutex, _>::new(RetainedStore::<MAX_RETAINED>::new());
        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, INBOUND_N>::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        block_on(connection_loop(
            &mut transport,
            &registry,
            &retained,
            &inbound,
            &config(),
            &mut frame_buf,
        ));

        let registry_guard = block_on(registry.lock());
        assert!(registry_guard.find_by_client_id("mobile-app").is_none());
    }

    #[test]
    fn keepalive_expire_publishes_lwt_and_removes_session() {
        let mut transport = AsyncMockTransport::new();
        transport.feed(encode_packet(&connect_packet("mobile-app", 60)).as_slice());

        let registry = std::sync::Arc::new(Mutex::<CriticalSectionRawMutex, _>::new(
            SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new(),
        ));
        let retained = std::sync::Arc::new(Mutex::<CriticalSectionRawMutex, _>::new(
            RetainedStore::<MAX_RETAINED>::new(),
        ));
        {
            let mut guard = block_on(registry.lock());
            let _ = guard
                .insert(session_with_subscription(
                    "watcher",
                    "sb/house1/device/relay-1/state",
                ))
                .unwrap();
        }

        let registry_for_thread = registry.clone();
        thread::spawn(move || loop {
            let mut guard = block_on(registry_for_thread.lock());
            if let Some(session_id) = guard.find_by_client_id("mobile-app") {
                guard.get_mut(session_id).unwrap().last_activity =
                    Instant::now().checked_sub(Duration::from_secs(120)).unwrap();
                break;
            }
            drop(guard);
            thread::sleep(StdDuration::from_millis(10));
        });

        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, INBOUND_N>::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        block_on(connection_loop(
            &mut transport,
            &registry,
            &retained,
            &inbound,
            &config(),
            &mut frame_buf,
        ));

        let registry_guard = block_on(registry.lock());
        assert!(registry_guard.find_by_client_id("mobile-app").is_none());
        let watcher_id = registry_guard.find_by_client_id("watcher").unwrap();
        assert!(!registry_guard.get(watcher_id).unwrap().outbox.is_empty());
        drop(registry_guard);

        let retained_guard = block_on(retained.lock());
        assert_eq!(retained_guard.len(), 1);
    }

    #[test]
    fn rate_limit_disconnect_removes_session() {
        let mut cfg = config();
        cfg.rate_capacity = 0;
        cfg.rate_per_sec = 0;
        cfg.max_violations = 50;

        let mut transport = AsyncMockTransport::new();
        transport.feed(encode_packet(&connect_packet("mobile-app", 60)).as_slice());
        for _ in 0..50 {
            transport.feed(
                encode_packet(&publish_packet(
                    "test/topic",
                    b"hello",
                    QosPid::AtMostOnce,
                ))
                .as_slice(),
            );
        }
        transport.finish();

        let registry = Mutex::<CriticalSectionRawMutex, _>::new(SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new());
        let retained = Mutex::<CriticalSectionRawMutex, _>::new(RetainedStore::<MAX_RETAINED>::new());
        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, INBOUND_N>::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        block_on(connection_loop(
            &mut transport,
            &registry,
            &retained,
            &inbound,
            &cfg,
            &mut frame_buf,
        ));

        assert!(block_on(registry.lock()).find_by_client_id("mobile-app").is_none());
    }

    #[test]
    fn qos1_retry_disconnect_removes_session() {
        let mut cfg = config();
        cfg.qos1_retry_ms = 1;
        cfg.qos1_max_retries = 3;

        let mut transport = AsyncMockTransport::new();
        transport.feed(encode_packet(&connect_packet("mobile-app", 60)).as_slice());

        let registry = std::sync::Arc::new(Mutex::<CriticalSectionRawMutex, _>::new(
            SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new(),
        ));
        let retained = std::sync::Arc::new(Mutex::<CriticalSectionRawMutex, _>::new(
            RetainedStore::<MAX_RETAINED>::new(),
        ));

        let registry_for_thread = registry.clone();
        thread::spawn(move || loop {
            let mut guard = block_on(registry_for_thread.lock());
            if let Some(session_id) = guard.find_by_client_id("mobile-app") {
                guard
                    .get_mut(session_id)
                    .unwrap()
                    .inflight_add(InflightEntry {
                        packet_id: 7,
                        topic: String::<128>::try_from("devices/kitchen/temp").unwrap(),
                        payload: Vec::<u8, 512>::from_slice(b"hello").unwrap(),
                        qos: QoS::AtLeastOnce,
                        sent_at: Instant::now().checked_sub(Duration::from_secs(2)).unwrap(),
                        retries: 0,
                    })
                    .unwrap();
                break;
            }
            drop(guard);
            thread::sleep(StdDuration::from_millis(10));
        });

        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, INBOUND_N>::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        block_on(connection_loop(
            &mut transport,
            &registry,
            &retained,
            &inbound,
            &cfg,
            &mut frame_buf,
        ));

        assert!(block_on(registry.lock()).find_by_client_id("mobile-app").is_none());
    }

    #[test]
    fn happy_path_connect_subscribe_outbound_disconnect() {
        let mut transport = AsyncMockTransport::new();
        let transport_writer = transport.clone();
        transport.feed(encode_packet(&connect_packet("mobile-app", 60)).as_slice());
        transport.feed(encode_packet(&subscribe_packet("test/topic", 1)).as_slice());
        let expected_publish = encode_packet(&publish_packet(
            "test/topic",
            b"hello",
            QosPid::AtMostOnce,
        ));

        let registry = std::sync::Arc::new(Mutex::<CriticalSectionRawMutex, _>::new(
            SessionRegistry::<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>::new(),
        ));
        let retained = std::sync::Arc::new(Mutex::<CriticalSectionRawMutex, _>::new(
            RetainedStore::<MAX_RETAINED>::new(),
        ));

        let registry_for_thread = registry.clone();
        thread::spawn(move || loop {
            let mut guard = block_on(registry_for_thread.lock());
            if let Some(session_id) = guard.find_by_client_id("mobile-app") {
                if !guard.get(session_id).unwrap().subscriptions.is_empty() {
                    guard
                        .get_mut(session_id)
                        .unwrap()
                        .outbox
                        .push(crate::session::state::OutboundPacket {
                            bytes: Vec::from_slice(
                                encode_packet(&publish_packet(
                                    "test/topic",
                                    b"hello",
                                    QosPid::AtMostOnce,
                                ))
                                .as_slice(),
                            )
                            .unwrap(),
                        })
                        .unwrap();
                    drop(guard);
                    while !transport_writer
                        .tx_log()
                        .iter()
                        .any(|frame| frame == &expected_publish)
                    {
                        thread::sleep(StdDuration::from_millis(10));
                    }
                    transport_writer.feed(encode_packet(&Packet::Disconnect).as_slice());
                    transport_writer.finish();
                    break;
                }
            }
            drop(guard);
            thread::sleep(StdDuration::from_millis(10));
        });

        let inbound = Channel::<CriticalSectionRawMutex, MqttIntent, INBOUND_N>::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        block_on(connection_loop(
            &mut transport,
            &registry,
            &retained,
            &inbound,
            &config(),
            &mut frame_buf,
        ));

        let tx = transport.tx_log();
        assert!(tx.iter().any(|frame| frame == &vec![0x20, 0x02, 0x00, 0x00]));
        assert!(tx.iter().any(|frame| frame == &vec![0x90, 0x03, 0x00, 0x01, 0x00]));
        assert!(tx.iter().any(|frame| {
            matches!(
                mqttrs::decode_slice(frame).unwrap().unwrap(),
                Packet::Publish(Publish { topic_name: "test/topic", payload: b"hello", .. })
            )
        }));
    }
}
