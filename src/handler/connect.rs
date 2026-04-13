use crate::codec::frame::{write_packet, WriteError};
use crate::session::registry::{RegistryError, SessionRegistry};
use crate::session::state::{ClientId, LwtMessage, SessionId, SessionState};
use crate::transport::Transport;
use heapless::{String, Vec};
use mqttrs::{Connect, ConnectReturnCode, Connack, Packet, Protocol};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HouseToken<'a> {
    pub username: &'a str,
    pub password: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectOutcome {
    pub session_id: SessionId,
    pub displaced_lwt: Option<LwtMessage>,
    /// Reflects the CONNACK session_present flag: true when an existing session was
    /// resumed (clean_session=false and a matching session was found in the registry).
    pub session_present: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PreparedConnect {
    Accepted(ConnectOutcome),
    Rejected(ConnectReturnCode),
}

#[derive(Debug, PartialEq)]
pub enum ConnectError<E> {
    Rejected(ConnectReturnCode),
    Write(WriteError<E>),
    Registry(RegistryError),
    ClientIdTooLong,
    WillTopicTooLong,
    WillPayloadTooLarge,
}

impl<E> From<WriteError<E>> for ConnectError<E> {
    fn from(value: WriteError<E>) -> Self {
        Self::Write(value)
    }
}

impl<E> From<RegistryError> for ConnectError<E> {
    fn from(value: RegistryError) -> Self {
        Self::Registry(value)
    }
}

pub async fn handle_connect<
    T: Transport,
    const MAX_SESSIONS: usize,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
    const MAX_PACKET_SIZE: usize,
>(
    transport: &mut T,
    registry: &mut SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>,
    packet: &Connect<'_>,
    house_token: &HouseToken<'_>,
    frame_buf: &mut [u8; MAX_PACKET_SIZE],
) -> Result<SessionId, ConnectError<T::Error>> {
    handle_connect_with_outcome(transport, registry, packet, house_token, frame_buf)
        .await
        .map(|outcome| outcome.session_id)
}

pub async fn handle_connect_with_outcome<
    T: Transport,
    const MAX_SESSIONS: usize,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
    const MAX_PACKET_SIZE: usize,
>(
    transport: &mut T,
    registry: &mut SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>,
    packet: &Connect<'_>,
    house_token: &HouseToken<'_>,
    frame_buf: &mut [u8; MAX_PACKET_SIZE],
) -> Result<ConnectOutcome, ConnectError<T::Error>> {
    match prepare_connect(registry, packet, house_token) {
        Ok(PreparedConnect::Accepted(outcome)) => {
            write_connack(transport, ConnectReturnCode::Accepted, outcome.session_present, frame_buf).await?;
            Ok(outcome)
        }
        Ok(PreparedConnect::Rejected(code)) => reject_connect(transport, code, frame_buf).await,
        Err(ConnectError::Registry(error)) => return Err(ConnectError::Registry(error)),
        Err(ConnectError::ClientIdTooLong) => return Err(ConnectError::ClientIdTooLong),
        Err(ConnectError::WillTopicTooLong) => return Err(ConnectError::WillTopicTooLong),
        Err(ConnectError::WillPayloadTooLarge) => return Err(ConnectError::WillPayloadTooLarge),
        Err(ConnectError::Rejected(code)) => return reject_connect(transport, code, frame_buf).await,
        Err(ConnectError::Write(_)) => unreachable!(),
    }
}

pub fn prepare_connect<
    const MAX_SESSIONS: usize,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
>(
    registry: &mut SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>,
    packet: &Connect<'_>,
    house_token: &HouseToken<'_>,
) -> Result<PreparedConnect, ConnectError<core::convert::Infallible>> {
    if packet.protocol != Protocol::MQTT311 {
        return Ok(PreparedConnect::Rejected(
            ConnectReturnCode::RefusedProtocolVersion,
        ));
    }

    if packet.client_id.is_empty() {
        return Ok(PreparedConnect::Rejected(
            ConnectReturnCode::RefusedIdentifierRejected,
        ));
    }

    if !matches_house_token(packet, house_token) {
        return Ok(PreparedConnect::Rejected(
            ConnectReturnCode::BadUsernamePassword,
        ));
    }

    // ── clean_session = false: resume existing session ───────────────────────
    //
    // MQTT 3.1.1 §3.1.2.4: if clean_session=0 and an existing session is found,
    // resume it — subscriptions, inflight, and outbox are preserved.
    // Only connection-level fields (LWT, keepalive) are updated from the new packet.
    // session_present=true is reflected in CONNACK so the client knows state was kept.
    if !packet.clean_session {
        if let Some(existing_id) = registry.find_by_client_id(packet.client_id) {
            let session = registry.get_mut(existing_id).unwrap();
            session.lwt = to_lwt(packet)?;
            session.keepalive_secs = packet.keep_alive;
            // Reset connection-level backpressure state — this is a fresh TCP connection.
            session.quarantined = false;
            session.outbox_drops = 0;
            // subscriptions, inflight, outbox: intentionally kept (persistent session)
            return Ok(PreparedConnect::Accepted(ConnectOutcome {
                session_id: existing_id,
                displaced_lwt: None,
                session_present: true,
            }));
        }
        // No existing session → fall through and create a fresh one.
        // session_present remains false.
    }

    // ── clean_session = true (or no existing session) ────────────────────────
    //
    // Discard any previous session for this client_id, then create a fresh one.
    let mut displaced_lwt = None;
    if let Some(existing_id) = registry.find_by_client_id(packet.client_id) {
        if let Some(previous) = registry.remove(existing_id) {
            displaced_lwt = previous.lwt;
        }
    }

    if registry.is_full() {
        if let Some(lwt) = displaced_lwt {
            registry.record_published_lwt(lwt)?;
        }

        return Ok(PreparedConnect::Rejected(
            ConnectReturnCode::ServerUnavailable,
        ));
    }

    let mut state = SessionState::<MAX_SUBS, MAX_INFLIGHT>::new(
        to_client_id(packet.client_id)?,
        packet.keep_alive,
    );
    state.lwt = to_lwt(packet)?;

    let session_id = registry.insert(state)?;

    if let Some(lwt) = displaced_lwt.clone() {
        registry.record_published_lwt(lwt)?;
    }

    Ok(PreparedConnect::Accepted(ConnectOutcome {
        session_id,
        displaced_lwt,
        session_present: false,
    }))
}

pub async fn write_connack<T: Transport, const MAX_PACKET_SIZE: usize>(
    transport: &mut T,
    code: ConnectReturnCode,
    session_present: bool,
    frame_buf: &mut [u8; MAX_PACKET_SIZE],
) -> Result<(), WriteError<T::Error>> {
    write_packet(
        transport,
        &Packet::Connack(Connack {
            session_present,
            code,
        }),
        frame_buf,
    )
    .await
}

async fn reject_connect<T: Transport, const MAX_PACKET_SIZE: usize>(
    transport: &mut T,
    code: ConnectReturnCode,
    frame_buf: &mut [u8; MAX_PACKET_SIZE],
) -> Result<ConnectOutcome, ConnectError<T::Error>> {
    // session_present is always false for rejected connections (MQTT 3.1.1 §3.2.2.2)
    write_connack(transport, code, false, frame_buf).await?;
    transport.close().await;
    Err(ConnectError::Rejected(code))
}

fn matches_house_token(packet: &Connect<'_>, house_token: &HouseToken<'_>) -> bool {
    let Some(username) = packet.username else {
        return false;
    };
    let Some(password) = packet.password else {
        return false;
    };

    ct_eq(username.as_bytes(), house_token.username.as_bytes())
        && ct_eq(password, house_token.password.as_bytes())
}

fn to_client_id<E>(client_id: &str) -> Result<ClientId, ConnectError<E>> {
    String::<64>::try_from(client_id).map_err(|_| ConnectError::ClientIdTooLong)
}

fn to_lwt<E>(
    packet: &Connect<'_>,
) -> Result<Option<LwtMessage>, ConnectError<E>> {
    let Some(last_will) = packet.last_will.as_ref() else {
        return Ok(None);
    };

    let topic = String::<128>::try_from(last_will.topic).map_err(|_| ConnectError::WillTopicTooLong)?;
    let payload =
        Vec::<u8, 512>::from_slice(last_will.message).map_err(|_| ConnectError::WillPayloadTooLarge)?;

    Ok(Some(LwtMessage {
        topic,
        payload,
        qos: last_will.qos,
        retain: last_will.retain,
    }))
}

pub fn ct_eq(lhs: &[u8], rhs: &[u8]) -> bool {
    let max_len = core::cmp::max(lhs.len(), rhs.len());
    let mut diff = lhs.len() ^ rhs.len();

    for idx in 0..max_len {
        let left = lhs.get(idx).copied().unwrap_or(0);
        let right = rhs.get(idx).copied().unwrap_or(0);
        diff |= (left ^ right) as usize;
    }

    diff == 0
}

#[cfg(test)]
mod tests {
    use super::{ct_eq, handle_connect, handle_connect_with_outcome, ConnectError, HouseToken};
    use crate::session::registry::SessionRegistry;
    use crate::session::state::{LwtMessage, SessionState};
    use crate::transport::mock::MockTransport;
    use core::future::Future;
    use core::pin::{pin, Pin};
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    use heapless::{String, Vec};
    use mqttrs::{Connect, ConnectReturnCode, LastWill, Protocol, QoS};
    use std::vec;

    const MAX_SESSIONS: usize = 8;
    const MAX_SUBS: usize = 32;
    const MAX_INFLIGHT: usize = 16;
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

    fn house_token() -> HouseToken<'static> {
        HouseToken {
            username: "house",
            password: "secret",
        }
    }

    fn valid_connect<'a>() -> Connect<'a> {
        Connect {
            protocol: Protocol::MQTT311,
            keep_alive: 60,
            client_id: "mobile-app",
            clean_session: true,
            last_will: None,
            username: Some("house"),
            password: Some(b"secret"),
        }
    }

    fn registry() -> SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT> {
        SessionRegistry::new()
    }

    #[test]
    fn connect_with_valid_token_is_accepted() {
        let mut transport = MockTransport::new();
        let mut registry = registry();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        let session_id = block_on(handle_connect(
            &mut transport,
            &mut registry,
            &valid_connect(),
            &house_token(),
            &mut frame_buf,
        ))
        .unwrap();

        assert_eq!(session_id, 0);
        assert_eq!(transport.tx_log, vec![vec![0x20, 0x02, 0x00, 0x00]]);
        assert!(!transport.closed);
    }

    #[test]
    fn wrong_password_is_rejected_and_transport_closed() {
        let mut packet = valid_connect();
        packet.password = Some(b"wrong");

        let mut transport = MockTransport::new();
        let mut registry = registry();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        let err = block_on(handle_connect(
            &mut transport,
            &mut registry,
            &packet,
            &house_token(),
            &mut frame_buf,
        ))
        .unwrap_err();

        assert_eq!(err, ConnectError::Rejected(ConnectReturnCode::BadUsernamePassword));
        assert_eq!(transport.tx_log, vec![vec![0x20, 0x02, 0x00, 0x04]]);
        assert!(transport.closed);
    }

    #[test]
    fn wrong_username_is_rejected_and_transport_closed() {
        let mut packet = valid_connect();
        packet.username = Some("wrong");

        let mut transport = MockTransport::new();
        let mut registry = registry();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        let err = block_on(handle_connect(
            &mut transport,
            &mut registry,
            &packet,
            &house_token(),
            &mut frame_buf,
        ))
        .unwrap_err();

        assert_eq!(err, ConnectError::Rejected(ConnectReturnCode::BadUsernamePassword));
        assert_eq!(transport.tx_log, vec![vec![0x20, 0x02, 0x00, 0x04]]);
        assert!(transport.closed);
    }

    #[test]
    fn empty_client_id_is_rejected() {
        let mut packet = valid_connect();
        packet.client_id = "";

        let mut transport = MockTransport::new();
        let mut registry = registry();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        let err = block_on(handle_connect(
            &mut transport,
            &mut registry,
            &packet,
            &house_token(),
            &mut frame_buf,
        ))
        .unwrap_err();

        assert_eq!(
            err,
            ConnectError::Rejected(ConnectReturnCode::RefusedIdentifierRejected)
        );
        assert_eq!(transport.tx_log, vec![vec![0x20, 0x02, 0x00, 0x02]]);
        assert!(transport.closed);
    }

    #[test]
    fn unsupported_protocol_is_rejected() {
        let mut packet = valid_connect();
        packet.protocol = Protocol::MQIsdp;

        let mut transport = MockTransport::new();
        let mut registry = registry();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        let err = block_on(handle_connect(
            &mut transport,
            &mut registry,
            &packet,
            &house_token(),
            &mut frame_buf,
        ))
        .unwrap_err();

        assert_eq!(
            err,
            ConnectError::Rejected(ConnectReturnCode::RefusedProtocolVersion)
        );
        assert_eq!(transport.tx_log, vec![vec![0x20, 0x02, 0x00, 0x01]]);
        assert!(transport.closed);
    }

    #[test]
    fn full_registry_is_rejected_with_service_unavailable() {
        let mut registry = registry();
        for client_id in [
            "client-0", "client-1", "client-2", "client-3", "client-4", "client-5", "client-6",
            "client-7",
        ] {
            registry
                .insert(SessionState::new(String::<64>::try_from(client_id).unwrap(), 30))
                .unwrap();
        }

        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];
        let mut packet = valid_connect();
        packet.client_id = "new-client";

        let err = block_on(handle_connect(
            &mut transport,
            &mut registry,
            &packet,
            &house_token(),
            &mut frame_buf,
        ))
        .unwrap_err();

        assert_eq!(err, ConnectError::Rejected(ConnectReturnCode::ServerUnavailable));
        assert_eq!(transport.tx_log, vec![vec![0x20, 0x02, 0x00, 0x03]]);
        assert!(transport.closed);
    }

    #[test]
    fn nine_valid_connect_attempts_keep_registry_bounded_to_eight_sessions() {
        let mut registry = registry();

        for idx in 0..MAX_SESSIONS {
            let client_id = std::format!("client-{idx}");
            let mut transport = MockTransport::new();
            let mut frame_buf = [0u8; MAX_PACKET_SIZE];
            let mut packet = valid_connect();
            packet.client_id = std::boxed::Box::leak(client_id.into_boxed_str());

            let session_id = block_on(handle_connect(
                &mut transport,
                &mut registry,
                &packet,
                &house_token(),
                &mut frame_buf,
            ))
            .unwrap();

            assert_eq!(session_id, idx);
            assert_eq!(transport.tx_log, vec![vec![0x20, 0x02, 0x00, 0x00]]);
            assert!(!transport.closed);
        }

        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];
        let mut packet = valid_connect();
        packet.client_id = "client-8";

        let err = block_on(handle_connect(
            &mut transport,
            &mut registry,
            &packet,
            &house_token(),
            &mut frame_buf,
        ))
        .unwrap_err();

        assert_eq!(err, ConnectError::Rejected(ConnectReturnCode::ServerUnavailable));
        assert_eq!(registry.len(), MAX_SESSIONS);
        assert!(registry.is_full());
        assert!(registry.find_by_client_id("client-8").is_none());
        assert_eq!(transport.tx_log, vec![vec![0x20, 0x02, 0x00, 0x03]]);
        assert!(transport.closed);
    }

    #[test]
    fn connect_with_lwt_stores_it_in_session_state() {
        let mut packet = valid_connect();
        packet.last_will = Some(LastWill {
            topic: "house/device/status",
            message: b"offline",
            qos: QoS::AtLeastOnce,
            retain: true,
        });

        let mut transport = MockTransport::new();
        let mut registry = registry();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        let session_id = block_on(handle_connect(
            &mut transport,
            &mut registry,
            &packet,
            &house_token(),
            &mut frame_buf,
        ))
        .unwrap();

        let lwt = registry.get(session_id).unwrap().lwt.clone().unwrap();
        assert_eq!(lwt.topic.as_str(), "house/device/status");
        assert_eq!(lwt.payload.as_slice(), b"offline");
        assert_eq!(lwt.qos, QoS::AtLeastOnce);
        assert!(lwt.retain);
    }

    #[test]
    fn duplicate_client_id_removes_old_session_and_records_old_lwt() {
        let mut registry = registry();
        let mut previous = SessionState::new(String::<64>::try_from("mobile-app").unwrap(), 30);
        previous.lwt = Some(LwtMessage {
            topic: String::<128>::try_from("house/device/status").unwrap(),
            payload: Vec::<u8, 512>::from_slice(b"offline").unwrap(),
            qos: QoS::AtLeastOnce,
            retain: true,
        });
        let old_session_id = registry.insert(previous).unwrap();

        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        let outcome = block_on(handle_connect_with_outcome(
            &mut transport,
            &mut registry,
            &valid_connect(),
            &house_token(),
            &mut frame_buf,
        ))
        .unwrap();

        assert_eq!(registry.len(), 1);
        assert_eq!(outcome.session_id, old_session_id);
        assert!(registry.get(old_session_id).is_some());
        assert_eq!(registry.published_lwts().len(), 1);
        assert_eq!(registry.published_lwts()[0].topic.as_str(), "house/device/status");
        assert_eq!(registry.published_lwts()[0].payload.as_slice(), b"offline");
        assert_eq!(outcome.displaced_lwt.unwrap().payload.as_slice(), b"offline");
    }

    #[test]
    fn missing_username_and_password_is_rejected() {
        let mut packet = valid_connect();
        packet.username = None;
        packet.password = None;

        let mut transport = MockTransport::new();
        let mut registry = registry();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];

        let err = block_on(handle_connect(
            &mut transport,
            &mut registry,
            &packet,
            &house_token(),
            &mut frame_buf,
        ))
        .unwrap_err();

        assert_eq!(err, ConnectError::Rejected(ConnectReturnCode::BadUsernamePassword));
        assert_eq!(transport.tx_log, vec![vec![0x20, 0x02, 0x00, 0x04]]);
        assert!(transport.closed);
    }

    // ── clean_session semantics ───────────────────────────────────────────────

    #[test]
    fn clean_session_true_always_creates_fresh_session_and_sends_no_session_present() {
        let mut registry = registry();
        // Pre-populate a session for this client.
        registry
            .insert(SessionState::new(String::<64>::try_from("mobile-app").unwrap(), 30))
            .unwrap();

        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];
        // valid_connect() has clean_session: true
        let id = block_on(handle_connect(
            &mut transport,
            &mut registry,
            &valid_connect(),
            &house_token(),
            &mut frame_buf,
        ))
        .unwrap();

        // session_present bit (byte 3) must be 0x00
        assert_eq!(transport.tx_log, vec![vec![0x20, 0x02, 0x00, 0x00]]);
        assert_eq!(registry.len(), 1, "old session replaced by fresh one");
        // The session was replaced — new session_id should still map to mobile-app.
        assert_eq!(
            registry.get(id).unwrap().client_id.as_str(),
            "mobile-app"
        );
    }

    #[test]
    fn clean_session_false_no_existing_session_creates_fresh_and_returns_no_session_present() {
        let mut registry = registry();
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];
        let mut packet = valid_connect();
        packet.clean_session = false;

        block_on(handle_connect(
            &mut transport,
            &mut registry,
            &packet,
            &house_token(),
            &mut frame_buf,
        ))
        .unwrap();

        // No prior session existed → session_present = 0x00
        assert_eq!(transport.tx_log, vec![vec![0x20, 0x02, 0x00, 0x00]]);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn clean_session_false_resumes_existing_session_and_sends_session_present_true() {
        let mut registry = registry();
        // Pre-populate a session for this client.
        registry
            .insert(SessionState::new(String::<64>::try_from("mobile-app").unwrap(), 30))
            .unwrap();

        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];
        let mut packet = valid_connect();
        packet.clean_session = false;

        block_on(handle_connect(
            &mut transport,
            &mut registry,
            &packet,
            &house_token(),
            &mut frame_buf,
        ))
        .unwrap();

        // Existing session reused → session_present bit (byte 3) = 0x01
        assert_eq!(transport.tx_log, vec![vec![0x20, 0x02, 0x01, 0x00]]);
        assert_eq!(registry.len(), 1, "no duplicate session created");
    }

    #[test]
    fn clean_session_false_preserves_subscriptions_on_resume() {
        use crate::session::state::Subscription;

        let mut registry = registry();
        let mut existing = SessionState::new(
            String::<64>::try_from("mobile-app").unwrap(), 30,
        );
        existing
            .subscriptions
            .push(Subscription {
                filter: String::<128>::try_from("sb/house1/device/+/state").unwrap(),
                qos: QoS::AtLeastOnce,
            })
            .unwrap();
        let old_id = registry.insert(existing).unwrap();

        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];
        let mut packet = valid_connect();
        packet.clean_session = false;

        let resumed_id = block_on(handle_connect(
            &mut transport,
            &mut registry,
            &packet,
            &house_token(),
            &mut frame_buf,
        ))
        .unwrap();

        assert_eq!(resumed_id, old_id, "same session slot reused");
        let subs = &registry.get(resumed_id).unwrap().subscriptions;
        assert_eq!(subs.len(), 1, "subscription survived reconnect");
        assert_eq!(subs[0].filter.as_str(), "sb/house1/device/+/state");
    }

    #[test]
    fn clean_session_false_updates_lwt_and_keepalive_on_resume() {
        let mut registry = registry();
        let old = SessionState::new(String::<64>::try_from("mobile-app").unwrap(), 30);
        registry.insert(old).unwrap();

        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];
        let mut packet = valid_connect();
        packet.clean_session = false;
        packet.keep_alive = 120;
        packet.last_will = Some(LastWill {
            topic: "sb/house1/device/app/state",
            message: b"offline",
            qos: QoS::AtLeastOnce,
            retain: true,
        });

        let resumed_id = block_on(handle_connect(
            &mut transport,
            &mut registry,
            &packet,
            &house_token(),
            &mut frame_buf,
        ))
        .unwrap();

        let session = registry.get(resumed_id).unwrap();
        assert_eq!(session.keepalive_secs, 120);
        let lwt = session.lwt.as_ref().unwrap();
        assert_eq!(lwt.topic.as_str(), "sb/house1/device/app/state");
        assert_eq!(lwt.payload.as_slice(), b"offline");
    }

    #[test]
    fn clean_session_false_resets_quarantine_flag_on_resume() {
        let mut registry = registry();
        let mut quarantined = SessionState::new(
            String::<64>::try_from("mobile-app").unwrap(), 30,
        );
        quarantined.quarantined = true;
        quarantined.outbox_drops = 99;
        registry.insert(quarantined).unwrap();

        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; MAX_PACKET_SIZE];
        let mut packet = valid_connect();
        packet.clean_session = false;

        let resumed_id = block_on(handle_connect(
            &mut transport,
            &mut registry,
            &packet,
            &house_token(),
            &mut frame_buf,
        ))
        .unwrap();

        let session = registry.get(resumed_id).unwrap();
        assert!(!session.quarantined, "quarantine must be cleared on reconnect");
        assert_eq!(session.outbox_drops, 0);
    }

    #[test]
    fn ct_eq_matches_only_identical_inputs() {
        assert!(ct_eq(b"secret", b"secret"));
        assert!(!ct_eq(b"secret", b"secreu"));
        assert!(!ct_eq(b"secret", b"secret-2"));
    }
}
