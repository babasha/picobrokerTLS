#![no_std]
#![no_main]
#![allow(async_fn_in_trait)]

use cyw43_pio::{PioSpi, DEFAULT_CLOCK_DIVIDER};
use defmt::info;
use embassy_executor::Spawner;
use embassy_net::{Config, Stack, StackResources};
use embassy_rp::gpio::Output;
use embassy_rp::interrupt::typelevel::Binding;
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use gato_embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex as GatoCriticalSectionRawMutex;
use gato_embassy_sync::mutex::Mutex as GatoMutex;
use heapless::{String as HString, Vec as HVec};
use picobroker::broker::PicoBroker;
use picobroker::config::GATOMQTT_CONFIG;
use picobroker::handler::connection::connection_loop as gatomqtt_connection_loop;
use picobroker::handler::publish::encode_publish_qos0;
use picobroker::handler::publish::InboundQueue as GatoInboundQueue;
use picobroker::router::{find_subscribers, RetainedStore as GatoRetainedStore};
use picobroker::session::registry::SessionRegistry as GatoSessionRegistry;
use picobroker::session::state::OutboundPacket;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

// Modules (private)
mod handler;
mod io;
mod server;
mod state;
mod transport;

// Public exports
pub use handler::{handle_connection, HandlerConfig};
pub use server::{MqttServer, MqttServerConfig};
pub use state::{current_time_nanos, NotificationRegistry, SessionIdGen};
pub use transport::{TcpTransport, TcpTransportError};

// =============================================================================
// Convenience Type Aliases
// =============================================================================
//
// These aliases take a mutex type parameter (must implement `RawMutex`).
//
// Example:
// ```
// use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
// type MyMqttServer = DefaultMqttServer<CriticalSectionRawMutex>;
// ```

/// Default MQTT server type with specified mutex.
/// Usage: `DefaultMqttServer<CriticalSectionRawMutex>`
pub type DefaultMqttServer<M> = MqttServer<M, 64, 256, 8, 4, 32, 8>;

/// Small MQTT server for embedded/constrained environments with specified mutex.
/// Usage: `SmallMqttServer<CriticalSectionRawMutex>`
pub type SmallMqttServer<M> = MqttServer<M, 32, 128, 4, 2, 16, 4>;

/// Large MQTT server for higher capacity with specified mutex.
/// Usage: `LargeMqttServer<CriticalSectionRawMutex>`
pub type LargeMqttServer<M> = MqttServer<M, 128, 1024, 16, 16, 128, 32>;

embassy_rp::bind_interrupts!(pub struct Irqs {
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<embassy_rp::peripherals::PIO0>;
});

const CYW43_FW: &[u8] = include_bytes!("./cyw43-firmware/43439A0.bin");
const CYW43_CLM: &[u8] = include_bytes!("./cyw43-firmware/43439A0_clm.bin");

const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PASSWORD: &str = env!("WIFI_PASSWORD");

// MQTT Broker Configuration
const MAX_TOPIC_NAME_LENGTH: usize = 20;
const MAX_PAYLOAD_SIZE: usize = 128;
const QUEUE_SIZE: usize = 5;
const MAX_SESSIONS: usize = 4;
const MAX_TOPICS: usize = 8;
const MAX_SUBSCRIBERS_PER_TOPIC: usize = 4;
const MQTT_PORT: u16 = 1883;

const GATO_MAX_SESSIONS: usize = 8;
const GATO_MAX_SUBS: usize = 32;
const GATO_MAX_INFLIGHT: usize = 16;
const GATO_MAX_RETAINED: usize = 64;
const GATO_INBOUND_N: usize = 8;
const GATO_OUTBOUND_N: usize = 8;
const GATO_MAX_PACKET_SIZE: usize = 512;
const GATO_TCP_BUFFER_SIZE: usize = 4096;
const GATO_MQTT_PORT: u16 = 1883;

// Type aliases for broker
type MqttBroker = PicoBroker<
    MAX_TOPIC_NAME_LENGTH,
    MAX_PAYLOAD_SIZE,
    QUEUE_SIZE,
    MAX_SESSIONS,
    MAX_TOPICS,
    MAX_SUBSCRIBERS_PER_TOPIC,
>;
type BrokerMutex = Mutex<CriticalSectionRawMutex, MqttBroker>;

// Static storage (required by Embassy)
static BROKER_CELL: StaticCell<BrokerMutex> = StaticCell::new();
static NOTIFICATION_REGISTRY: StaticCell<
    NotificationRegistry<MAX_SESSIONS, CriticalSectionRawMutex>,
