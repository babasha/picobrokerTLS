#![no_std]
#![no_main]

extern crate alloc;

use alloc::boxed::Box;
#[cfg(feature = "tls")]
use core::cell::UnsafeCell;
use core::future::pending;
#[cfg(not(feature = "tls"))]
use core::ptr::NonNull;
use core::str;

use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpListenEndpoint, Stack, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use esp_alloc::heap_allocator;
use esp_backtrace as _;
use esp_println::println;
use gatomqtt::config::GATOMQTT_CONFIG;
use gatomqtt::handler::command::MqttIntent;
use gatomqtt::handler::connection::connection_loop;
use gatomqtt::router::RetainedStore;
use gatomqtt::session::registry::SessionRegistry;
#[cfg(feature = "tls")]
use gatomqtt::tls::embedded_tls_psk::EmbeddedTlsPskSession;
#[cfg(feature = "tls")]
use gatomqtt::transport::tls::TlsTransport as GatoTlsTransport;
#[cfg(feature = "tls")]
use gatomqtt::transport::Transport as _;

mod bootstrap;
#[cfg(feature = "tls")]
mod tls_support;
mod transport;

use bootstrap::RECLAIMED_RAM;
#[cfg(feature = "tls")]
use bootstrap::SharedTrng;
#[cfg(feature = "tls")]
use tls_support::psk_config;
use transport::TcpTransport;

esp_bootloader_esp_idf::esp_app_desc!();

#[defmt::panic_handler]
fn defmt_panic() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

defmt::timestamp!("{=u64:us}", 0);

#[cfg(feature = "tls")]
const MQTT_PORT: u16 = 8883;
#[cfg(not(feature = "tls"))]
const MQTT_PORT: u16 = 1883;

#[cfg(feature = "tls")]
const HEAP_SIZE: usize = 200 * 1024;
#[cfg(not(feature = "tls"))]
const HEAP_SIZE: usize = 160 * 1024;

#[cfg(feature = "tls")]
const TLS_ENABLE_INBOUND_LOGGER: bool = false;

// Must fit the largest MQTT packet the broker will accept:
//   5 (fixed header) + 2 + max_topic_len(128) + 2 (QoS1 PID) + max_payload_len(512) ≈ 650.
// Rounded up for headroom.
const MAX_PACKET_SIZE: usize = 768;
#[cfg(feature = "tls")]
const TCP_BUFFER_SIZE: usize = 512;
#[cfg(not(feature = "tls"))]
const TCP_BUFFER_SIZE: usize = 512;

/// Buffer size for the embedded-tls session (rx/tx record buffers + plaintext
/// buffer, three of these per session = ~12 KB plus the session struct).
#[cfg(feature = "tls")]
const TLS_SESSION_BUF: usize = 4096;

/// Maximum concurrent TLS connections.
#[cfg(feature = "tls")]
const MAX_TLS_CONNECTIONS: usize = 4;

/// Maximum concurrent plaintext connections.
#[cfg(not(feature = "tls"))]
const MAX_PLAIN_CONNECTIONS: usize = 10;

#[cfg(feature = "tls")]
const MAX_SESSIONS: usize = MAX_TLS_CONNECTIONS;
#[cfg(not(feature = "tls"))]
const MAX_SESSIONS: usize = MAX_PLAIN_CONNECTIONS;

#[cfg(feature = "tls")]
const MAX_SUBS: usize = 1;
#[cfg(not(feature = "tls"))]
const MAX_SUBS: usize = 4;

#[cfg(feature = "tls")]
const MAX_INFLIGHT: usize = 0;
#[cfg(not(feature = "tls"))]
const MAX_INFLIGHT: usize = 2;

#[cfg(feature = "tls")]
const MAX_RETAINED: usize = 0;
#[cfg(not(feature = "tls"))]
const MAX_RETAINED: usize = 8;

#[cfg(feature = "tls")]
const INBOUND_N: usize = 1;
#[cfg(not(feature = "tls"))]
const INBOUND_N: usize = 4;

#[cfg(feature = "tls")]
const STACK_SOCKETS: usize = MAX_SESSIONS + 1;
#[cfg(not(feature = "tls"))]
const STACK_SOCKETS: usize = MAX_SESSIONS + 2;

// ── shared broker state ────────────────────────────────────────────────────────

