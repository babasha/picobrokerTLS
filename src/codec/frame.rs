use crate::transport::Transport;

#[derive(Debug, PartialEq, Eq)]
pub enum ReadError<E> {
    Eof,
    PacketTooLarge { packet_size: usize, buffer_size: usize },
    Transport(E),
    Protocol(mqttrs::Error),
}

#[derive(Debug, PartialEq, Eq)]
pub enum WriteError<E> {
    Transport(E),
    Protocol(mqttrs::Error),
}

impl<E> From<mqttrs::Error> for ReadError<E> {
    fn from(value: mqttrs::Error) -> Self {
        Self::Protocol(value)
    }
}

impl<E> From<mqttrs::Error> for WriteError<E> {
    fn from(value: mqttrs::Error) -> Self {
        Self::Protocol(value)
    }
}

pub async fn read_packet<'a, T: Transport, const MAX_PACKET_SIZE: usize>(
    transport: &mut T,
    buf: &'a mut [u8; MAX_PACKET_SIZE],
) -> Result<mqttrs::Packet<'a>, ReadError<T::Error>> {
    read_exact(transport, &mut buf[..2]).await?;

    let mut header_len = 2usize;
    while (buf[header_len - 1] & 0x80) != 0 {
        if header_len >= 5 {
            return Err(ReadError::Protocol(mqttrs::Error::InvalidHeader));
        }

        read_exact(transport, &mut buf[header_len..header_len + 1]).await?;
        header_len += 1;
    }

    let remaining_length = decode_remaining_length(&buf[1..header_len])?;
    let total_len = header_len + remaining_length;

    if total_len > buf.len() {
        return Err(ReadError::PacketTooLarge {
            packet_size: total_len,
            buffer_size: buf.len(),
        });
    }

    read_exact(transport, &mut buf[header_len..total_len]).await?;

    match mqttrs::decode_slice(&buf[..total_len]).map_err(ReadError::Protocol)? {
        Some(packet) => Ok(packet),
        None => Err(ReadError::Protocol(mqttrs::Error::InvalidLength)),
    }
}

pub async fn write_packet<T: Transport, const MAX_PACKET_SIZE: usize>(
    transport: &mut T,
    packet: &mqttrs::Packet<'_>,
    buf: &mut [u8; MAX_PACKET_SIZE],
) -> Result<(), WriteError<T::Error>> {
    let len = mqttrs::encode_slice(packet, buf).map_err(WriteError::Protocol)?;
    transport
        .write(&buf[..len])
        .await
        .map_err(WriteError::Transport)
}

fn decode_remaining_length(bytes: &[u8]) -> Result<usize, mqttrs::Error> {
    let mut multiplier = 1usize;
    let mut value = 0usize;

    for (index, byte) in bytes.iter().copied().enumerate() {
        value += ((byte & 0x7F) as usize) * multiplier;

        if (byte & 0x80) == 0 {
            return Ok(value);
        }

        multiplier *= 128;
        if index == 3 || multiplier > 128 * 128 * 128 * 128 {
            return Err(mqttrs::Error::InvalidHeader);
        }
    }

    Err(mqttrs::Error::InvalidHeader)
}

