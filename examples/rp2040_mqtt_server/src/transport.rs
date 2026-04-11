use embassy_net::tcp::{Error, TcpSocket};
use picobroker::transport::Transport;

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

pub struct TcpTransport<'a> {
    socket: TcpSocket<'a>,
}

impl<'a> TcpTransport<'a> {
    pub fn new(socket: TcpSocket<'a>) -> Self {
        Self { socket }
    }

    pub fn into_inner(self) -> TcpSocket<'a> {
        self.socket
    }

    pub fn socket(&self) -> &TcpSocket<'a> {
        &self.socket
    }

    pub fn socket_mut(&mut self) -> &mut TcpSocket<'a> {
        &mut self.socket
    }
}

impl<'a> Transport for TcpTransport<'a> {
    type Error = TcpTransportError;

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.socket.read(buf).await.map_err(Into::into)
    }

    async fn write(&mut self, buf: &[u8]) -> Result<(), Self::Error> {
        let mut written = 0;
        while written < buf.len() {
            let count = self.socket.write(&buf[written..]).await?;
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