type Registry = SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>;
type Retained = RetainedStore<MAX_RETAINED>;
type Inbound = Channel<CriticalSectionRawMutex, MqttIntent, INBOUND_N>;

static REGISTRY_MUTEX: Mutex<CriticalSectionRawMutex, Registry> =
    Mutex::new(SessionRegistry::new());
static RETAINED_MUTEX: Mutex<CriticalSectionRawMutex, Retained> = Mutex::new(RetainedStore::new());
static INBOUND_MUTEX: Inbound = Channel::new();
static OUTBOX_SIGNALS: [Signal<CriticalSectionRawMutex, ()>; MAX_SESSIONS] =
    [const { Signal::new() }; MAX_SESSIONS];

#[cfg(feature = "tls")]
struct WorkerBuffers {
    rx: UnsafeCell<[u8; TCP_BUFFER_SIZE]>,
    tx: UnsafeCell<[u8; TCP_BUFFER_SIZE]>,
}

#[cfg(feature = "tls")]
unsafe impl Sync for WorkerBuffers {}

#[cfg(feature = "tls")]
impl WorkerBuffers {
    const fn new() -> Self {
        Self {
            rx: UnsafeCell::new([0; TCP_BUFFER_SIZE]),
            tx: UnsafeCell::new([0; TCP_BUFFER_SIZE]),
        }
    }

    fn borrow(
        &'static self,
    ) -> (
        &'static mut [u8; TCP_BUFFER_SIZE],
        &'static mut [u8; TCP_BUFFER_SIZE],
    ) {
        unsafe { (&mut *self.rx.get(), &mut *self.tx.get()) }
    }
}

#[cfg(feature = "tls")]
struct WorkerSocketSlot {
    socket: UnsafeCell<Option<TcpSocket<'static>>>,
}

#[cfg(feature = "tls")]
unsafe impl Sync for WorkerSocketSlot {}

#[cfg(feature = "tls")]
impl WorkerSocketSlot {
    const fn new() -> Self {
        Self {
            socket: UnsafeCell::new(None),
        }
    }

    fn put(&'static self, socket: TcpSocket<'static>) {
        unsafe {
            *self.socket.get() = Some(socket);
        }
    }

    fn take(&'static self) -> Option<TcpSocket<'static>> {
        unsafe { (*self.socket.get()).take() }
    }
}

#[cfg(feature = "tls")]
struct WorkerFrameBuffers {
    read: UnsafeCell<[u8; MAX_PACKET_SIZE]>,
}

#[cfg(feature = "tls")]
unsafe impl Sync for WorkerFrameBuffers {}

#[cfg(feature = "tls")]
impl WorkerFrameBuffers {
    const fn new() -> Self {
        Self {
            read: UnsafeCell::new([0; MAX_PACKET_SIZE]),
        }
    }

    fn borrow(&'static self) -> &'static mut [u8; MAX_PACKET_SIZE] {
        unsafe { &mut *self.read.get() }
    }
}

#[cfg(feature = "tls")]
static WORKER_BUFFERS: [WorkerBuffers; MAX_TLS_CONNECTIONS] =
    [const { WorkerBuffers::new() }; MAX_TLS_CONNECTIONS];
#[cfg(feature = "tls")]
static WORKER_SOCKETS: [WorkerSocketSlot; MAX_TLS_CONNECTIONS] =
    [const { WorkerSocketSlot::new() }; MAX_TLS_CONNECTIONS];
#[cfg(feature = "tls")]
static WORKER_FRAME_BUFFERS: [WorkerFrameBuffers; MAX_TLS_CONNECTIONS] =
    [const { WorkerFrameBuffers::new() }; MAX_TLS_CONNECTIONS];
#[cfg(feature = "tls")]
static WORKER_BUSY: [Mutex<CriticalSectionRawMutex, bool>; MAX_TLS_CONNECTIONS] =
    [const { Mutex::new(false) }; MAX_TLS_CONNECTIONS];
#[cfg(feature = "tls")]
static WORKER_SIGNALS: [Signal<CriticalSectionRawMutex, ()>; MAX_TLS_CONNECTIONS] =
    [const { Signal::new() }; MAX_TLS_CONNECTIONS];

// ── dynamic TCP socket buffers (plain mode only) ───────────────────────────────

#[cfg(not(feature = "tls"))]
/// Heap-allocated TCP socket rx/tx buffers that expose `'static` references.
struct OwnedSocketBuffers {
    rx: NonNull<[u8]>,
    tx: NonNull<[u8]>,
}

#[cfg(not(feature = "tls"))]
impl OwnedSocketBuffers {
    fn new() -> Option<Self> {
        let rx = alloc::vec![0u8; TCP_BUFFER_SIZE].into_boxed_slice();
        let tx = alloc::vec![0u8; TCP_BUFFER_SIZE].into_boxed_slice();
        Some(Self {
            rx: NonNull::new(Box::into_raw(rx))?,
            tx: NonNull::new(Box::into_raw(tx))?,
        })
    }