> = StaticCell::new();
static SESSION_ID_GEN_CELL: StaticCell<Mutex<CriticalSectionRawMutex, SessionIdGen>> =
    StaticCell::new();
static GATO_REGISTRY_CELL: StaticCell<
    GatoMutex<
        GatoCriticalSectionRawMutex,
        GatoSessionRegistry<GATO_MAX_SESSIONS, GATO_MAX_SUBS, GATO_MAX_INFLIGHT>,
    >,
> = StaticCell::new();
static GATO_RETAINED_CELL: StaticCell<
    GatoMutex<GatoCriticalSectionRawMutex, GatoRetainedStore<GATO_MAX_RETAINED>>,
> = StaticCell::new();
static GATO_INBOUND_Q_CELL: StaticCell<
    GatoMutex<GatoCriticalSectionRawMutex, GatoInboundQueue<GATO_INBOUND_N>>,
> = StaticCell::new();
static GATO_OUTBOUND_Q_CELL: StaticCell<
    Channel<CriticalSectionRawMutex, OutboundPublish, GATO_OUTBOUND_N>,
> = StaticCell::new();

#[derive(Debug, Clone)]
struct OutboundPublish {
    topic: HString<128>,
    payload: HVec<u8, 512>,
    retain: bool,
}

pub struct WifiPins {
    pub pwr: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_23>,
    pub cs: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_25>,
    pub clk: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_29>,
    pub dio: embassy_rp::Peri<'static, embassy_rp::peripherals::PIN_24>,
    pub pio: embassy_rp::Peri<'static, embassy_rp::peripherals::PIO0>,
    pub dma: embassy_rp::Peri<'static, embassy_rp::peripherals::DMA_CH0>,
}

pub struct PioKeepalive<'a> {
    _common: embassy_rp::pio::Common<'a, embassy_rp::peripherals::PIO0>,
    _irq_flags: embassy_rp::pio::IrqFlags<'a, embassy_rp::peripherals::PIO0>,
    _irq1: embassy_rp::pio::Irq<'a, embassy_rp::peripherals::PIO0, 1>,
    _irq2: embassy_rp::pio::Irq<'a, embassy_rp::peripherals::PIO0, 2>,
    _irq3: embassy_rp::pio::Irq<'a, embassy_rp::peripherals::PIO0, 3>,
    _sm1: embassy_rp::pio::StateMachine<'a, embassy_rp::peripherals::PIO0, 1>,
    _sm2: embassy_rp::pio::StateMachine<'a, embassy_rp::peripherals::PIO0, 2>,
    _sm3: embassy_rp::pio::StateMachine<'a, embassy_rp::peripherals::PIO0, 3>,
}

pub struct WifiManager {
    pub control: cyw43::Control<'static>,
    pub stack: &'static Stack<'static>,
    _pio_keepalive: PioKeepalive<'static>,
}

impl WifiManager {
    pub async fn init_cyw43(
        pins: WifiPins,
        irqs: impl Binding<
            embassy_rp::interrupt::typelevel::PIO0_IRQ_0,
            InterruptHandler<embassy_rp::peripherals::PIO0>,
        >,
        power_mode: cyw43::PowerManagementMode,
        spawner: embassy_executor::Spawner,
    ) -> (
        cyw43::Control<'static>,
        cyw43::NetDriver<'static>,
        PioKeepalive<'static>,
    ) {
        let pwr = Output::new(pins.pwr, embassy_rp::gpio::Level::Low);
        let cs = Output::new(pins.cs, embassy_rp::gpio::Level::High);

        let mut pio = Pio::new(pins.pio, irqs);
        let spi = PioSpi::new(
            &mut pio.common,
            pio.sm0,
            DEFAULT_CLOCK_DIVIDER,
            pio.irq0,
            cs,
            pins.dio,
            pins.clk,
            pins.dma,
        );
        let pio_keepalive = PioKeepalive {
            _common: pio.common,
            _irq_flags: pio.irq_flags,
            _irq1: pio.irq1,
            _irq2: pio.irq2,
            _irq3: pio.irq3,
            _sm1: pio.sm1,
            _sm2: pio.sm2,
            _sm3: pio.sm3,
        };

        static STATE: StaticCell<cyw43::State> = StaticCell::new();
        let state = STATE.init(cyw43::State::new());
        let (net_device, mut control, runner) = cyw43::new(state, pwr, spi, CYW43_FW).await;

        let _ = spawner.spawn(cyw43_runner_task(runner));

        control.init(CYW43_CLM).await;
        control.set_power_management(power_mode).await;

        (control, net_device, pio_keepalive)
    }

