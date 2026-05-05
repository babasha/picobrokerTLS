# picobrokerTLS — workspace

A Cargo workspace hosting a small family of crates for embedded TLS-secured
MQTT brokers. Each crate has its own README, version, and license; this
top-level page is just an index.

| Crate | Path | Purpose | License |
|---|---|---|---|
| [`GatoMQTT`](crates/GatoMQTT/README.md) | `crates/GatoMQTT/` | `no_std` MQTT 3.1.1 broker library | MIT |
| [`GatoPSKTLS`](crates/GatoPSKTLS/README.md) | `crates/GatoPSKTLS/` | TLS 1.3 PSK_KE client + server (`no_std`); fork of `drogue-iot/embedded-tls` with server-mode added | Apache-2.0 |
| `smartbox-config` | `crates/smartbox-config/` | Schema-versioned config compiler/storage for SmartBox firmware | MIT |

## Companion artifacts (not published)

| Path | Purpose |
|---|---|
| `examples/esp32c3_mqtt_server/` | ESP32-C3 firmware that combines `GatoMQTT` + `GatoPSKTLS` over Wi-Fi + embassy-net |
| `tools/host_tls_psk_server/` | tokio-based host server wrapper around `GatoPSKTLS::server` — used for `openssl s_client` / `mbedtls ssl_client2` interop testing |
| `tools/tls-spike/` | RFC 8448 §4 PSK key-schedule reproduction (decision gate during initial development of GatoPSKTLS) |
| `tools/Run-Tls13PskHandshake.ps1` | Helper that builds mbedtls 3.6.5's `ssl_client2` and connects to a PSK-only TLS server (used to test against ESP32-C3) |
| `vendor/mbedtls-rs/` | Local-only path-dep used by older ESP32-C3 builds; superseded by `GatoPSKTLS`. Gitignored. |

## Building

The workspace builds with stable Rust on Linux/macOS. On Windows MSVC,
`GatoPSKTLS`'s `openssl` dev-dep can't find system libssl, so use WSL with
a Linux-only `CARGO_TARGET_DIR`:

```sh
# Workspace tests (all three crates)
wsl bash -lc 'cd /mnt/c/.../picobrokerTLS && \
  CARGO_TARGET_DIR=$HOME/.cache/picobroker-target \
  cargo test --workspace --lib --features tls-psk'
```

ESP32-C3 firmware (separate workspace, has its own `[workspace]` block in
the example's `Cargo.toml`):

```sh
cd examples/esp32c3_mqtt_server
cargo build --release --features tls
espflash flash --monitor target/riscv32imc-unknown-none-elf/release/esp32c3_mqtt_server
```

## Layout philosophy

* `crates/` — first-party libraries; each is publishable to crates.io.
* `examples/` — end-product binaries that combine multiple crates.
* `tools/` — interop testers, debug helpers, host-only utilities.
* `vendor/` — third-party path-deps (gitignored where licensing or size matters).
* `third_party/` — locally checked-out source trees of third-party deps.

## License

Each crate carries its own `LICENSE` file:
* `GatoMQTT` — MIT
* `GatoPSKTLS` — Apache-2.0 (with NOTICE attributing upstream)
* `smartbox-config` — MIT

The workspace as a whole has no overarching license; it is the union of the
licenses listed above.
