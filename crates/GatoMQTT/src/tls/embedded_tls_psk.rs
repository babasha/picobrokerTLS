//! `TlsSession` adapter on top of the vendored `embedded-tls` server-mode
//! primitives (TLS 1.3 PSK_KE).
//!
//! The adapter wraps any [`Transport`](crate::transport::Transport) (e.g. a
//! TCP socket) and drives the handshake byte-stream through
//! `embedded_tls::server::TlsServerSession`, which is itself I/O-free.
//! Application data records are encrypted/decrypted in caller-supplied
//! buffers attached to the adapter struct.
//!
//! ## Feature gate
//!
//! Compiled only when the `tls-psk` Cargo feature is enabled.
//!
//! ## Buffer sizing
//!
//! Three internal buffers are sized by the const generic `BUF`:
//!   * `record_buf` — staging for one incoming TLS record (up to BUF bytes
//!     including the 5-byte header).
//!   * `out_buf` — staging for outgoing records the server emits during the
//!     handshake (carries `ServerHello + EE + Finished`, ≤ ~200 bytes for
//!     plain PSK).
//!   * `plain_buf` — decrypted plaintext from the most recent application
//!     data record (the unread tail is replayed across `read` calls).
//!
//! Default `BUF = 4096`, plenty for MQTT control packets. Callers expecting
//! larger app messages can specialise.

use core::convert::Infallible;

use gatopsktls::TlsError;
use gatopsktls::alert::{AlertDescription, AlertLevel};
use gatopsktls::server::{
    AppDataOrAlert, HandshakeOutput, TlsServerConfig, TlsServerSession,
};

// Re-export so callers can build `PskConfig.dhe_keypair` without
// depending on `gatopsktls` directly. Name shadows the symbol used
// internally below — both refer to the same type.
pub use gatopsktls::server::DheKeyShare;

use crate::transport::Transport;
use crate::transport::tls::TlsSession;

const RECORD_HEADER_LEN: usize = 5;

const TLS_CT_CHANGE_CIPHER_SPEC: u8 = 20;
const TLS_CT_ALERT: u8 = 21;
const TLS_CT_HANDSHAKE: u8 = 22;
const TLS_CT_APPLICATION_DATA: u8 = 23;

/// Errors returned by the embedded-tls PSK adapter.
#[derive(Debug)]
pub enum SessionError<TransportErr> {
    /// Underlying transport reported an error (lost socket, timeout, …).
    Transport(TransportErr),
    /// `embedded-tls` rejected an input or hit a crypto error.
    Tls(TlsError),
    /// Peer closed the connection mid-record.
    Eof,
    /// Record body advertised in the header is larger than our internal buffer.
    RecordTooLarge { advertised: usize, capacity: usize },
    /// Record type the state machine wasn't expecting at this point in the
    /// session lifecycle.
    UnexpectedRecordType(u8),
    /// Caller invoked an operation that requires a completed handshake before
    /// `accept()` had been driven to completion.
    HandshakeIncomplete,
    /// Peer-initiated close arrived while we were trying to read app data.
    Closed,
}

impl<E: core::fmt::Debug> SessionError<E> {
    fn from_tls(error: TlsError) -> Self {
        Self::Tls(error)
    }
}

/// PSK handshake configuration for a single inbound connection.
///
/// `dhe_keypair = None` keeps the legacy `psk_ke`-only behaviour
/// (forward-secrecy-less, but cheap). `dhe_keypair = Some(_)` enables
/// `psk_dhe_ke` when the client offers it — required for interop with
/// stock OpenSSL/mosquitto, which won't negotiate plain `psk_ke` without
/// `-allow_no_dhe_kex`. Generate a fresh keypair per connection via
/// [`DheKeyShare::generate`]; reusing one across sessions defeats
/// forward secrecy.
///
/// Note: deliberately not `Debug` — `secret` and `dhe_keypair` would
/// expose key material via accidental `{:?}` logging.
#[derive(Clone)]
pub struct PskConfig<'cfg> {
    /// PSK identity sent by clients on the wire (e.g. `b"gatomqtt-psk"`).
    pub identity: &'cfg [u8],
    /// PSK secret bytes (32 bytes for SHA-256-based suites).
    pub secret: &'cfg [u8],
    /// Optional ephemeral X25519 keypair for `psk_dhe_ke`. See module docs.
    pub dhe_keypair: Option<DheKeyShare>,
}

