use super::Transport;

/// Abstraction over a TLS session that has completed setup and is ready to
/// exchange application data.
///
/// Implement this trait for your board's TLS library session type
/// (e.g. `embedded_tls::TlsConnection`, `mbedtls_rs::Session`, …) and wrap it
/// in [`TlsTransport`] to obtain a [`Transport`] that `connection_loop` accepts.
///
/// The split between session setup (constructor / `accept`) and I/O
/// (`read`/`write`/`flush`/`close`) is intentional: the handshake happens
/// outside `connection_loop`; once it succeeds you hand the ready transport to
/// the broker.
#[allow(async_fn_in_trait)]
pub trait TlsSession {
    type Error: core::fmt::Debug;

    /// Perform the server-side TLS handshake.
    ///
    /// Call this once after the TCP connection is accepted, before passing the
    /// transport to `connection_loop`.  On failure, drop the transport (which
    /// must close the underlying socket in its `Drop` or via `close()`).
    async fn accept(&mut self) -> Result<(), Self::Error>;

    /// Read decrypted application data from the session.
    ///
    /// Returns the number of bytes placed in `buf`, or 0 on clean EOF.
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error>;

    /// Write up to `buf.len()` bytes of plaintext. Returns the number of bytes
    /// consumed.  The session may buffer internally; call `flush` to push data.
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error>;

    /// Flush any internally buffered outbound data.
    async fn flush(&mut self) -> Result<(), Self::Error>;

    /// Send TLS `close_notify` and close the underlying stream.
    async fn close(&mut self);
}

/// Error type for [`TlsTransport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsTransportError<E: core::fmt::Debug> {
    /// The underlying TLS session returned an error.
    Tls(E),
    /// A `write` call returned 0 bytes written without an error.
    WriteZero,
}

/// Wraps a [`TlsSession`] and implements [`Transport`].
///
/// # Usage
///
/// ```ignore
/// let tcp = TcpSocket::new(...);
/// let mut session = MyTlsSession::new(tcp, &config);
/// let mut transport = TlsTransport::new(session);
///
/// // Handshake before handing off to the broker.
/// transport.accept().await?;
///
/// connection_loop(&mut transport, &registry, &retained, &inbound, &config, &mut frame_buf).await;
/// ```
pub struct TlsTransport<S> {
    session: S,
}

impl<S: TlsSession> TlsTransport<S> {
    pub fn new(session: S) -> Self {
        Self { session }
    }

    /// Perform the TLS server-side handshake.
    ///
    /// Must complete successfully before the transport is passed to
    /// `connection_loop`.  On error, call [`Transport::close`] to release the
    /// underlying socket.
    pub async fn accept(&mut self) -> Result<(), S::Error> {
        self.session.accept().await
    }
}

