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
use embassy_futures::select::{select, Either};

use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpListenEndpoint, Stack, StackResources};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
#[cfg(feature = "tls")]
use embassy_sync::signal::Signal;
use esp_alloc::heap_allocator;
#[cfg(feature = "tls")]
use esp_alloc::HEAP;
use esp_backtrace as _;
use esp_println::println;
use gatomqtt::codec::frame::write_packet;
use gatomqtt::codec::frame::{read_packet, ReadError};
use gatomqtt::config::GATOMQTT_CONFIG;
use gatomqtt::handler::connect::{prepare_connect, write_connack, HouseToken, PreparedConnect};
use gatomqtt::handler::pingreq::touch_pingreq;
use gatomqtt::handler::publish::{handle_publish, MqttIntent, PublishError};
use gatomqtt::handler::subscribe::{
    apply_unsubscribe, prepare_subscribe, write_prepared_subscribe,
};
use gatomqtt::router::RetainedStore;
use gatomqtt::session::registry::SessionRegistry;
use gatomqtt::transport::Transport as _;
use mqttrs::{Packet, PacketType};

mod bootstrap;
#[cfg(feature = "tls")]
mod tls_support;
mod transport;

use bootstrap::RECLAIMED_RAM;
#[cfg(feature = "tls")]
use tls_support::mqtt_server_config;
#[cfg(not(feature = "tls"))]
use transport::TcpTransport;
#[cfg(feature = "tls")]
use transport::{TcpTransport, TlsTransport};

#[cfg(feature = "tls")]
use mbedtls_rs::{SharedSessionConfig, Tls};

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
const TLS_USE_HW_ACCEL: bool = false;
#[cfg(feature = "tls")]
const TLS_DEBUG_LEVEL: u32 = 0;
#[cfg(feature = "tls")]
const TLS_ENABLE_INBOUND_LOGGER: bool = false;

#[cfg(feature = "tls")]
const HEAP_SIZE: usize = 272 * 1024;
#[cfg(not(feature = "tls"))]
const HEAP_SIZE: usize = 160 * 1024;

const MAX_PACKET_SIZE: usize = 192;
#[cfg(feature = "tls")]
const TCP_BUFFER_SIZE: usize = 256;
#[cfg(not(feature = "tls"))]
const TCP_BUFFER_SIZE: usize = 512;

/// Maximum concurrent TLS connections.
///
/// Each spawned worker holds 2×512 B TCP buffers on the heap while waiting on
/// accept(), so idle cost = MAX_TLS_CONNECTIONS × 1 KB.  TLS sessions (≈13 KB
/// each) are allocated only while a connection is active.
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
    write: UnsafeCell<[u8; MAX_PACKET_SIZE]>,
}

#[cfg(feature = "tls")]
unsafe impl Sync for WorkerFrameBuffers {}

#[cfg(feature = "tls")]
impl WorkerFrameBuffers {
    const fn new() -> Self {
        Self {
            read: UnsafeCell::new([0; MAX_PACKET_SIZE]),
            write: UnsafeCell::new([0; MAX_PACKET_SIZE]),
        }
    }