/// Adapter that exposes a [`TlsSession`] over the vendored embedded-tls
/// server-mode primitives.
///
/// Construct with `new`, then drive the handshake via `accept().await` (the
/// trait method) before passing the adapter to `TlsTransport::new` and the
/// broker `connection_loop`.
pub struct EmbeddedTlsPskSession<'cfg, T, const BUF: usize = 4096> {
    transport: T,
    inner: TlsServerSession,
    config: PskConfig<'cfg>,
    /// Server-side random for the ServerHello — caller seeds from a CSPRNG.
    server_random: [u8; 32],
    record_buf: [u8; BUF],
    /// Length of the current record in `record_buf` (header + body), or 0 if
    /// the buffer is empty.
    record_len: usize,
    out_buf: [u8; BUF],
    plain_buf: [u8; BUF],
    /// Bytes of decrypted plaintext currently buffered.
    plain_len: usize,
    /// Read cursor inside the buffered plaintext.
    plain_offset: usize,
    handshake_done: bool,
    closed: bool,
}

impl<'cfg, T, const BUF: usize> EmbeddedTlsPskSession<'cfg, T, BUF>
where
    T: Transport,
{
    /// Build a fresh session bound to `transport`. The caller is responsible
    /// for supplying `server_random` from a CSPRNG; we deliberately don't
    /// depend on a global RNG here so the adapter stays no_std-friendly.
    pub fn new(transport: T, config: PskConfig<'cfg>, server_random: [u8; 32]) -> Self {
        Self {
            transport,
            inner: TlsServerSession::new(),
            config,
            server_random,
            record_buf: [0u8; BUF],
            record_len: 0,
            out_buf: [0u8; BUF],
            plain_buf: [0u8; BUF],
            plain_len: 0,
            plain_offset: 0,
            handshake_done: false,
            closed: false,
        }
    }

    /// Read exactly `len` bytes into `record_buf[start..start+len]`.
    async fn read_exact_into(
        &mut self,
        start: usize,
        len: usize,
    ) -> Result<(), SessionError<T::Error>> {
        let end = start + len;
        if end > self.record_buf.len() {
            return Err(SessionError::RecordTooLarge {
                advertised: end,
                capacity: self.record_buf.len(),
            });
        }
        let mut filled = start;
        while filled < end {
            let n = self
                .transport
                .read(&mut self.record_buf[filled..end])
                .await
                .map_err(SessionError::Transport)?;
            if n == 0 {
                return Err(SessionError::Eof);
            }
            filled += n;
        }
        Ok(())
    }

    /// Pull one TLS record off the wire into `record_buf`. After this returns,
    /// `record_buf[..record_len]` holds the full record (header + body).
    async fn fetch_record(&mut self) -> Result<(), SessionError<T::Error>> {
        // Header: 5 bytes — type(1) + version(2) + length(2).
        self.read_exact_into(0, RECORD_HEADER_LEN).await?;
        let body_len =
            u16::from_be_bytes([self.record_buf[3], self.record_buf[4]]) as usize;
        let total = RECORD_HEADER_LEN + body_len;
        if total > self.record_buf.len() {
            return Err(SessionError::RecordTooLarge {
                advertised: total,
                capacity: self.record_buf.len(),
            });
        }
        self.read_exact_into(RECORD_HEADER_LEN, body_len).await?;
        self.record_len = total;
        Ok(())
    }

    fn record_type(&self) -> u8 {
        self.record_buf[0]
    }

    /// Drive the server-side TLS 1.3 (PSK_KE or PSK_DHE_KE) handshake to
    /// completion. Loops over `process_client_hello` to handle a single
    /// HelloRetryRequest round-trip when the client offers `psk_dhe_ke`
    /// without a usable X25519 share — required for interop with stock
    /// OpenSSL/mosquitto, which always wraps PSK with DHE for forward
    /// secrecy.
    async fn do_accept(&mut self) -> Result<(), SessionError<T::Error>> {
        // CH1 → server first flight, with at most one HRR round-trip
        // for the case where the client offered psk_dhe_ke without a
        // usable X25519 share. Per RFC 8446 §4.1.4 the server may emit
        // exactly one HRR per handshake.
        //
        // 1) Read CH1.
        self.fetch_record().await?;
        if self.record_type() != TLS_CT_HANDSHAKE {
            return Err(SessionError::UnexpectedRecordType(self.record_type()));
        }
        let ch1_len = self.record_len - RECORD_HEADER_LEN;
        let cfg = TlsServerConfig {
            psk: (self.config.identity, self.config.secret),
            server_random: self.server_random,
            dhe_keypair: self.config.dhe_keypair.clone(),
        };
        let flight_len = match self
            .inner
            .process_client_hello(
                &self.record_buf[RECORD_HEADER_LEN..RECORD_HEADER_LEN + ch1_len],
                &cfg,
                &mut self.out_buf,
            )
            .map_err(SessionError::from_tls)?
        {
            HandshakeOutput::FirstFlight(bytes) => bytes.len(),
            HandshakeOutput::HelloRetryRequest(bytes) => {
                // 2a) Write HRR.
                let hrr_len = bytes.len();
                self.transport
                    .write(&self.out_buf[..hrr_len])
                    .await
                    .map_err(SessionError::Transport)?;
                // 2b) Drain dummy CCS records (RFC 8446 §D.4).
                loop {
                    self.fetch_record().await?;
                    match self.record_type() {
                        TLS_CT_CHANGE_CIPHER_SPEC => continue,
                        TLS_CT_HANDSHAKE => break,
                        other => {
                            return Err(SessionError::UnexpectedRecordType(other));
                        }
                    }
                }
                // 2c) Process CH2. Same session — it remembers internally
                //     that one HRR has been issued and rejects a second
                //     per RFC 8446 §4.1.4 / §4.1.2.
                let ch2_len = self.record_len - RECORD_HEADER_LEN;
                let cfg2 = TlsServerConfig {
                    psk: (self.config.identity, self.config.secret),
                    server_random: self.server_random,
                    dhe_keypair: self.config.dhe_keypair.clone(),
                };
                match self
                    .inner
                    .process_client_hello(
                        &self.record_buf[RECORD_HEADER_LEN..RECORD_HEADER_LEN + ch2_len],
                        &cfg2,
                        &mut self.out_buf,
                    )
                    .map_err(SessionError::from_tls)?
                {
                    HandshakeOutput::FirstFlight(b) => b.len(),
                    HandshakeOutput::HelloRetryRequest(_) => {
                        return Err(SessionError::from_tls(TlsError::InvalidHandshake));
                    }
                }
            }
        };

        self.transport
            .write(&self.out_buf[..flight_len])
            .await
            .map_err(SessionError::Transport)?;

        // ── 3. Drain ChangeCipherSpec dummies, then read encrypted Finished.
        loop {
            self.fetch_record().await?;
            match self.record_type() {
                TLS_CT_CHANGE_CIPHER_SPEC => continue,
                TLS_CT_APPLICATION_DATA => break,
                other => return Err(SessionError::UnexpectedRecordType(other)),
            }
        }
        // Direct field access (not `self.record_slice()`) so the borrow checker
        // can see the immutable view of `record_buf` and the mutable borrow of
        // `inner` are on disjoint fields.
        let record_len = self.record_len;
        self.inner
            .process_client_finished(&self.record_buf[..record_len])
            .map_err(SessionError::from_tls)?;

        self.handshake_done = true;
        Ok(())
    }

    /// Pull one application-phase record off the wire, decrypt it into
    /// `plain_buf`, and update `plain_len`/`plain_offset`. Tolerates and
    /// skips CCS dummies. A peer-initiated `close_notify` (RFC 8446 §6 —
    /// encrypted Alert with inner CT = 21) is reported as `Closed` so the
    /// `read` path can return EOF cleanly. A non-`close_notify` Alert
    /// (e.g. peer reporting a fatal error) also triggers `Closed`; the
    /// extra detail is logged via the level/description but the session
    /// just shuts down either way for the v1 contract.
    async fn refill_plaintext(&mut self) -> Result<(), SessionError<T::Error>> {
        loop {
            self.fetch_record().await?;
            let ct = self.record_type();
            match ct {
                TLS_CT_CHANGE_CIPHER_SPEC => continue,
                TLS_CT_APPLICATION_DATA => break,
                TLS_CT_ALERT => {
                    // Plaintext Alert at the outer record level — only
                    // legal pre-handshake (i.e. never on this code path,
                    // which runs after handshake completion). Be defensive.
                    self.closed = true;
                    return Err(SessionError::Closed);
                }
                other => return Err(SessionError::UnexpectedRecordType(other)),
            }
        }

        let record_len = self.record_len;
        match self
            .inner
            .decrypt_app_data_or_alert(&self.record_buf[..record_len], &mut self.plain_buf)
            .map_err(SessionError::from_tls)?
        {
            AppDataOrAlert::AppData(plaintext) => {
                let len = plaintext.len();
                self.plain_len = len;
                self.plain_offset = 0;
                Ok(())
            }
            AppDataOrAlert::Alert { .. } => {
                // Peer signalled close (close_notify) or a fatal error.
                // Either way the session is done — surface as a clean
                // close so the reader returns EOF.
                self.closed = true;
                Err(SessionError::Closed)
            }
        }
    }
}

