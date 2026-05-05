use embassy_net::tcp::{Error, TcpSocket};
use gatomqtt::transport::Transport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpTransportError {
    Socket(Error),
    WriteZero,
}

impl From<Error> for TcpTransportError {
    fn from(error: Error) -> Self {
        Self::Socket(error)
    }
}

impl core::fmt::Display for TcpTransportError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Socket(error) => write!(f, "socket error: {:?}", error),
            Self::WriteZero => write!(f, "write returned zero"),
        }
    }
}

impl core::error::Error for TcpTransportError {}

impl embedded_io::Error for TcpTransportError {
    fn kind(&self) -> embedded_io::ErrorKind {
        match self {
            Self::WriteZero => embedded_io::ErrorKind::WriteZero,
            Self::Socket(_) => embedded_io::ErrorKind::Other,
        }
    }
}

pub struct TcpTransport<'a, 'sock> {
    socket: &'a mut TcpSocket<'sock>,
}

impl<'a, 'sock> TcpTransport<'a, 'sock> {
    pub fn new(socket: &'a mut TcpSocket<'sock>) -> Self {
        Self { socket }
    }
}

impl<'a, 'sock> Transport for TcpTransport<'a, 'sock> {
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