    pub fn new(
        control: cyw43::Control<'static>,
        stack: &'static Stack<'static>,
        pio_keepalive: PioKeepalive<'static>,
    ) -> Self {
        Self {
            control,
            stack,
            _pio_keepalive: pio_keepalive,
        }
    }

    pub async fn join_network(&mut self, wifi_ssid: &str, wifi_password: &str) {
        loop {
            match self
                .control
                .join(wifi_ssid, cyw43::JoinOptions::new(wifi_password.as_bytes()))
                .await
            {
                Ok(()) => break,
                Err(_) => {
                    defmt::warn!("WiFi join failed, retrying...");
                }
            }
        }
        self.stack.wait_link_up().await;
        self.stack.wait_config_up().await;
    }
}

#[embassy_executor::task]
async fn cyw43_runner_task(
    runner: cyw43::Runner<
        'static,
        Output<'static>,
        PioSpi<'static, embassy_rp::peripherals::PIO0, 0, embassy_rp::peripherals::DMA_CH0>,
    >,
) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_runner_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

// =============================================================================
// Cleanup Task
// =============================================================================

#[embassy_executor::task]
async fn cleanup_task(broker: &'static BrokerMutex) {
    defmt::info!("Cleanup task started");

    loop {
        embassy_time::Timer::after(embassy_time::Duration::from_secs(5)).await;

        let current_time = current_time_nanos();
        let mut broker = broker.lock().await;

        // Clean expired sessions
        let expired: heapless::Vec<u128, 4> = broker
            .get_expired_sessions(current_time)
            .into_iter()
            .flatten()
            .map(|info| info.session_id)
            .collect();

        for session_id in expired {
            defmt::info!("Cleaning up expired session {}", session_id);
            broker.remove_session(session_id);
        }

        // Clean disconnected sessions
        let disconnected: heapless::Vec<u128, 4> = broker
            .get_disconnected_sessions()
            .into_iter()
            .flatten()
            .collect();

        for session_id in disconnected {
            defmt::info!("Cleaning up disconnected session {}", session_id);
            broker.remove_session(session_id);
        }
    }
}

#[embassy_executor::task]
async fn gatomqtt_inbound_logger_task(
    inbound_queue: &'static GatoMutex<GatoCriticalSectionRawMutex, GatoInboundQueue<GATO_INBOUND_N>>,
    outbound_queue: &'static Channel<CriticalSectionRawMutex, OutboundPublish, GATO_OUTBOUND_N>,
) {
    loop {
        let maybe_intent = {
            let mut inbound_queue = inbound_queue.lock().await;
            inbound_queue.try_recv()
        };

        if let Some(intent) = maybe_intent {
            defmt::info!(
                "GatoMQTT inbound topic={} payload_len={=usize}",
                intent.topic.as_str(),
                intent.payload.len()
            );

            if let Some(state_topic) = map_set_topic_to_state(intent.topic.as_str()) {
                let mut payload = HVec::<u8, 512>::new();
                let _ = payload.extend_from_slice(intent.payload.as_slice());
                let outbound = OutboundPublish {
                    topic: state_topic,
                    payload,
                    retain: true,
                };

                if outbound_queue.try_send(outbound).is_err() {
                    defmt::warn!("GatoMQTT outbound queue full; dropping state echo");
                }
            }
        } else {
            embassy_time::Timer::after(embassy_time::Duration::from_millis(50)).await;
        }
    }
}

#[embassy_executor::task]
async fn mqtt_publish_task(
    registry: &'static GatoMutex<
        GatoCriticalSectionRawMutex,
        GatoSessionRegistry<GATO_MAX_SESSIONS, GATO_MAX_SUBS, GATO_MAX_INFLIGHT>,
    >,
    retained: &'static GatoMutex<GatoCriticalSectionRawMutex, GatoRetainedStore<GATO_MAX_RETAINED>>,
    outbound_queue: &'static Channel<CriticalSectionRawMutex, OutboundPublish, GATO_OUTBOUND_N>,
) {
    loop {
        let outbound = outbound_queue.receive().await;
        let bytes = match encode_publish_qos0(
            outbound.topic.as_str(),
            outbound.payload.as_slice(),
            outbound.retain,
        ) {
            Ok(bytes) => bytes,
            Err(_) => {
                defmt::warn!("GatoMQTT failed to encode outbound publish");
                continue;
            }
        };

        let mut registry = registry.lock().await;
        let subscribers = find_subscribers(&registry, outbound.topic.as_str(), usize::MAX);

        if outbound.retain {
            let mut retained = retained.lock().await;
            if retained
                .set(
                    outbound.topic.as_str(),
                    outbound.payload.as_slice(),
                    mqttrs::QoS::AtMostOnce,
                )
                .is_err()
            {
                defmt::warn!("GatoMQTT retained store full; dropping retained update");
            }
            drop(retained);
        }

        for (session_id, _) in subscribers {
            let Some(session) = registry.get_mut(session_id) else {
                continue;
            };

            if session
                .outbox
                .push(OutboundPacket {
                    bytes: bytes.clone(),
                })
                .is_err()
            {
                defmt::warn!("GatoMQTT session outbox full for session {}", session_id);
            }
        }
    }
}