impl<S: TlsSession> Transport for TlsTransport<S> {
    type Error = TlsTransportError<S::Error>;

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.session.read(buf).await.map_err(TlsTransportError::Tls)
    }

    /// Writes all of `buf` through the TLS session, flushing once at the end.
    ///
    /// Returns [`TlsTransportError::WriteZero`] if the session accepts zero
    /// bytes without signalling an error (which indicates a closed or stuck
    /// peer and should be treated as a disconnect).
    async fn write(&mut self, buf: &[u8]) -> Result<(), Self::Error> {
        let mut written = 0;
        while written < buf.len() {
            let count = self
                .session
                .write(&buf[written..])
                .await
                .map_err(TlsTransportError::Tls)?;
            if count == 0 {
                return Err(TlsTransportError::WriteZero);
            }
            written += count;
        }
        self.session.flush().await.map_err(TlsTransportError::Tls)?;
        Ok(())
    }

    async fn close(&mut self) {
        self.session.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::{TlsSession, TlsTransport, TlsTransportError};
    use crate::transport::Transport;
    use core::future::Future;
    use core::pin::{pin, Pin};
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    use std::collections::VecDeque;

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
            Poll::Ready(out) => out,
            Poll::Pending => panic!("future unexpectedly pending"),
        }
    }

    /// A [`TlsSession`] stand-in that exercises every control path.
    struct MockTlsSession {
        rx: VecDeque<std::vec::Vec<u8>>,
        tx: std::vec::Vec<std::vec::Vec<u8>>,
        accepted: bool,
        closed: bool,
        /// When `Some(n)`, each `write` call returns `n` bytes consumed.
        /// When `None`, returns the full slice length (normal case).
        write_chunk: Option<usize>,
        fail_accept: bool,
        fail_read: bool,
        fail_write: bool,
        fail_flush: bool,
    }

    impl MockTlsSession {
        fn new() -> Self {
            Self {
                rx: VecDeque::new(),
                tx: std::vec::Vec::new(),
                accepted: false,
                closed: false,
                write_chunk: None,
                fail_accept: false,
                fail_read: false,
                fail_write: false,
                fail_flush: false,
            }
        }

        fn feed(&mut self, data: &[u8]) {
            self.rx.push_back(data.to_vec());
        }
    }

    impl TlsSession for MockTlsSession {
        type Error = &'static str;

        async fn accept(&mut self) -> Result<(), Self::Error> {
            if self.fail_accept {
                return Err("accept failed");
            }
            self.accepted = true;
            Ok(())
        }

        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            if self.fail_read {
                return Err("read failed");
            }
            let Some(mut chunk) = self.rx.pop_front() else {
                return Ok(0);
            };
            let read_len = core::cmp::min(buf.len(), chunk.len());
            buf[..read_len].copy_from_slice(&chunk[..read_len]);
            if read_len < chunk.len() {
                let rest = chunk.split_off(read_len);
                self.rx.push_front(rest);
            }
            Ok(read_len)
        }

        async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
            if self.fail_write {
                return Err("write failed");
            }
            let consumed = match self.write_chunk {
                Some(n) => core::cmp::min(n, buf.len()),
                None => buf.len(),
            };
            if consumed > 0 {
                self.tx.push(buf[..consumed].to_vec());
            }
            Ok(consumed)
        }

        async fn flush(&mut self) -> Result<(), Self::Error> {
            if self.fail_flush {
                return Err("flush failed");
            }
            Ok(())
        }

        async fn close(&mut self) {
            self.closed = true;
        }
    }

    #[test]
    fn accept_delegates_to_session_and_sets_accepted_flag() {
        let mut transport = TlsTransport::new(MockTlsSession::new());

        block_on(transport.accept()).unwrap();

        assert!(transport.session.accepted);
    }

    #[test]
    fn accept_propagates_session_error() {
        let mut session = MockTlsSession::new();
        session.fail_accept = true;
        let mut transport = TlsTransport::new(session);

        let err = block_on(transport.accept()).unwrap_err();

        assert_eq!(err, "accept failed");
    }

    #[test]
    fn read_delegates_to_session() {
        let mut session = MockTlsSession::new();
        session.feed(b"hello");
        let mut transport = TlsTransport::new(session);
        let mut buf = [0u8; 5];

        let n = block_on(transport.read(&mut buf)).unwrap();

        assert_eq!(n, 5);
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn read_returns_zero_on_eof() {
        let transport = TlsTransport::new(MockTlsSession::new());
        let mut transport = transport;
        let mut buf = [0u8; 4];

        let n = block_on(transport.read(&mut buf)).unwrap();

        assert_eq!(n, 0);
    }

    #[test]
    fn read_maps_session_error_to_tls_error() {
        let mut session = MockTlsSession::new();
        session.fail_read = true;
        let mut transport = TlsTransport::new(session);
        let mut buf = [0u8; 4];

        let err = block_on(transport.read(&mut buf)).unwrap_err();

        assert_eq!(err, TlsTransportError::Tls("read failed"));
    }

    #[test]
    fn write_sends_all_bytes_in_one_chunk_and_flushes() {
        let mut transport = TlsTransport::new(MockTlsSession::new());

        block_on(transport.write(b"hello")).unwrap();

        assert_eq!(transport.session.tx.len(), 1);
        assert_eq!(transport.session.tx[0], b"hello");
    }

    #[test]
    fn write_loops_when_session_write_returns_partial_bytes() {
        let mut session = MockTlsSession::new();
        session.write_chunk = Some(3); // only 3 bytes per call
        let mut transport = TlsTransport::new(session);

        block_on(transport.write(b"hello")).unwrap();

        // "hel" then "lo" — two chunks
        let all: std::vec::Vec<u8> = transport.session.tx.concat();
        assert_eq!(all, b"hello");
    }

    #[test]
    fn write_returns_write_zero_when_session_accepts_zero_bytes() {
        let mut session = MockTlsSession::new();
        session.write_chunk = Some(0);
        let mut transport = TlsTransport::new(session);

        let err = block_on(transport.write(b"hello")).unwrap_err();

        assert_eq!(err, TlsTransportError::WriteZero);
    }

    #[test]
    fn write_maps_session_write_error_to_tls_error() {
        let mut session = MockTlsSession::new();
        session.fail_write = true;
        let mut transport = TlsTransport::new(session);

        let err = block_on(transport.write(b"hello")).unwrap_err();

        assert_eq!(err, TlsTransportError::Tls("write failed"));
    }

    #[test]
    fn write_maps_flush_error_to_tls_error() {
        let mut session = MockTlsSession::new();
        session.fail_flush = true;
        let mut transport = TlsTransport::new(session);

        let err = block_on(transport.write(b"hello")).unwrap_err();

        assert_eq!(err, TlsTransportError::Tls("flush failed"));
    }

    #[test]
    fn close_delegates_to_session() {
        let mut transport = TlsTransport::new(MockTlsSession::new());

        block_on(transport.close());

        assert!(transport.session.closed);
    }
}
