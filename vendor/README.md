# vendor/

This directory holds private path-dependencies that are not published to crates.io.

## mbedtls-rs

Used by `examples/esp32c3_mqtt_server` (TLS feature).

The `examples/esp32c3_mqtt_server/Cargo.toml` resolves it via:

```toml
mbedtls-rs = { path = "../../vendor/mbedtls-rs/mbedtls-rs", ... }
```

Place (or clone) your copy of the `mbedtls-rs` workspace here so the layout is:

```
vendor/
  mbedtls-rs/
    mbedtls-rs/          ← the Rust crate
    mbedtls-rs-sys/      ← the -sys crate + C source
```

The directory is listed in `.gitignore` because it contains a large C library (~1 900 files).