impl<'cfg, T, const BUF: usize> TlsSession for EmbeddedTlsPskSession<'cfg, T, BUF>
where
    T: Transport,
{
    type Error = SessionError<T::Error>;

    async fn accept(&mut self) -> Result<(), Self::Error> {
        if self.handshake_done {
            return Ok(());
        }
        self.do_accept().await
    }

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        if !self.handshake_done {
            return Err(SessionError::HandshakeIncomplete);
        }
        if self.closed {
            return Ok(0);
        }
        if self.plain_offset >= self.plain_len {
            // Buffer drained — pull next record.
            match self.refill_plaintext().await {
                Ok(()) => {}
                Err(SessionError::Closed) | Err(SessionError::Eof) => return Ok(0),
                Err(other) => return Err(other),
            }
        }
        let available = self.plain_len - self.plain_offset;
        let n = core::cmp::min(buf.len(), available);
        buf[..n]
            .copy_from_slice(&self.plain_buf[self.plain_offset..self.plain_offset + n]);
        self.plain_offset += n;
        Ok(n)
    }

    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        if !self.handshake_done {
            return Err(SessionError::HandshakeIncomplete);
        }
        if self.closed {
            return Err(SessionError::Closed);
        }
        // We may need to fragment if `buf` exceeds the maximum plaintext we
        // can fit in a single record under our buffer budget.
        // Per RFC 8446 §5.1, max plaintext = 2^14. We further cap by
        // (BUF - record_header(5) - inner_marker(1) - aead_tag(16)).
        let max_chunk = BUF.saturating_sub(RECORD_HEADER_LEN + 1 + 16);
        let chunk = core::cmp::min(buf.len(), max_chunk);
        if chunk == 0 {
            return Err(SessionError::Tls(TlsError::InsufficientSpace));
        }
        let record_len = self
            .inner
            .encrypt_app_data(&buf[..chunk], &mut self.out_buf)
            .map_err(SessionError::from_tls)?
            .len();
        self.transport
            .write(&self.out_buf[..record_len])
            .await
            .map_err(SessionError::Transport)?;
        Ok(chunk)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        // Records are sent eagerly inside `write`; nothing to flush.
        Ok(())
    }

    async fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        if !self.handshake_done {
            // Can't send a TLS alert without keys — just drop the transport.
            self.transport.close().await;
            return;
        }
        // Emit a proper TLS 1.3 close_notify (RFC 8446 §6.1): warning-level
        // Alert wrapped in an outer ApplicationData TLSCiphertext under
        // the application traffic keys. Best-effort — if encrypt or
        // socket write fails we still close the transport so resources
        // get released.
        if let Ok(record) = self.inner.encrypt_alert(
            AlertLevel::Warning,
            AlertDescription::CloseNotify,
            &mut self.out_buf,
        ) {
            let len = record.len();
            let _ = self.transport.write(&self.out_buf[..len]).await;
        }
        self.transport.close().await;
    }
}

