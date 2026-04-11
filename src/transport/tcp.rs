use embassy_net::tcp::{Error as TcpError, TcpSocket};

use super::Transport;

/// Error type for [`TcpTransport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpTransportError {
    /// The underlying socket returned an error.
    Socket(TcpError),
    /// A `write` call returned 0 bytes without an error, indicating a stuck or
    /// closed peer.
    WriteZero,
}

impl From<TcpError> for TcpTransportError {
    fn from(error: TcpError) -> Self {
        Self::Socket(error)
    }
}

/// [`Transport`] implementation over an [`embassy_net::tcp::TcpSocket`].
///
/// Takes ownership of the socket for the duration of the connection.  When the
/// broker calls [`Transport::close`], the socket is closed via
/// [`TcpSocket::close`].
///
/// The `write` implementation loops until all bytes have been accepted by the
/// socket's send buffer (embassy-net may accept only a portion of a write call
/// if the buffer is temporarily full).
pub struct TcpTransport<'sock> {
    socket: TcpSocket<'sock>,
}

impl<'sock> TcpTransport<'sock> {
    pub fn new(socket: TcpSocket<'sock>) -> Self {
        Self { socket }
    }

    /// Return a shared reference to the underlying socket (e.g. to call
    /// `remote_endpoint()` for logging before handing off to
    /// `connection_loop`).
    pub fn socket(&self) -> &TcpSocket<'sock> {
        &self.socket
    }
}

impl<'sock> Transport for TcpTransport<'sock> {
    type Error = TcpTransportError;

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.socket.read(buf).await.map_err(TcpTransportError::from)
    }

    async fn write(&mut self, buf: &[u8]) -> Result<(), Self::Error> {
        let mut written = 0;
        while written < buf.len() {
            let count = self
                .socket
                .write(&buf[written..])
                .await
                .map_err(TcpTransportError::from)?;
            if count == 0 {
                return Err(TcpTransportError::WriteZero);
            }
            written += count;
        }
        Ok(())
    }

    async fn close(&mut self) {
        self.socket.close();
    }
}
