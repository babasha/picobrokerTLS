# Architecture Notes

This document is a short orientation guide for the current `GatoMQTT` codebase.

## Runtime Shape

The broker core is transport-agnostic.

It expects a type implementing the transport abstraction in [`src/transport.rs`](../src/transport.rs) and then drives MQTT packet parsing, routing and session state on top of that transport.

The important consequence is that TCP, TLS and board-specific sockets stay outside the broker logic.

## Main Components

- [`src/broker.rs`](../src/broker.rs)
  Main broker loop and orchestration.
- [`src/handler/connect.rs`](../src/handler/connect.rs)
  CONNECT validation, "house token" auth, session insertion and duplicate-client handling.
- [`src/handler/publish.rs`](../src/handler/publish.rs)
  Publish routing, retained updates, rate limiting and QoS 1 retry handling.
- [`src/router/dispatcher.rs`](../src/router/dispatcher.rs)
  Subscriber matching and effective QoS selection.
- [`src/router/retained.rs`](../src/router/retained.rs)
  Fixed-capacity retained message store.
- [`src/session/registry.rs`](../src/session/registry.rs)
  Fixed-slot session registry plus published-LWT queue.
- [`src/session/state.rs`](../src/session/state.rs)
  Per-session state: subscriptions, inflight QoS 1 entries, keepalive, LWT and outbox.
- [`src/session/rate_limit.rs`](../src/session/rate_limit.rs)
  Token-bucket limiter used on incoming publishes.

## Connection Flow

1. A transport is accepted by the application example.
2. The broker decodes MQTT packets from that transport.
3. `CONNECT` is validated against protocol version, client ID and configured credentials.
4. A session slot is allocated in the registry.
5. Subsequent `SUBSCRIBE`, `PUBLISH`, `PINGREQ` and `DISCONNECT` packets are dispatched to handlers.

If a new client connects with an existing `client_id`, the old session is displaced and its LWT is queued for publication.

## Publish Flow

For inbound `PUBLISH`, the broker currently does the following:

1. Apply per-session rate limiting.
2. Update the retained store if the retain flag is set.
3. Find subscribers whose filters match the topic.
4. Encode outbound `PUBLISH` frames into per-session outboxes.
5. Mirror command topics matching `sb/+/device/+/set` into the local inbound queue.

The sender does not receive its own publish back through normal subscriber routing.

## QoS Model

The current model is intentionally bounded.

- `QoS 0` is straightforward fire-and-forget delivery.
- `QoS 1` uses fixed-capacity inflight tracking per session.
- retry timing and retry count are controlled by `GATOMQTT_CONFIG`.
- `QoS 2` is not implemented.

This is enough for small control systems, but it is not a full durable broker implementation.

## Retained Messages

Retained messages are kept in a fixed-capacity in-memory store.

- setting retain with a non-empty payload inserts or updates the retained entry
- setting retain with an empty payload removes the retained entry
- wildcard matching is supported when replaying retained messages to new subscriptions

There is currently no broker-level persistence layer for retained data across reboot.

## Authentication Model

Authentication is currently simple by design.

- a single configured username/password pair is stored in [`src/config.rs`](../src/config.rs)
- comparison is done with a constant-time equality helper
- there is no ACL engine or per-device auth policy yet

This matches the current closed-infrastructure use case, but it is not a general multi-user auth system.

## TLS Integration

TLS is handled outside the broker core.

The ESP32-C3 example is the main reference:

- the app owns the TLS context and socket lifecycle
- the broker still sees only a transport
- the TLS example uses `TLS 1.3 + pure PSK`
- the companion `mbedtls-rs` fork shares one prepared TLS config across many sessions

That split keeps broker logic independent from any specific TLS stack.

## Tuning Knobs

The first place to tune runtime limits is [`src/config.rs`](../src/config.rs).

The most important knobs are:

- `max_sessions`
- `max_subscriptions`
- `max_inflight`
- `max_retained`
- `max_payload_len`
- `rate_capacity`
- `rate_per_sec`
- `max_violations`
- `qos1_retry_ms`
- `qos1_max_retries`

When changing them, remember that this codebase is designed around static memory budgets.
