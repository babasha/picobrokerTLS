pub mod tls;
#[cfg(feature = "embassy-net")]
pub mod tcp;

#[allow(async_fn_in_trait)]
pub trait Transport {
    type Error: core::fmt::Debug;

    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error>;
    async fn write(&mut self, buf: &[u8]) -> Result<(), Self::Error>;
    async fn close(&mut self);
}

#[cfg(test)]
pub mod mock {
    use std::collections::VecDeque;
    use std::vec::Vec;

    #[derive(Debug, Default)]
    pub struct MockTransport {
        pub rx_queue: VecDeque<Vec<u8>>,
        pub tx_log: Vec<Vec<u8>>,
        pub closed: bool,
    }

    impl MockTransport {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn feed(&mut self, data: &[u8]) {
            self.rx_queue.push_back(data.to_vec());
        }
    }

    impl super::Transport for MockTransport {
        type Error = ();

        async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
            let Some(mut chunk) = self.rx_queue.pop_front() else {
                return Ok(0);
            };

            let read_len = core::cmp::min(buf.len(), chunk.len());
            buf[..read_len].copy_from_slice(&chunk[..read_len]);

            if read_len < chunk.len() {
                let rest = chunk.split_off(read_len);
                self.rx_queue.push_front(rest);
            }

            Ok(read_len)
        }

        async fn write(&mut self, buf: &[u8]) -> Result<(), Self::Error> {
            self.tx_log.push(buf.to_vec());
            Ok(())
        }

        async fn close(&mut self) {
            self.closed = true;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::MockTransport;
        use crate::transport::Transport;
        use core::future::Future;
        use core::pin::{pin, Pin};
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

            match Pin::as_mut(&mut future).poll(&mut cx) {
                Poll::Ready(output) => output,
                Poll::Pending => panic!("test future unexpectedly returned Pending"),
            }
        }

        #[test]
        fn feed_then_read_returns_bytes_in_order() {
            let mut transport = MockTransport::new();
            let mut buf = [0u8; 3];

            transport.feed(b"hello");

            let read = block_on(transport.read(&mut buf)).unwrap();
            assert_eq!(read, 3);
            assert_eq!(&buf, b"hel");

            let read = block_on(transport.read(&mut buf)).unwrap();
            assert_eq!(read, 2);
            assert_eq!(&buf[..2], b"lo");
        }

        #[test]
        fn write_appends_to_tx_log() {
            let mut transport = MockTransport::new();

            block_on(transport.write(b"pong")).unwrap();
            block_on(transport.write(b"ack")).unwrap();

            assert_eq!(transport.tx_log.len(), 2);
            assert_eq!(transport.tx_log[0], b"pong");
            assert_eq!(transport.tx_log[1], b"ack");
        }
    }
}