// Keep the Infallible re-export available for downstream `where` bounds.
#[allow(dead_code)]
type _PhantomInfallible = Infallible;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::mock::MockTransport;
    use core::future::Future;
    use core::pin::{Pin, pin};
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

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
        loop {
            match Pin::as_mut(&mut future).poll(&mut cx) {
                Poll::Ready(out) => return out,
                Poll::Pending => panic!("future unexpectedly pending"),
            }
        }
    }

    #[test]
    fn write_before_accept_errors() {
        let transport = MockTransport::new();
        let cfg = PskConfig {
            identity: b"id",
            secret: &[0u8; 32],
        };
        let mut session: EmbeddedTlsPskSession<'_, MockTransport, 1024> =
            EmbeddedTlsPskSession::new(transport, cfg, [0u8; 32]);
        let err = block_on(session.write(b"hello")).unwrap_err();
        assert!(matches!(err, SessionError::HandshakeIncomplete));
    }

    #[test]
    fn read_before_accept_errors() {
        let transport = MockTransport::new();
        let cfg = PskConfig {
            identity: b"id",
            secret: &[0u8; 32],
        };
        let mut session: EmbeddedTlsPskSession<'_, MockTransport, 1024> =
            EmbeddedTlsPskSession::new(transport, cfg, [0u8; 32]);
        let mut buf = [0u8; 16];
        let err = block_on(session.read(&mut buf)).unwrap_err();
        assert!(matches!(err, SessionError::HandshakeIncomplete));
    }

    #[test]
    fn close_marks_session_closed_and_calls_transport_close() {
        let transport = MockTransport::new();
        let cfg = PskConfig {
            identity: b"id",
            secret: &[0u8; 32],
        };
        let mut session: EmbeddedTlsPskSession<'_, MockTransport, 1024> =
            EmbeddedTlsPskSession::new(transport, cfg, [0u8; 32]);
        block_on(session.close());
        assert!(session.closed);
        assert!(session.transport.closed);
    }

    #[test]
    fn read_returns_eof_when_transport_returns_zero_during_record_header() {
        // Transport produces 0 bytes immediately — fetch_record should see
        // an immediate EOF on the header read.
        let transport = MockTransport::new();
        let cfg = PskConfig {
            identity: b"id",
            secret: &[0u8; 32],
        };
        let mut session: EmbeddedTlsPskSession<'_, MockTransport, 1024> =
            EmbeddedTlsPskSession::new(transport, cfg, [0u8; 32]);
        // Force handshake_done = true so we exercise refill_plaintext.
        session.handshake_done = true;
        let mut buf = [0u8; 16];
        let n = block_on(session.read(&mut buf)).unwrap();
        assert_eq!(n, 0, "EOF on socket -> read returns 0 bytes");
    }
}