    unsafe fn as_static_mut(&mut self) -> (&'static mut [u8], &'static mut [u8]) {
        unsafe { (&mut *self.rx.as_ptr(), &mut *self.tx.as_ptr()) }
    }
}

#[cfg(not(feature = "tls"))]
impl Drop for OwnedSocketBuffers {
    fn drop(&mut self) {
        unsafe {
            drop(Box::from_raw(self.rx.as_ptr()));
            drop(Box::from_raw(self.tx.as_ptr()));
        }
    }
}

// ── inbound logger (optional) ─────────────────────────────────────────────────

#[embassy_executor::task]
async fn inbound_logger_task(inbound: &'static Inbound) {
    loop {
        if let Ok(intent) = inbound.try_receive() {
            let payload = str::from_utf8(intent.payload.as_slice()).unwrap_or("<binary>");
            println!("[INBOUND] {} => {}", intent.topic.as_str(), payload);
        } else {
            embassy_time::Timer::after(embassy_time::Duration::from_millis(50)).await;
        }
    }
}

// ── TLS worker ────────────────────────────────────────────────────────────────

#[cfg(feature = "tls")]
async fn wait_for_worker_socket(
    socket_slot: &'static WorkerSocketSlot,
    ready: &'static Signal<CriticalSectionRawMutex, ()>,
) -> TcpSocket<'static> {
    loop {
        if let Some(socket) = socket_slot.take() {
            return socket;
        }
        ready.wait().await;
    }
}

#[cfg(feature = "tls")]
async fn release_worker_slot(
    slot: usize,
    busy_flag: &'static Mutex<CriticalSectionRawMutex, bool>,
    label: &str,
) {
    let mut busy = busy_flag.lock().await;
    *busy = false;
    println!("[{} {}] Released slot", label, slot);
}

#[cfg(feature = "tls")]
async fn try_reserve_worker_slot() -> Option<usize> {
    for slot in 0..MAX_TLS_CONNECTIONS {
        let mut busy = WORKER_BUSY[slot].lock().await;
        if !*busy {
            *busy = true;
            println!("[ACCEPT/TLS] Reserved worker {}", slot);
            return Some(slot);
        }
    }

    None
}

#[cfg(feature = "tls")]
async fn worker_busy_snapshot() -> [bool; MAX_TLS_CONNECTIONS] {
    let mut snapshot = [false; MAX_TLS_CONNECTIONS];
    let mut slot = 0;
    while slot < MAX_TLS_CONNECTIONS {
        snapshot[slot] = *WORKER_BUSY[slot].lock().await;
        slot += 1;
    }
    snapshot
}

/// Pull 32 bytes of CSPRNG output from the shared TRNG, used as the server's
/// random in the ServerHello. Brief lock held while we burst 8 × u32.
#[cfg(feature = "tls")]
async fn fresh_server_random(trng: &'static SharedTrng) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    let guard = trng.lock().await;
    for chunk in bytes.chunks_exact_mut(4) {
        chunk.copy_from_slice(&guard.random().to_le_bytes());
    }
    bytes
}

#[cfg(feature = "tls")]
async fn mqtt_tls_worker_body(
    slot: usize,
    trng: &'static SharedTrng,
    socket_slot: &'static WorkerSocketSlot,
    busy_flag: &'static Mutex<CriticalSectionRawMutex, bool>,
    ready: &'static Signal<CriticalSectionRawMutex, ()>,
) {
    loop {
        let mut socket = wait_for_worker_socket(socket_slot, ready).await;

        println!(
            "[TLS WORKER {}] Handling {:?}",
            slot,
            socket.remote_endpoint()
        );

        let server_random = fresh_server_random(trng).await;
        let frame_buf = WORKER_FRAME_BUFFERS[slot].borrow();
        let tcp_transport = TcpTransport::new(&mut socket);

        // Build the embedded-tls session and wrap it in gatomqtt's TlsTransport
        // adapter so it can be fed straight into connection_loop.
        let session: EmbeddedTlsPskSession<'_, _, TLS_SESSION_BUF> =
            EmbeddedTlsPskSession::new(tcp_transport, psk_config(), server_random);
        let mut transport = GatoTlsTransport::new(session);

        if let Err(error) = transport.accept().await {
            println!("[TLS WORKER {}] Handshake failed: {:?}", slot, error);
            transport.close().await;
            release_worker_slot(slot, busy_flag, "TLS WORKER").await;
            continue;
        }
        println!("[TLS WORKER {}] Handshake complete", slot);

        Box::pin(connection_loop(
            &mut transport,
            plain_registry(),
            plain_retained(),
            plain_outbox_signals(),
            plain_inbound(),
            &GATOMQTT_CONFIG,
            frame_buf,
        ))
        .await;

        println!("[TLS WORKER {}] Connection closed", slot);
        release_worker_slot(slot, busy_flag, "TLS WORKER").await;
    }
}

#[cfg(feature = "tls")]
macro_rules! define_tls_worker_task {
    ($name:ident, $slot:expr) => {
        #[embassy_executor::task]
        async fn $name(trng: &'static SharedTrng) {
            mqtt_tls_worker_body(
                $slot,
                trng,
                &WORKER_SOCKETS[$slot],
                &WORKER_BUSY[$slot],
                &WORKER_SIGNALS[$slot],
            )
            .await;
        }
    };
}

#[cfg(feature = "tls")]
define_tls_worker_task!(mqtt_tls_worker0_task, 0);
#[cfg(feature = "tls")]
define_tls_worker_task!(mqtt_tls_worker1_task, 1);
#[cfg(feature = "tls")]
define_tls_worker_task!(mqtt_tls_worker2_task, 2);
#[cfg(feature = "tls")]
define_tls_worker_task!(mqtt_tls_worker3_task, 3);

#[cfg(feature = "tls")]
#[embassy_executor::task]
async fn mqtt_accept_task(stack: Stack<'static>) {
    println!("[ACCEPT/TLS] TLS accept loop started");
    let mut all_busy_logged = false;

    loop {
        let Some(slot) = try_reserve_worker_slot().await else {
            if !all_busy_logged {
                let snapshot = worker_busy_snapshot().await;
                println!("[ACCEPT/TLS] All worker slots busy {:?}", snapshot);
                all_busy_logged = true;
            }
            embassy_time::Timer::after(embassy_time::Duration::from_millis(50)).await;
            continue;
        };

        if all_busy_logged {
            println!("[ACCEPT/TLS] Worker slot available again");
            all_busy_logged = false;
        }

        let buffers = &WORKER_BUFFERS[slot];
        let socket_slot = &WORKER_SOCKETS[slot];
        let ready = &WORKER_SIGNALS[slot];
        let (rx_buf, tx_buf) = buffers.borrow();
        let mut socket = TcpSocket::new(stack, rx_buf, tx_buf);
        socket.set_timeout(Some(embassy_time::Duration::from_secs(30)));

        println!(
            "[ACCEPT/TLS] Listening on {} with worker {}",
            MQTT_PORT, slot
        );
        if socket
            .accept(IpListenEndpoint {
                addr: None,
                port: MQTT_PORT,
            })
            .await
            .is_err()
        {
            println!("[ACCEPT/TLS] Accept error");
            let mut busy = WORKER_BUSY[slot].lock().await;
            *busy = false;
            embassy_time::Timer::after(embassy_time::Duration::from_millis(250)).await;
            continue;
        }

        println!(
            "[ACCEPT/TLS] Accepted {:?}, handoff to worker {}",
            socket.remote_endpoint(),
            slot
        );
        socket_slot.put(socket);
        ready.signal(());
    }
}

// ── plain worker ──────────────────────────────────────────────────────────────

#[cfg(not(feature = "tls"))]
#[embassy_executor::task(pool_size = MAX_PLAIN_CONNECTIONS)]
async fn plain_worker(stack: Stack<'static>) {
    loop {
        let Some(mut bufs) = OwnedSocketBuffers::new() else {
            println!("[PLAIN WORKER] OOM: cannot allocate TCP buffers, retrying");
            embassy_time::Timer::after(embassy_time::Duration::from_millis(500)).await;
            continue;
        };
        let (rx, tx) = unsafe { bufs.as_static_mut() };
        let mut socket = TcpSocket::new(stack, rx, tx);
        socket.set_timeout(Some(embassy_time::Duration::from_secs(30)));

        if socket
            .accept(IpListenEndpoint {
                addr: None,
                port: MQTT_PORT,
            })
            .await
            .is_err()
        {
            continue;
        }
        println!("[PLAIN WORKER] Accepted {:?}", socket.remote_endpoint());

        let mut frame_buf = [0u8; MAX_PACKET_SIZE];
        let mut transport = TcpTransport::new(&mut socket);

        Box::pin(connection_loop(
            &mut transport,
            plain_registry(),
            plain_retained(),
            plain_outbox_signals(),
            plain_inbound(),
            &GATOMQTT_CONFIG,
            &mut frame_buf,
        ))
        .await;

        println!("[PLAIN WORKER] Connection closed");
    }
}

// ── runtime tasks ─────────────────────────────────────────────────────────────

#[cfg(feature = "tls")]
#[embassy_executor::task]
async fn tls_runtime_task(spawner: Spawner, stack: Stack<'static>, trng: &'static SharedTrng) {
    println!(
        "[MAIN] GatoMQTT port=8883 transport=tls (embedded-tls PSK_KE) max_connections={}",
        MAX_TLS_CONNECTIONS
    );
    if TLS_ENABLE_INBOUND_LOGGER {
        spawner.must_spawn(inbound_logger_task(&INBOUND_MUTEX));
    }
    spawner.must_spawn(mqtt_tls_worker0_task(trng));
    spawner.must_spawn(mqtt_tls_worker1_task(trng));
    spawner.must_spawn(mqtt_tls_worker2_task(trng));
    spawner.must_spawn(mqtt_tls_worker3_task(trng));
    spawner.must_spawn(mqtt_accept_task(stack));
    println!(
        "[MAIN] Spawned {} TLS workers + accept loop",
        MAX_TLS_CONNECTIONS
    );
    pending::<()>().await;
}

#[cfg(not(feature = "tls"))]
#[embassy_executor::task]
async fn plain_runtime_task(spawner: Spawner, stack: Stack<'static>) {
    println!(
        "[MAIN] GatoMQTT port=1883 transport=plain max_connections={}",
        MAX_PLAIN_CONNECTIONS
    );
    for _ in 0..MAX_PLAIN_CONNECTIONS {
        spawner.must_spawn(plain_worker(stack));
    }
    println!("[MAIN] Spawned {} plain workers", MAX_PLAIN_CONNECTIONS);
    pending::<()>().await;
}

// ── entry point ───────────────────────────────────────────────────────────────

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    heap_allocator!(size: HEAP_SIZE - RECLAIMED_RAM);
    let stack_resources = mk_static!(StackResources<STACK_SOCKETS>, StackResources::new());

    #[cfg(feature = "tls")]
    let (stack, trng) = bootstrap::bootstrap_stack(spawner, stack_resources).await;
    #[cfg(not(feature = "tls"))]
    let (stack, _trng) = bootstrap::bootstrap_stack(spawner, stack_resources).await;

    #[cfg(feature = "tls")]
    {
        spawner.must_spawn(tls_runtime_task(spawner, stack, trng));
    }

    #[cfg(not(feature = "tls"))]
    {
        spawner.must_spawn(plain_runtime_task(spawner, stack));
    }

    pending::<()>().await;
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn plain_registry() -> &'static Mutex<CriticalSectionRawMutex, Registry> {
    &REGISTRY_MUTEX
}

fn plain_retained() -> &'static Mutex<CriticalSectionRawMutex, Retained> {
    &RETAINED_MUTEX
}

fn plain_outbox_signals() -> &'static [Signal<CriticalSectionRawMutex, ()>; MAX_SESSIONS] {
    &OUTBOX_SIGNALS
}

fn plain_inbound() -> &'static Inbound {
    &INBOUND_MUTEX
}