fn map_set_topic_to_state(topic: &str) -> Option<HString<128>> {
    let prefix = topic.strip_suffix("/set")?;
    let mut state_topic = HString::<128>::new();
    state_topic.push_str(prefix).ok()?;
    state_topic.push_str("/state").ok()?;
    Some(state_topic)
}

// =============================================================================
// Accept Task
// =============================================================================

#[embassy_executor::task(pool_size = MAX_SESSIONS)]
async fn accept_task(
    stack: &'static Stack<'static>,
    broker: &'static BrokerMutex,
    notification_registry: &'static NotificationRegistry<MAX_SESSIONS, CriticalSectionRawMutex>,
    session_id_gen: &'static Mutex<CriticalSectionRawMutex, SessionIdGen>,
    socket_idx: usize,
) {
    const RX_BUFFER_SIZE: usize = 1536;
    const TX_BUFFER_SIZE: usize = 1536;
    const DEFAULT_KEEP_ALIVE: u16 = 60;

    loop {
        let mut rx_buf = [0u8; RX_BUFFER_SIZE];
        let mut tx_buf = [0u8; TX_BUFFER_SIZE];
        let mut socket = embassy_net::tcp::TcpSocket::new(*stack, &mut rx_buf, &mut tx_buf);

        defmt::debug!(
            "Socket {} waiting for connection on port {}",
            socket_idx,
            MQTT_PORT
        );

        if socket.accept(MQTT_PORT).await.is_err() {
            defmt::error!("Socket {} accept error", socket_idx);
            embassy_time::Timer::after(embassy_time::Duration::from_secs(1)).await;
            continue;
        }

        let session_id = session_id_gen.lock().await.generate();
        defmt::info!("Socket {} accepted session {}", socket_idx, session_id);

        // Register session with broker
        {
            let mut broker_lock = broker.lock().await;
            if broker_lock
                .register_new_session(session_id, DEFAULT_KEEP_ALIVE, current_time_nanos())
                .is_err()
            {
                defmt::error!("Failed to register session {}", session_id);
                continue;
            }
        }

        notification_registry
            .register_session(socket_idx, session_id)
            .await;
        let notify_receiver = notification_registry.receiver(socket_idx);

        let handler_config = HandlerConfig {
            default_keep_alive_secs: DEFAULT_KEEP_ALIVE,
        };

        let _result = handle_connection::<
            CriticalSectionRawMutex,
            MAX_TOPIC_NAME_LENGTH,
            MAX_PAYLOAD_SIZE,
            QUEUE_SIZE,
            MAX_SESSIONS,
            MAX_TOPICS,
            MAX_SUBSCRIBERS_PER_TOPIC,
            RX_BUFFER_SIZE,
        >(
            &mut socket,
            session_id,
            socket_idx,
            broker,
            notification_registry,
            notify_receiver,
            &handler_config,
        )
        .await;

        defmt::info!("Session {} closing", session_id);
        notification_registry.unregister_session(socket_idx).await;
        let mut broker_lock = broker.lock().await;
        broker_lock.remove_session(session_id);

        defmt::info!("Socket {} ready for new connection", socket_idx);
    }
}

