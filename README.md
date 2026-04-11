# GatoMQTT

`GatoMQTT` is a `no_std` MQTT 3.1.1 broker for constrained systems.

This repository started from a minimal PicoBroker-style core and has since been extended into a more practical embedded broker with:

- bounded in-memory session management
- retained messages
- Last Will handling
- simple constant-time username/password authentication
- QoS 1 retry state with bounded inflight storage
- publish rate limiting
- transport abstraction for plain TCP or TLS
- embedded-focused examples for RP2040 and ESP32-C3

The project is intended for small private deployments where predictability, static limits and low resource usage matter more than full broker feature coverage.

## What Is In This Fork

Compared to the original minimal broker shape, this version adds:

- a transport-agnostic broker core in [`src/lib.rs`](./src/lib.rs)
- retained message storage in [`src/router/retained.rs`](./src/router/retained.rs)
- session registry, LWT tracking and inflight QoS 1 state in [`src/session`](./src/session)
- command-topic fan-in queue for local device automation flows
- token-bucket rate limiting per session
- TLS identity storage helpers in [`src/tls/mod.rs`](./src/tls/mod.rs)
- an ESP32-C3 TLS example wired to a companion `mbedtls-rs` fork

The current ESP32-C3 example uses `TLS 1.3 + pure PSK`, which keeps handshake cost lower than X.509 on small MCUs and matches a closed-infrastructure deployment model.

## Features

- `no_std` broker core
- MQTT 3.1.1 packet handling
- wildcard subscriptions with `+` and `#`
- retained messages
- Last Will capture and delivery on displaced sessions
- bounded QoS 1 inflight retry tracking
- simple "house token" authentication via username/password
- publish rate limiting with disconnect threshold
- command-topic ingress queue for local control topics
- Tokio, RP2040 and ESP32-C3 example applications

## Current Broker Model

The broker is deliberately static and bounded.

- sessions live in a fixed-size registry
- subscriptions live in fixed-size per-session arrays
- retained entries live in a fixed-size store
- command intents live in a fixed-size queue
- no heap allocation is required in the core fast path

Default values are defined in [`src/config.rs`](./src/config.rs):

| Parameter | Default |
| --- | ---: |
| `max_sessions` | `8` |
| `max_subscriptions` | `32` |
| `max_inflight` | `16` |
| `max_retained` | `64` |
| `max_topic_len` | `128` |
| `max_payload_len` | `512` |
| `rate_capacity` | `20` |
| `rate_per_sec` | `10` |
| `max_violations` | `50` |
| `qos1_retry_ms` | `5000` |
| `qos1_max_retries` | `3` |

## TLS Story

TLS is not baked into the broker core. The broker only depends on a `Transport` trait, so the same logic can run over:

- plain TCP
- async TLS streams
- platform-specific socket wrappers

The ESP32-C3 example in [`examples/esp32c3_mqtt_server`](./examples/esp32c3_mqtt_server) is the reference secure deployment in this repository.

Important details:

- it uses a local fork of `mbedtls-rs`
- it is configured for `TLS 1.3` only
- it uses `pure PSK` instead of X.509
- it reuses one prepared `mbedtls_ssl_config` across sessions
- it does not serialize handshakes behind a global lock

That combination is aimed at maximizing concurrent handshakes on a small MCU with limited heap.

## Examples

- [`examples/tokio_mqtt_server`](./examples/tokio_mqtt_server): host-side broker for fast iteration
- [`examples/tokio_mqtt_client`](./examples/tokio_mqtt_client): simple host-side test client
- [`examples/rp2040_mqtt_server`](./examples/rp2040_mqtt_server): Pico W broker example
- [`examples/esp32c3_mqtt_server`](./examples/esp32c3_mqtt_server): ESP32-C3 broker with optional TLS

Run the host example:

```bash
cd examples/tokio_mqtt_server
cargo run
```

The embedded examples require their respective toolchains and board setup. The ESP32-C3 TLS example also depends on the companion `mbedtls-rs` workspace located next to this repository.

## Repository Layout

- [`src/broker.rs`](./src/broker.rs): top-level broker orchestration
- [`src/handler`](./src/handler): MQTT packet handlers
- [`src/router`](./src/router): subscriber matching and retained routing
- [`src/session`](./src/session): session state, registry and rate limiting
- [`src/transport.rs`](./src/transport.rs): broker transport abstraction
- [`src/tls/mod.rs`](./src/tls/mod.rs): TLS identity persistence helpers

More implementation notes are in [`docs/ARCHITECTURE.md`](./docs/ARCHITECTURE.md).

## Limitations

- MQTT 3.1.1 only
- no QoS 2 flow
- no persistent storage for sessions or retained messages in the broker core
- no ACL engine
- authentication is currently a single configured username/password pair
- capacities are static and tuned for embedded use, not multi-tenant hosting

## Status

This repository is best understood as an embedded broker workbench, not as a drop-in replacement for Mosquitto or EMQX.

It is optimized for:

- private deployments
- embedded controllers
- predictable memory use
- custom transports
- constrained TLS endpoints

## License

MIT