async fn read_exact<T: Transport>(
    transport: &mut T,
    buf: &mut [u8],
) -> Result<(), ReadError<T::Error>> {
    let mut offset = 0usize;

    while offset < buf.len() {
        let read = transport
            .read(&mut buf[offset..])
            .await
            .map_err(ReadError::Transport)?;

        if read == 0 {
            return Err(ReadError::Eof);
        }

        offset += read;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{read_packet, write_packet, ReadError};
    use crate::transport::mock::MockTransport;
    use core::convert::TryFrom;
    use core::future::Future;
    use core::pin::{pin, Pin};
    use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
    use mqttrs::{
        Connect, ConnectReturnCode, Connack, Packet, Pid, Protocol, Publish, QosPid, QoS,
    };
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

    fn encode_packet(packet: &Packet<'_>) -> std::vec::Vec<u8> {
        let mut buf = [0u8; 1024];
        let len = mqttrs::encode_slice(packet, &mut buf).unwrap();
        buf[..len].to_vec()
    }

    fn decode_packet<'a>(bytes: &'a [u8]) -> Packet<'a> {
        mqttrs::decode_slice(bytes).unwrap().unwrap()
    }

    fn assert_round_trip(packet: Packet<'_>) {
        let mut writer = MockTransport::new();
        let mut write_buf = [0u8; 512];
        block_on(write_packet(&mut writer, &packet, &mut write_buf)).unwrap();

        let encoded = writer.tx_log[0].clone();
        let mut reader = MockTransport::new();
        reader.feed(&encoded);

        let mut read_buf = [0u8; 512];
        let decoded = block_on(read_packet::<MockTransport, 512>(&mut reader, &mut read_buf))
            .unwrap();

        assert_eq!(decoded, packet);
    }

    #[test]
    fn read_connect_minimal_packet() {
        let expected = Packet::Connect(Connect {
            protocol: Protocol::MQTT311,
            keep_alive: 30,
            client_id: "cid",
            clean_session: true,
            last_will: None,
            username: None,
            password: None,
        });

        let encoded = encode_packet(&expected);
        let mut transport = MockTransport::new();
        transport.feed(&encoded);

        let mut frame_buf = [0u8; 512];
        let decoded = block_on(read_packet::<MockTransport, 512>(&mut transport, &mut frame_buf))
            .unwrap();

        assert_eq!(decoded, expected);
    }

    #[test]
    fn read_connect_with_username_and_password() {
        let expected = Packet::Connect(Connect {
            protocol: Protocol::MQTT311,
            keep_alive: 45,
            client_id: "client-1",
            clean_session: true,
            last_will: None,
            username: Some("user"),
            password: Some(b"secret"),
        });

        let encoded = encode_packet(&expected);
        let mut transport = MockTransport::new();
        transport.feed(&encoded);

        let mut frame_buf = [0u8; 512];
        let decoded = block_on(read_packet::<MockTransport, 512>(&mut transport, &mut frame_buf))
            .unwrap();

        match decoded {
            Packet::Connect(connect) => {
                assert_eq!(connect.username, Some("user"));
                assert_eq!(connect.password, Some(b"secret".as_ref()));
                assert_eq!(connect.client_id, "client-1");
            }
            other => panic!("expected CONNECT, got {:?}", other),
        }
    }

    #[test]
    fn read_publish_qos0_packet() {
        let expected = Packet::Publish(Publish {
            dup: false,
            qospid: QosPid::AtMostOnce,
            retain: false,
            topic_name: "test",
            payload: b"hello",
        });

        let encoded = encode_packet(&expected);
        let mut transport = MockTransport::new();
        transport.feed(&encoded);

        let mut frame_buf = [0u8; 128];
        let decoded = block_on(read_packet::<MockTransport, 128>(&mut transport, &mut frame_buf))
            .unwrap();

        assert_eq!(decoded, expected);
    }

    #[test]
    fn read_publish_qos1_packet() {
        let expected = Packet::Publish(Publish {
            dup: false,
            qospid: QosPid::AtLeastOnce(Pid::try_from(1).unwrap()),
            retain: false,
            topic_name: "test",
            payload: b"hello",
        });

        let encoded = encode_packet(&expected);
        let mut transport = MockTransport::new();
        transport.feed(&encoded);

        let mut frame_buf = [0u8; 128];
        let decoded = block_on(read_packet::<MockTransport, 128>(&mut transport, &mut frame_buf))
            .unwrap();

        assert_eq!(decoded, expected);
    }

    #[test]
    fn read_subscribe_single_topic_qos0() {
        let encoded = [
            0x82, 0x09, 0x00, 0x01, 0x00, 0x04, b't', b'e', b's', b't', 0x00,
        ];
        let mut transport = MockTransport::new();
        transport.feed(&encoded);

        let mut frame_buf = [0u8; 128];
        let decoded = block_on(read_packet::<MockTransport, 128>(&mut transport, &mut frame_buf))
            .unwrap();

        match decoded {
            Packet::Subscribe(subscribe) => {
                assert_eq!(subscribe.pid.get(), 1);
                assert_eq!(subscribe.topics.len(), 1);
                assert_eq!(subscribe.topics[0].topic_path.as_str(), "test");
                assert_eq!(subscribe.topics[0].qos, QoS::AtMostOnce);
            }
            other => panic!("expected SUBSCRIBE, got {:?}", other),
        }
    }

    #[test]
    fn read_pingreq_packet() {
        let mut transport = MockTransport::new();
        transport.feed(&[0xC0, 0x00]);

        let mut frame_buf = [0u8; 16];
        let decoded = block_on(read_packet::<MockTransport, 16>(&mut transport, &mut frame_buf))
            .unwrap();

        assert_eq!(decoded, Packet::Pingreq);
    }

    #[test]
    fn read_disconnect_packet() {
        let mut transport = MockTransport::new();
        transport.feed(&[0xE0, 0x00]);

        let mut frame_buf = [0u8; 16];
        let decoded = block_on(read_packet::<MockTransport, 16>(&mut transport, &mut frame_buf))
            .unwrap();

        assert_eq!(decoded, Packet::Disconnect);
    }

    #[test]
    fn read_empty_transport_returns_eof() {
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; 16];

        let err =
            block_on(read_packet::<MockTransport, 16>(&mut transport, &mut frame_buf)).unwrap_err();

        assert_eq!(err, ReadError::Eof);
    }

    #[test]
    fn read_truncated_packet_returns_eof() {
        let packet = Packet::Connect(Connect {
            protocol: Protocol::MQTT311,
            keep_alive: 30,
            client_id: "cid",
            clean_session: true,
            last_will: None,
            username: None,
            password: None,
        });
        let encoded = encode_packet(&packet);

        let mut transport = MockTransport::new();
        transport.feed(&encoded[..encoded.len() - 1]);

        let mut frame_buf = [0u8; 128];
        let err =
            block_on(read_packet::<MockTransport, 128>(&mut transport, &mut frame_buf)).unwrap_err();

        assert_eq!(err, ReadError::Eof);
    }

    #[test]
    fn read_packet_too_large_returns_error() {
        let mut transport = MockTransport::new();
        transport.feed(&[0x30, 0xB0, 0x09]);

        let mut frame_buf = [0u8; 512];
        let err =
            block_on(read_packet::<MockTransport, 512>(&mut transport, &mut frame_buf)).unwrap_err();

        assert_eq!(
            err,
            ReadError::PacketTooLarge {
                packet_size: 1203,
                buffer_size: 512,
            }
        );
    }

    #[test]
    fn read_multibyte_remaining_length_packet() {
        let payload = [b'a'; 200];
        let packet = Packet::Publish(Publish {
            dup: false,
            qospid: QosPid::AtMostOnce,
            retain: false,
            topic_name: "big/topic",
            payload: &payload,
        });
        let encoded = encode_packet(&packet);

        let mut transport = MockTransport::new();
        transport.feed(&encoded);

        let mut frame_buf = [0u8; 512];
        let decoded = block_on(read_packet::<MockTransport, 512>(&mut transport, &mut frame_buf))
            .unwrap();

        match decoded {
            Packet::Publish(publish) => {
                assert_eq!(publish.topic_name, "big/topic");
                assert_eq!(publish.payload.len(), 200);
                assert!(publish.payload.iter().all(|byte| *byte == b'a'));
            }
            other => panic!("expected PUBLISH, got {:?}", other),
        }
    }

    #[test]
    fn write_connack_packet() {
        let packet = Packet::Connack(Connack {
            session_present: false,
            code: ConnectReturnCode::Accepted,
        });

        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; 16];
        block_on(write_packet(&mut transport, &packet, &mut frame_buf)).unwrap();

        assert_eq!(transport.tx_log, vec![vec![0x20, 0x02, 0x00, 0x00]]);
    }

    #[test]
    fn write_connack_bad_username_password_packet() {
        let packet = Packet::Connack(Connack {
            session_present: false,
            code: ConnectReturnCode::BadUsernamePassword,
        });

        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; 16];
        block_on(write_packet(&mut transport, &packet, &mut frame_buf)).unwrap();

        assert_eq!(transport.tx_log, vec![vec![0x20, 0x02, 0x00, 0x04]]);
    }

    #[test]
    fn write_suback_packet() {
        let packet = decode_packet(&[0x90, 0x03, 0x00, 0x01, 0x00]);

        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; 16];
        block_on(write_packet(&mut transport, &packet, &mut frame_buf)).unwrap();

        assert_eq!(transport.tx_log, vec![vec![0x90, 0x03, 0x00, 0x01, 0x00]]);
    }

    #[test]
    fn write_puback_packet() {
        let packet = Packet::Puback(Pid::try_from(5).unwrap());

        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; 16];
        block_on(write_packet(&mut transport, &packet, &mut frame_buf)).unwrap();

        assert_eq!(transport.tx_log, vec![vec![0x40, 0x02, 0x00, 0x05]]);
    }

    #[test]
    fn write_pingresp_packet() {
        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; 16];
        block_on(write_packet(&mut transport, &Packet::Pingresp, &mut frame_buf)).unwrap();

        assert_eq!(transport.tx_log, vec![vec![0xD0, 0x00]]);
    }

    #[test]
    fn write_publish_retain_packet_sets_retain_bit() {
        let packet = Packet::Publish(Publish {
            dup: false,
            qospid: QosPid::AtMostOnce,
            retain: true,
            topic_name: "test",
            payload: b"hello",
        });

        let mut transport = MockTransport::new();
        let mut frame_buf = [0u8; 64];
        block_on(write_packet(&mut transport, &packet, &mut frame_buf)).unwrap();

        assert_eq!(transport.tx_log[0][0], 0x31);
    }

    #[test]
    fn round_trip_connect_packet() {
        assert_round_trip(Packet::Connect(Connect {
            protocol: Protocol::MQTT311,
            keep_alive: 30,
            client_id: "cid",
            clean_session: true,
            last_will: None,
            username: Some("user"),
            password: Some(b"pw"),
        }));
    }

    #[test]
    fn round_trip_connack_packet() {
        assert_round_trip(Packet::Connack(Connack {
            session_present: false,
            code: ConnectReturnCode::Accepted,
        }));
    }

    #[test]
    fn round_trip_publish_qos0_packet() {
        assert_round_trip(Packet::Publish(Publish {
            dup: false,
            qospid: QosPid::AtMostOnce,
            retain: false,
            topic_name: "topic/a",
            payload: b"hello",
        }));
    }

    #[test]
    fn round_trip_publish_qos1_packet() {
        assert_round_trip(Packet::Publish(Publish {
            dup: false,
            qospid: QosPid::AtLeastOnce(Pid::try_from(7).unwrap()),
            retain: false,
            topic_name: "topic/b",
            payload: b"hello",
        }));
    }

    #[test]
    fn round_trip_subscribe_packet() {
        assert_round_trip(decode_packet(&[
            0x82, 0x09, 0x00, 0x01, 0x00, 0x04, b't', b'e', b's', b't', 0x00,
        ]));
    }

    #[test]
    fn round_trip_suback_packet() {
        assert_round_trip(decode_packet(&[0x90, 0x03, 0x00, 0x01, 0x00]));
    }

    #[test]
    fn round_trip_puback_packet() {
        assert_round_trip(Packet::Puback(Pid::try_from(5).unwrap()));
    }

    #[test]
    fn round_trip_pingreq_packet() {
        assert_round_trip(Packet::Pingreq);
    }

    #[test]
    fn round_trip_pingresp_packet() {
        assert_round_trip(Packet::Pingresp);
    }

    #[test]
    fn round_trip_disconnect_packet() {
        assert_round_trip(Packet::Disconnect);
    }
}