#[embassy_executor::task(pool_size = GATO_MAX_SESSIONS)]
async fn gatomqtt_accept_task(
    stack: &'static Stack<'static>,
    socket_idx: usize,
    registry: &'static GatoMutex<
        GatoCriticalSectionRawMutex,
        GatoSessionRegistry<GATO_MAX_SESSIONS, GATO_MAX_SUBS, GATO_MAX_INFLIGHT>,
    >,
    retained: &'static GatoMutex<GatoCriticalSectionRawMutex, GatoRetainedStore<GATO_MAX_RETAINED>>,
    inbound_queue: &'static GatoMutex<
        GatoCriticalSectionRawMutex,
        GatoInboundQueue<GATO_INBOUND_N>,
    >,
) {
    loop {
        let mut rx_buf = [0u8; GATO_TCP_BUFFER_SIZE];
        let mut tx_buf = [0u8; GATO_TCP_BUFFER_SIZE];
        let mut socket = embassy_net::tcp::TcpSocket::new(*stack, &mut rx_buf, &mut tx_buf);
        socket.set_timeout(Some(embassy_time::Duration::from_secs(30)));

        defmt::debug!(
            "GatoMQTT socket {} waiting on port {}",
            socket_idx,
            GATO_MQTT_PORT
        );

        if socket.accept(GATO_MQTT_PORT).await.is_err() {
            defmt::warn!("GatoMQTT socket {} accept error", socket_idx);
            embassy_time::Timer::after(embassy_time::Duration::from_secs(1)).await;
            continue;
        }

        defmt::info!("GatoMQTT socket {} accepted client", socket_idx);

        let mut transport = TcpTransport::new(socket);
        let mut frame_buf = [0u8; GATO_MAX_PACKET_SIZE];
        gatomqtt_connection_loop(
            &mut transport,
            registry,
            retained,
            inbound_queue,
            &GATOMQTT_CONFIG,
            &mut frame_buf,
        )
        .await;

        defmt::info!("GatoMQTT socket {} ready for new connection", socket_idx);
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let pins = WifiPins {
        pio: p.PIO0,
        dma: p.DMA_CH0,
        clk: p.PIN_29,
        dio: p.PIN_24,
        cs: p.PIN_25,
        pwr: p.PIN_23,
    };

    let stack_config = Config::dhcpv4(Default::default());

    let (control, net_device, pio_keepalive) =
        WifiManager::init_cyw43(pins, Irqs, cyw43::PowerManagementMode::PowerSave, spawner).await;

    let mut rng = embassy_rp::clocks::RoscRng;
    let seed = rng.next_u64();

    static RESOURCES: StaticCell<StackResources<6>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        net_device,
        stack_config,
        RESOURCES.init(StackResources::new()),
        seed,
    );

    static STACK_CELL: StaticCell<Stack<'static>> = StaticCell::new();
    let stack = STACK_CELL.init(stack);

    let _ = spawner.spawn(net_runner_task(runner));

    let mut wifi_manager = WifiManager::new(control, stack, pio_keepalive);
    wifi_manager.join_network(WIFI_SSID, WIFI_PASSWORD).await;

    let ip_info = wifi_manager.stack.config_v4().unwrap();
    info!("WiFi connected successfully");
    info!("SSID: {}", WIFI_SSID);
    info!("IP Address: {}", ip_info.address);

    // Legacy broker components stay available in the example, but the active path below
    // runs the new GatoMQTT connection loop on top of embassy-net TcpSocket.
    let _broker: &'static BrokerMutex = BROKER_CELL.init(Mutex::new(MqttBroker::new()));
    let _session_id_gen: &'static Mutex<CriticalSectionRawMutex, SessionIdGen> =
        SESSION_ID_GEN_CELL.init(Mutex::new(SessionIdGen::new()));
    let _notification_registry: &'static NotificationRegistry<
        MAX_SESSIONS,
        CriticalSectionRawMutex,
    > = NOTIFICATION_REGISTRY.init(NotificationRegistry::new(core::array::from_fn(|_| {
        Channel::<CriticalSectionRawMutex, (), 1>::new()
    })));

    let registry = GATO_REGISTRY_CELL.init(GatoMutex::new(GatoSessionRegistry::new()));
    let retained = GATO_RETAINED_CELL.init(GatoMutex::new(GatoRetainedStore::new()));
    let inbound_queue = GATO_INBOUND_Q_CELL.init(GatoMutex::new(GatoInboundQueue::new()));
    let outbound_queue = GATO_OUTBOUND_Q_CELL.init(Channel::new());

    info!("GatoMQTT broker initialized");
    info!("GatoMQTT listening on port {}", GATO_MQTT_PORT);
    info!(
        "GatoMQTT supports up to {} concurrent sessions",
        GATO_MAX_SESSIONS
    );
    info!("TLS is not enabled on this step yet; use plaintext MQTT for stand testing");

    let _ = spawner.spawn(gatomqtt_inbound_logger_task(inbound_queue, outbound_queue));
    let _ = spawner.spawn(mqtt_publish_task(
        registry,
        retained,
        outbound_queue,
    ));
    for idx in 0..GATO_MAX_SESSIONS {
        let _ = spawner.spawn(gatomqtt_accept_task(
            stack,
            idx,
            registry,
            retained,
            inbound_queue,
        ));
    }

    // Keep executor alive
    loop {
        embassy_time::Timer::after(embassy_time::Duration::from_secs(1)).await;
    }
}