    fn borrow(
        &'static self,
    ) -> (
        &'static mut [u8; MAX_PACKET_SIZE],
        &'static mut [u8; MAX_PACKET_SIZE],
    ) {
        unsafe { (&mut *self.read.get(), &mut *self.write.get()) }
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

// ── dynamic TCP socket buffers ─────────────────────────────────────────────────

#[cfg(not(feature = "tls"))]
/// Heap-allocated TCP socket rx/tx buffers that expose `'static` references.
///
/// # Safety invariant
/// Any `TcpSocket` created from these buffers **must** be dropped before this
/// struct is dropped. The natural Rust LIFO drop order guarantees this when
/// `OwnedSocketBuffers` is declared before the `TcpSocket` in the same scope.
struct OwnedSocketBuffers {
    rx: NonNull<[u8]>,
    tx: NonNull<[u8]>,
}

#[cfg(not(feature = "tls"))]
impl OwnedSocketBuffers {
    /// Allocates rx and tx buffers from the heap.
    /// Returns `None` if the heap is exhausted.
    fn new() -> Option<Self> {
        let rx = alloc::vec![0u8; TCP_BUFFER_SIZE].into_boxed_slice();
        let tx = alloc::vec![0u8; TCP_BUFFER_SIZE].into_boxed_slice();
        Some(Self {
            rx: NonNull::new(Box::into_raw(rx))?,
            tx: NonNull::new(Box::into_raw(tx))?,
        })
    }

    /// Returns `'static` mutable slices into the heap buffers.
    ///
    /// # Safety
    /// The `TcpSocket` that borrows these slices must be dropped before
    /// `self` is dropped. Declare `self` before the socket so Rust's LIFO
    /// drop order guarantees this automatically.
    unsafe fn as_static_mut(&mut self) -> (&'static mut [u8], &'static mut [u8]) {
        // SAFETY: pointers are valid heap allocations, uniquely owned here.
        unsafe { (&mut *self.rx.as_ptr(), &mut *self.tx.as_ptr()) }
    }
}

#[cfg(not(feature = "tls"))]
impl Drop for OwnedSocketBuffers {
    fn drop(&mut self) {
        // SAFETY: pointers were created by Box::into_raw in new() and are
        // not used after TcpSocket is dropped (invariant upheld by callers).
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
//
// Each task instance handles one connection at a time, then loops back to
// accept the next one.  pool_size controls how many connections can be served
// concurrently; the embassy executor pre-allocates that many task-future slots
// (~300 B each), but TCP buffers and TLS sessions are heap-allocated only
// while a connection is active.
//
// Memory per active connection:  TCP buffers (2 × 512 B) + TLS session (~13 KB)
// Memory per idle slot:          TCP buffers (2 × 512 B, allocated at accept())

#[cfg(feature = "tls")]
async fn wait_for_worker_socket(socket_slot: &'static WorkerSocketSlot) -> TcpSocket<'static> {
    loop {
        if let Some(socket) = socket_slot.take() {
            return socket;
        }

        embassy_time::Timer::after(embassy_time::Duration::from_millis(10)).await;
    }
}

#[cfg(feature = "tls")]
fn log_tls_heap(stage: &str) {
    let stats = HEAP.stats();
    println!(
        "[TLS HEAP {}] used={} free={} size={}",
        stage,
        stats.current_usage,
        stats.size.saturating_sub(stats.current_usage),
        stats.size
    );
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

#[cfg(feature = "tls")]
async fn mqtt_tls_worker_body(
    slot: usize,
    tls: &'static Tls<'static>,
    tls_config: &'static SharedSessionConfig<'static>,
    socket_slot: &'static WorkerSocketSlot,
    busy_flag: &'static Mutex<CriticalSectionRawMutex, bool>,
    ready: &'static Signal<CriticalSectionRawMutex, ()>,
) {
    loop {
        ready.wait().await;

        let mut socket = wait_for_worker_socket(socket_slot).await;

        println!(
            "[TLS WORKER {}] Handling {:?}",
            slot,
            socket.remote_endpoint()
        );
        log_tls_heap("worker accepted");

        let (read_buf, write_buf) = WORKER_FRAME_BUFFERS[slot].borrow();
        let tcp_transport = TcpTransport::new(&mut socket);
        let mut transport = match TlsTransport::new(tls.reference(), tcp_transport, tls_config) {
            Ok(transport) => transport,
            Err(error) => {
                log_tls_heap("session init failed");
                println!("[TLS WORKER {}] Session init failed: {:?}", slot, error);
                release_worker_slot(slot, busy_flag, "TLS WORKER").await;
                continue;
            }
        };
        log_tls_heap("session init ok");

        if let Err(error) = transport.handshake().await {
            log_tls_heap("handshake failed");
            println!("[TLS WORKER {}] Handshake failed: {:?}", slot, error);
            transport.close().await;
            release_worker_slot(slot, busy_flag, "TLS WORKER").await;
            continue;
        }
        log_tls_heap("handshake ok");
        println!("[TLS WORKER {}] Handshake complete", slot);

        let session_id = Box::pin(accept_plain_connect(&mut transport, read_buf, write_buf)).await;

        if let Some(session_id) = session_id {
            loop {
                let keep_running = Box::pin(run_plain_session_step(
                    session_id,
                    &mut transport,
                    read_buf,
                    write_buf,
                ))
                .await;
                if !keep_running {
                    break;
                }
            }

            let mut registry = plain_registry().lock().await;
            let _ = registry.remove(session_id);
        }

        transport.close().await;
        println!("[TLS WORKER {}] Connection closed", slot);
        log_tls_heap("closed");
        release_worker_slot(slot, busy_flag, "TLS WORKER").await;
    }
}

#[cfg(feature = "tls")]
macro_rules! define_tls_worker_task {
    ($name:ident, $slot:expr) => {
        #[embassy_executor::task]
        async fn $name(
            tls: &'static Tls<'static>,
            tls_config: &'static SharedSessionConfig<'static>,
        ) {
            mqtt_tls_worker_body(
                $slot,
                tls,
                tls_config,
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

        let mut read_buf = [0u8; MAX_PACKET_SIZE];
        let mut write_buf = [0u8; MAX_PACKET_SIZE];
        let mut transport = TcpTransport::new(&mut socket);

        let session_id = Box::pin(accept_plain_connect(
            &mut transport,
            &mut read_buf,
            &mut write_buf,
        ))
        .await;

        if let Some(session_id) = session_id {
            loop {
                let keep_running = Box::pin(run_plain_session_step(
                    session_id,
                    &mut transport,
                    &mut read_buf,
                    &mut write_buf,
                ))
                .await;
                if !keep_running {
                    break;
                }
            }
            let mut registry = plain_registry().lock().await;
            let _ = registry.remove(session_id);
        }

        transport.close().await;
        println!("[PLAIN WORKER] Connection closed");
        // socket → bufs freed
    }
}

// ── CONNECT / session logic ───────────────────────────────────────────────────

async fn accept_plain_connect(
    transport: &mut impl gatomqtt::transport::Transport,
    read_buf: &mut [u8; MAX_PACKET_SIZE],
    write_buf: &mut [u8; MAX_PACKET_SIZE],
) -> Option<usize> {
    match read_packet(transport, read_buf).await {
        Ok(Packet::Connect(connect)) => {
            println!("[CONNECT] packet received");
            let config = gatomqtt_config();
            let token = HouseToken {
                username: config.house_token_username,
                password: config.house_token_password,
            };
            let connect_result = {
                let mut registry = plain_registry().lock().await;
                prepare_connect(&mut registry, &connect, &token)
            };
            match connect_result {
                Ok(PreparedConnect::Accepted(outcome)) => {
                    if write_connack(transport, mqttrs::ConnectReturnCode::Accepted, write_buf)
                        .await
                        .is_err()
                    {
                        let _ = transport.close().await;
                        return None;
                    }
                    mark_plain_activity(outcome.session_id).await;
                    println!("[CONNECT] accepted session={}", outcome.session_id);
                    Some(outcome.session_id)
                }
                Ok(PreparedConnect::Rejected(code)) => {
                    let _ = write_connack(transport, code, write_buf).await;
                    let _ = transport.close().await;
                    println!("[CONNECT] rejected");
                    None
                }
                Err(_) => {
                    let _ = transport.close().await;
                    println!("[CONNECT] error");
                    None
                }
            }
        }
        Ok(other) => {
            println!("[CONNECT] first packet not CONNECT: {:?}", other);
            let _ = transport.close().await;
            None
        }
        Err(ReadError::Eof) => {
            println!("[CONNECT] EOF before first packet");
            let _ = transport.close().await;
            None
        }
        Err(err) => {
            println!("[CONNECT] read error: {:?}", err);
            let _ = transport.close().await;
            None
        }
    }
}

async fn run_plain_session_step(
    session_id: usize,
    transport: &mut impl gatomqtt::transport::Transport,
    read_buf: &mut [u8; MAX_PACKET_SIZE],
    write_buf: &mut [u8; MAX_PACKET_SIZE],
) -> bool {
    if flush_plain_outbox(session_id, plain_registry(), transport)
        .await
        .is_err()
    {
        println!("[SESSION {}] outbox flush failed", session_id);
        return false;
    }

    match select(
        read_packet(transport, read_buf),
        embassy_time::Timer::after(embassy_time::Duration::from_millis(200)),
    )
    .await
    {
        Either::First(Ok(packet)) => {
            if !mark_plain_activity(session_id).await {
                println!("[SESSION {}] missing before packet handling", session_id);
                return false;
            }
            handle_plain_session_packet(session_id, transport, packet, write_buf).await
        }
        Either::First(Err(ReadError::Eof)) => {
            println!("[SESSION {}] peer closed", session_id);
            false
        }
        Either::First(Err(err)) => {
            println!("[SESSION {}] read error: {:?}", session_id, err);
            false
        }
        Either::Second(_) => {
            if is_plain_keepalive_expired(session_id).await {
                println!("[SESSION {}] keepalive expired", session_id);
                false
            } else {
                true
            }
        }
    }
}

async fn handle_plain_session_packet(
    session_id: usize,
    transport: &mut impl gatomqtt::transport::Transport,
    packet: Packet<'_>,
    write_buf: &mut [u8; MAX_PACKET_SIZE],
) -> bool {
    match packet.get_type() {
        PacketType::Disconnect => {
            println!("[SESSION {}] DISCONNECT", session_id);
            false
        }
        PacketType::Subscribe => {
            let Packet::Subscribe(subscribe) = packet else {
                unreachable!()
            };
            println!("[SESSION {}] SUBSCRIBE", session_id);
            let prepared = {
                let mut registry = plain_registry().lock().await;
                let retained = plain_retained().lock().await;
                let Some(session) = registry.get_mut(session_id) else {
                    println!("[SESSION {}] missing during SUBSCRIBE", session_id);
                    return false;
                };
                prepare_subscribe(session, &subscribe, &retained)
            };
            match write_prepared_subscribe(transport, &prepared, write_buf).await {
                Ok(()) => {
                    println!("[SESSION {}] SUBACK sent", session_id);
                    true
                }
                Err(_) => {
                    println!("[SESSION {}] SUBSCRIBE write failed", session_id);
                    false
                }
            }
        }
        PacketType::Unsubscribe => {
            let Packet::Unsubscribe(unsubscribe) = packet else {
                unreachable!()
            };
            println!("[SESSION {}] UNSUBSCRIBE", session_id);
            {
                let mut registry = plain_registry().lock().await;
                let Some(session) = registry.get_mut(session_id) else {
                    println!("[SESSION {}] missing during UNSUBSCRIBE", session_id);
                    return false;
                };
                apply_unsubscribe(session, &unsubscribe);
            }
            match write_packet(transport, &Packet::Unsuback(unsubscribe.pid), write_buf).await {
                Ok(()) => {
                    println!("[SESSION {}] UNSUBACK sent", session_id);
                    true
                }
                Err(_) => {
                    println!("[SESSION {}] UNSUBSCRIBE write failed", session_id);
                    false
                }
            }
        }
        PacketType::Pingreq => {
            println!("[SESSION {}] PINGREQ", session_id);
            {
                let mut registry = plain_registry().lock().await;
                let Some(session) = registry.get_mut(session_id) else {
                    println!("[SESSION {}] missing during PINGREQ", session_id);
                    return false;
                };
                touch_pingreq(session);
            }
            match write_packet(transport, &Packet::Pingresp, write_buf).await {
                Ok(()) => {
                    println!("[SESSION {}] PINGRESP sent", session_id);
                    true
                }
                Err(_) => {
                    println!("[SESSION {}] PINGREQ write failed", session_id);
                    false
                }
            }
        }
        PacketType::Publish => {
            let Packet::Publish(publish) = packet else {
                unreachable!()
            };
            println!("[SESSION {}] PUBLISH", session_id);
            let mut registry = plain_registry().lock().await;
            let mut retained = plain_retained().lock().await;
            match handle_publish(
                session_id,
                &mut registry,
                &mut retained,
                plain_inbound(),
                &publish,
            )
            .await
            {
                Ok(()) => {
                    println!("[SESSION {}] PUBLISH handled", session_id);
                    true
                }
                Err(PublishError::RateLimitDisconnect) => {
                    println!("[SESSION {}] rate limit disconnect", session_id);
                    false
                }
                Err(err) => {
                    println!("[SESSION {}] PUBLISH failed: {:?}", session_id, err);
                    false
                }
            }
        }
        other => {
            println!(
                "[SESSION {}] unhandled packet type: {:?}",
                session_id, other
            );
            true
        }
    }
}

// ── runtime tasks ─────────────────────────────────────────────────────────────

#[cfg(feature = "tls")]
#[embassy_executor::task]
async fn tls_runtime_task(
    spawner: Spawner,
    stack: Stack<'static>,
    tls: &'static Tls<'static>,
    tls_config: &'static SharedSessionConfig<'static>,
) {
    println!(
        "[MAIN] GatoMQTT port=8883 transport=tls max_connections={}",
        MAX_TLS_CONNECTIONS
    );
    if TLS_ENABLE_INBOUND_LOGGER {
        spawner.must_spawn(inbound_logger_task(&INBOUND_MUTEX));
    }
    spawner.must_spawn(mqtt_tls_worker0_task(tls, tls_config));
    spawner.must_spawn(mqtt_tls_worker1_task(tls, tls_config));
    spawner.must_spawn(mqtt_tls_worker2_task(tls, tls_config));
    spawner.must_spawn(mqtt_tls_worker3_task(tls, tls_config));
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
    let (mut tls, stack, mut accel): (Tls<'static>, Stack<'static>, _) =
        bootstrap::bootstrap_stack(spawner, stack_resources).await;
    #[cfg(not(feature = "tls"))]
    let stack = bootstrap::bootstrap_stack(spawner, stack_resources).await;

    #[cfg(feature = "tls")]
    {
        tls.set_debug(TLS_DEBUG_LEVEL);
    }

    #[cfg(feature = "tls")]
    let _accel_queue = if TLS_USE_HW_ACCEL {
        Some(accel.start())
    } else {
        None
    };
    #[cfg(feature = "tls")]
    let tls = mk_static!(Tls, tls);
    #[cfg(feature = "tls")]
    let tls_config = mk_static!(
        SharedSessionConfig<'static>,
        SharedSessionConfig::new(&mqtt_server_config()).unwrap()
    );

    #[cfg(feature = "tls")]
    {
        println!(
            "[MAIN] TLS HW accel: {}",
            if TLS_USE_HW_ACCEL {
                "enabled"
            } else {
                "disabled"
            }
        );
        spawner.must_spawn(tls_runtime_task(spawner, stack, tls, tls_config));
    }

    #[cfg(not(feature = "tls"))]
    {
        spawner.must_spawn(plain_runtime_task(spawner, stack));
    }

    pending::<()>().await;
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn gatomqtt_config() -> gatomqtt::config::BrokerConfig {
    let mut config = GATOMQTT_CONFIG;
    config.max_sessions = MAX_SESSIONS;
    config.max_subscriptions = MAX_SUBS;
    config.max_inflight = MAX_INFLIGHT;
    config.max_retained = MAX_RETAINED;
    config
}

fn plain_registry() -> &'static Mutex<CriticalSectionRawMutex, Registry> {
    &REGISTRY_MUTEX
}

fn plain_retained() -> &'static Mutex<CriticalSectionRawMutex, Retained> {
    &RETAINED_MUTEX
}

fn plain_inbound() -> &'static Inbound {
    &INBOUND_MUTEX
}

async fn flush_plain_outbox(
    session_id: usize,
    registry: &'static Mutex<CriticalSectionRawMutex, Registry>,
    transport: &mut impl gatomqtt::transport::Transport,
) -> Result<(), ()> {
    loop {
        let next_bytes = {
            let mut registry = registry.lock().await;
            let Some(session) = registry.get_mut(session_id) else {
                return Err(());
            };
            if session.outbox.is_empty() {
                None
            } else {
                Some(session.outbox.remove(0).bytes)
            }
        };
        let Some(bytes) = next_bytes else {
            return Ok(());
        };
        transport.write(bytes.as_slice()).await.map_err(|_| ())?;
    }
}

async fn mark_plain_activity(session_id: usize) -> bool {
    let mut registry = plain_registry().lock().await;
    let Some(session) = registry.get_mut(session_id) else {
        return false;
    };
    session.update_activity();
    true
}

async fn is_plain_keepalive_expired(session_id: usize) -> bool {
    let registry = plain_registry().lock().await;
    let Some(session) = registry.get(session_id) else {
        return true;
    };
    session.is_keepalive_expired(embassy_time::Instant::now())
}
