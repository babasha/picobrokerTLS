# GatoMQTT

[![Crates.io](https://img.shields.io/crates/v/GatoMQTT.svg)](https://crates.io/crates/GatoMQTT)
[![Docs.rs](https://docs.rs/GatoMQTT/badge.svg)](https://docs.rs/GatoMQTT)
[![License](https://img.shields.io/crates/l/GatoMQTT.svg)](https://github.com/babasha/picobrokerTLS/blob/main/crates/GatoMQTT/LICENSE)

`GatoMQTT` is a `no_std`, no-allocator MQTT 3.1.1 broker library for embedded
Rust — small enough to run on a Raspberry Pi Pico (RP2040) or ESP32-C3 with
a few connected clients, while staying transport-agnostic (plain TCP or any
implementation of the [`TlsSession`](crate::transport::tls::TlsSession) trait).

## Features

* **MQTT 3.1.1 control plane** — CONNECT / SUBSCRIBE / PUBLISH (QoS 0 + QoS 1)
  / PINGREQ / DISCONNECT, retained messages, Last Will & Testament, simple
  constant-time username/password auth.
* **Bounded session storage** — `SessionRegistry<MAX_SESSIONS, MAX_SUBS,
  MAX_INFLIGHT>` with per-session subscription set, in-flight QoS 1 PIDs, and
  token-bucket rate limiting. All state lives on the stack/static — no heap.
* **Transport-agnostic** — implements broker logic over a `Transport` trait
  (async `read` / `write` / `close`). Wrappers exist for plain TCP, and any
  `TlsSession` implementation can be plugged in via `TlsTransport`.
* **TLS 1.3 PSK_KE** out-of-the-box adapter (behind the `tls-psk` feature)
  using [`GatoPSKTLS`](https://crates.io/crates/GatoPSKTLS) — a sibling crate
  with a `no_std` TLS 1.3 PSK client + server.
* **Embassy-friendly** — async logic uses `embassy-sync` primitives.
  Optional integration with `embassy-net::TcpSocket` behind the `embassy-net`
  feature.

## Status / scope

GatoMQTT targets a small private deployment where predictability, static
limits, and low resource usage matter more than full MQTT broker feature
coverage. It is **not** an `mosquitto` replacement; rather, a building block
for "broker on a microcontroller" use cases.

| Area | Status |
|---|---|
| MQTT 3.1.1 control packets | ✅ |
| QoS 0 + QoS 1 | ✅ |
| Retained messages, LWT | ✅ |
| Username/password auth | ✅ |
| Bounded heapless storage | ✅ |
| TLS 1.3 PSK adapter | ✅ via `tls-psk` feature |
| MQTT 5.0 | ❌ |
| QoS 2 (PUBREC/PUBREL/PUBCOMP) | ❌ |
| Bridges / clustering | ❌ |
| Persistent retained-store | ❌ — in-memory only |

## Quick start

```rust,ignore
use core::cell::RefCell;
use embassy_executor::Spawner;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::{channel::Channel, mutex::Mutex, signal::Signal};
use gatomqtt::config::GATOMQTT_CONFIG;
use gatomqtt::handler::command::MqttIntent;
use gatomqtt::handler::connection::connection_loop;
use gatomqtt::router::RetainedStore;
use gatomqtt::session::registry::SessionRegistry;

// Static broker state — bounded.
const MAX_SESSIONS: usize = 4;
const MAX_SUBS: usize = 8;
const MAX_INFLIGHT: usize = 2;
const MAX_RETAINED: usize = 8;
const INBOUND_N: usize = 4;

static REGISTRY: Mutex<
    CriticalSectionRawMutex,
    SessionRegistry<MAX_SESSIONS, MAX_SUBS, MAX_INFLIGHT>,
> = Mutex::new(SessionRegistry::new());
static RETAINED: Mutex<CriticalSectionRawMutex, RetainedStore<MAX_RETAINED>> =
    Mutex::new(RetainedStore::new());
static INBOUND: Channel<CriticalSectionRawMutex, MqttIntent, INBOUND_N> =
    Channel::new();
static OUTBOX: [Signal<CriticalSectionRawMutex, ()>; MAX_SESSIONS] =
    [const { Signal::new() }; MAX_SESSIONS];

// Per-connection task — accept a TCP socket, wrap in your transport, drive.
async fn handle_one(mut transport: impl gatomqtt::transport::Transport) {
    let mut frame_buf = [0u8; 768];
    connection_loop(
        &mut transport,
        &REGISTRY,
        &RETAINED,
        &OUTBOX,
        &INBOUND,
        &GATOMQTT_CONFIG,
        &mut frame_buf,
    )
    .await;
}
```

For TLS 1.3 PSK, enable the `tls-psk` feature and wrap your TCP transport in
`EmbeddedTlsPskSession` → `TlsTransport`. See the
[`tls::embedded_tls_psk`](crate::tls::embedded_tls_psk) module docs for the
wiring snippet.

For a fully wired ESP32-C3 example combining GatoMQTT + GatoPSKTLS over Wi-Fi,
see the [`picobrokerTLS`](https://github.com/babasha/picobrokerTLS)
companion repository under `examples/esp32c3_mqtt_server/`.

## Cargo features

```toml
GatoMQTT = { version = "0.2", default-features = false }
```

| Feature       | Effect                                                                  |
|---------------|-------------------------------------------------------------------------|
| `embassy-net` | Pulls `embassy-net` so `TcpTransport` examples work directly with embassy-net `TcpSocket`. |
| `tls-psk`     | Enables `tls::embedded_tls_psk::EmbeddedTlsPskSession` — a `TlsSession` implementation backed by [`GatoPSKTLS`](https://crates.io/crates/GatoPSKTLS) for TLS 1.3 PSK_KE. |

## License

MIT — see [`LICENSE`](LICENSE).
