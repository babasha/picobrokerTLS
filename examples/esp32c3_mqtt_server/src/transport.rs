#![cfg_attr(not(feature = "tls"), allow(dead_code))]

use embassy_net::tcp::{Error, TcpSocket};
#[cfg(feature = "tls")]
use esp_println::println;
use gatomqtt::transport::Transport;

#[cfg(feature = "tls")]
use mbedtls_rs::io::{ErrorType as AsyncErrorType, Read as AsyncRead, Write as AsyncWrite};
#[cfg(feature = "tls")]
use mbedtls_rs::{Session, SessionError, SharedSessionConfig, TlsReference};
#[cfg(feature = "tls")]
const TLS_TRACE_IO: bool = false;

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

    #[cfg(feature = "tls")]
    pub fn socket_mut(&mut self) -> &mut TcpSocket<'sock> {
        self.socket
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

#[cfg(feature = "tls")]
impl<'a, 'sock> AsyncErrorType for TcpTransport<'a, 'sock> {
    type Error = TcpTransportError;
}

#[cfg(feature = "tls")]
impl<'a, 'sock> AsyncRead for TcpTransport<'a, 'sock> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        self.socket.read(buf).await.map_err(TcpTransportError::from)
    }
}

#[cfg(feature = "tls")]
impl<'a, 'sock> AsyncWrite for TcpTransport<'a, 'sock> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        self.socket
            .write(buf)
            .await
            .map_err(TcpTransportError::from)
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[cfg(feature = "tls")]
pub struct TlsTransport<'tls, 'a, 'sock> {
    session: Session<'tls, TcpTransport<'a, 'sock>>,
}

#[cfg(feature = "tls")]
impl<'tls, 'a, 'sock> TlsTransport<'tls, 'a, 'sock> {
    pub fn new(
        tls: TlsReference<'tls>,
        stream: TcpTransport<'a, 'sock>,
        config: &SharedSessionConfig<'tls>,
    ) -> Result<Self, SessionError> {
        Ok(Self {
            session: Session::new_shared(tls, stream, config)?,
        })
    }

    pub async fn handshake(&mut self) -> Result<(), SessionError> {
        let result = self.session.connect().await;
        match &result {
            Ok(()) => {
                if TLS_TRACE_IO {
                    println!("[TLS IO] handshake ok");
                }
            }
            Err(error) => println!("[TLS IO] handshake err: {:?}", error),
        }
        result
    }
}

#[cfg(feature = "tls")]
impl<'tls, 'a, 'sock> Transport for TlsTransport<'tls, 'a, 'sock> {
    type Error = SessionError;

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let result = self.session.read(buf).await;
        match &result {
            Ok(len) if *len == 0 => {
                if TLS_TRACE_IO {
                    println!("[TLS IO] read eof");
                }
            }
            Ok(len) => {
                if TLS_TRACE_IO {
                    println!("[TLS IO] read {}", len);
                }
            }
            Err(error) => println!("[TLS IO] read err: {:?}", error),
        }
        result
    }

    async fn write(&mut self, buf: &[u8]) -> Result<(), Self::Error> {
        let mut written = 0;
        while written < buf.len() {
            let count = match self.session.write(&buf[written..]).await {
                Ok(count) => count,
                Err(error) => {
                    println!("[TLS IO] write err: {:?}", error);
                    return Err(error);
                }
            };
            if count == 0 {
                if TLS_TRACE_IO {
                    println!("[TLS IO] write zero");
                }
                return Err(SessionError::Io(embedded_io::ErrorKind::WriteZero));
            }
            written += count;
        }
        if let Err(error) = self.session.flush().await {
            println!("[TLS IO] flush err: {:?}", error);
            return Err(error);
        }
        if TLS_TRACE_IO {
            println!("[TLS IO] wrote {}", buf.len());
        }
        Ok(())
    }

    async fn close(&mut self) {
        if TLS_TRACE_IO {
            println!("[TLS IO] close");
        }
        let _ = self.session.close().await;
        self.session.stream().socket_mut().close();
    }
}
