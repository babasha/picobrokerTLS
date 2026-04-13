use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::channel::Channel;
use heapless::{String, Vec};
use mqttrs::QoS;

/// A command intent received from an MQTT client on a command topic.
/// Passed to the `CommandHandler` for delivery to the control domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MqttIntent {
    pub topic: String<128>,
    pub payload: Vec<u8, 512>,
    pub qos: QoS,
}

/// Returned by `CommandHandler::handle` when the handler cannot accept the intent.
pub struct CommandOverloaded;

/// Receives command intents from the broker and delivers them to the control domain.
///
/// The broker calls `handle` for every publish on a command topic. The implementation
/// decides what to do: queue, coalesce, prioritise, or reject. If `Overloaded` is
/// returned for a QoS 1 publish, the broker disconnects the client so it can retry.
pub trait CommandHandler {
    fn handle(&self, intent: MqttIntent) -> Result<(), CommandOverloaded>;
}

/// Default implementation: wrap an embassy-sync `Channel`.
/// Works for the common case where the control domain reads from a bounded queue.
impl<M: RawMutex, const N: usize> CommandHandler for Channel<M, MqttIntent, N> {
    fn handle(&self, intent: MqttIntent) -> Result<(), CommandOverloaded> {
        self.try_send(intent).map_err(|_| CommandOverloaded)
    }
}

#[cfg(test)]
pub mod mock {
    use super::{CommandHandler, CommandOverloaded, MqttIntent};
    use core::cell::RefCell;

    pub struct MockCommandHandler {
        pub received: RefCell<std::vec::Vec<MqttIntent>>,
        pub overloaded: bool,
    }

    impl MockCommandHandler {
        pub fn new() -> Self {
            Self {
                received: RefCell::new(std::vec::Vec::new()),
                overloaded: false,
            }
        }

        pub fn new_overloaded() -> Self {
            Self {
                received: RefCell::new(std::vec::Vec::new()),
                overloaded: true,
            }
        }
    }

    impl CommandHandler for MockCommandHandler {
        fn handle(&self, intent: MqttIntent) -> Result<(), CommandOverloaded> {
            if self.overloaded {
                return Err(CommandOverloaded);
            }
            self.received.borrow_mut().push(intent);
            Ok(())
        }
    }
}
