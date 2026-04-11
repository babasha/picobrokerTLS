use crate::codec::frame::{write_packet, WriteError};
use crate::session::state::SessionState;
use crate::transport::Transport;
use mqttrs::Packet;

#[derive(Debug, PartialEq)]
pub enum HandlerError<E> {
    Write(WriteError<E>),
}

impl<E> From<WriteError<E>> for HandlerError<E> {
    fn from(value: WriteError<E>) -> Self {
        Self::Write(value)
    }
}

pub async fn handle_pingreq<
    T: Transport,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
    const MAX_PACKET_SIZE: usize,
>(
    transport: &mut T,
    session: &mut SessionState<MAX_SUBS, MAX_INFLIGHT>,
    frame_buf: &mut [u8; MAX_PACKET_SIZE],
) -> Result<(), HandlerError<T::Error>> {
    touch_pingreq(session);
    write_packet(transport, &Packet::Pingresp, frame_buf).await?;
    Ok(())
}

pub fn touch_pingreq<const MAX_SUBS: usize, const MAX_INFLIGHT: usize>(
    session: &mut SessionState<MAX_SUBS, MAX_INFLIGHT>,
) {
    session.update_activity();
}

#[cfg(test)]
mod tests {
    use super::handle_pingreq;
    use crate::session::state::SessionState;
    use crate::transport::mock::MockTransport;
    use core::future::Future;
    use core::pin::{pin, Pin};
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    use embassy_time::Instant;
    use heapless::String;
    use std::vec;

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
    fn pingreq_sends_pingresp_and_updates_activity() {
        let mut transport = MockTransport::new();
        let mut session = SessionState::<32, 16>::new(
            String::<64>::try_from("mobile-app").unwrap(),
            60,
        );
        let mut frame_buf = [0u8; 32];
        let before = Instant::now();

        block_on(handle_pingreq(&mut transport, &mut session, &mut frame_buf)).unwrap();
        let after = Instant::now();

        assert_eq!(transport.tx_log, vec![vec![0xD0, 0x00]]);
        assert!(session.last_activity >= before);
        assert!(session.last_activity <= after);
    }
}
