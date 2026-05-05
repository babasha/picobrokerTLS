//! Host-side TCP TLS 1.3 PSK_KE server, used for interop tests against
//! external clients (openssl s_client, mbedtls ssl_client2, mosquitto, …).
//!
//! Protocol flow per connection:
//!   1. Read one TLSPlaintext Handshake record (ClientHello).
//!   2. Run `TlsServerSession::process_client_hello` and write the produced
//!      bytes (ServerHello + encrypted EncryptedExtensions + encrypted
//!      Finished) verbatim to the transport.
//!   3. Drain any TLSPlaintext ChangeCipherSpec records the peer sends for
//!      "middlebox compatibility" (RFC 8446 §D.4); they have no effect.
//!   4. Read one TLSCiphertext record (client Finished) and run
//!      `process_client_finished`.
//!   5. Enter a simple echo loop: read encrypted record, decrypt, log it,
//!      encrypt a reply and write it back.
//!
//! Caveats:
//!   * Single cipher suite (TLS_AES_128_GCM_SHA256), psk_ke only.
//!   * No graceful close-notify yet; we drop the socket on first read error.
//!   * Records are read into 18 KiB buffers, sized for the TLS 1.3 max
//!     plaintext (16 KiB) + AEAD tag + record header.

use anyhow::{Context, Result, bail};
use clap::Parser;
use gatopsktls::server::{TlsServerConfig, TlsServerSession};
use log::{debug, error, info, warn};
use rand::RngCore;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const RECORD_HEADER_LEN: usize = 5;
const MAX_RECORD_LEN: usize = 18 * 1024;

const TLS_CT_CHANGE_CIPHER_SPEC: u8 = 20;
const TLS_CT_HANDSHAKE: u8 = 22;
const TLS_CT_APPLICATION_DATA: u8 = 23;

#[derive(Parser, Debug)]
#[command(about = "TLS 1.3 PSK_KE host server (interop tester)")]
struct Args {
    /// Address to bind, e.g. 127.0.0.1:8443.
    #[arg(short, long, default_value = "127.0.0.1:8443")]
    bind: SocketAddr,

    /// PSK identity sent by clients on the wire.
    #[arg(long, default_value = "smartbox-house-1")]
    psk_identity: String,

    /// PSK secret as a hex string (e.g. 64 hex chars for 32 bytes).
    #[arg(long, default_value = "4242424242424242424242424242424242424242424242424242424242424242")]
    psk_hex: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let psk = hex::decode(&args.psk_hex).context("decode --psk-hex")?;
    let identity = args.psk_identity.into_bytes();

    let listener = TcpListener::bind(args.bind).await.context("bind")?;
    info!(
        "listening on {} (PSK identity = {:?}, key length = {} bytes)",
        listener.local_addr()?,
        std::str::from_utf8(&identity).unwrap_or("<binary>"),
        psk.len(),
    );

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                error!("accept failed: {e}");
                continue;
            }
        };
        let identity = identity.clone();
        let psk = psk.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, peer, &identity, &psk).await {
                warn!("[{peer}] connection ended with error: {e:#}");
            } else {
                info!("[{peer}] connection closed cleanly");
            }
        });
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    identity: &[u8],
    psk: &[u8],
) -> Result<()> {
    info!("[{peer}] new TCP connection");

    // ── 1. Read ClientHello as one TLSPlaintext Handshake record. ────────
    let ch_record = read_record(&mut stream).await.context("read ClientHello record")?;
    if ch_record[0] != TLS_CT_HANDSHAKE {
        bail!(
            "first record is not Handshake (type={}, len={})",
            ch_record[0],
            ch_record.len()
        );
    }
    let ch_handshake = &ch_record[RECORD_HEADER_LEN..];
    debug!("[{peer}] received ClientHello: {} bytes", ch_handshake.len());

    // ── 2. Build session, run process_client_hello, write first flight. ──
    let mut server_random = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut server_random);

    let config = TlsServerConfig {
        psk: (identity, psk),
        server_random,
    };
    let mut session = TlsServerSession::new();
    let mut out = vec![0u8; 4 * 1024];
    let flight_len = {
        let flight = session
            .process_client_hello(ch_handshake, &config, &mut out)
            .map_err(|e| anyhow::anyhow!("process_client_hello: {e:?}"))?;
        flight.len()
    };
    stream
        .write_all(&out[..flight_len])
        .await
        .context("write server first flight")?;
    stream.flush().await.context("flush after first flight")?;
    debug!("[{peer}] wrote server first flight: {flight_len} bytes");

    // ── 3. Drain any ChangeCipherSpec records (middlebox-compat dummy). ──
    let finished_record = loop {
        let rec = read_record(&mut stream)
            .await
            .context("read encrypted client Finished")?;
        match rec[0] {
            TLS_CT_CHANGE_CIPHER_SPEC => {
                debug!("[{peer}] ignoring ChangeCipherSpec ({} bytes)", rec.len());
                continue;
            }
            TLS_CT_APPLICATION_DATA => break rec,
            other => bail!("unexpected record type before Finished: {other}"),
        }
    };

    // ── 4. Decrypt + verify client Finished. ─────────────────────────────
    session
        .process_client_finished(&finished_record)
        .map_err(|e| anyhow::anyhow!("process_client_finished: {e:?}"))?;
    info!("[{peer}] handshake complete");

    // ── 5. Echo loop. ────────────────────────────────────────────────────
    let mut plain_buf = vec![0u8; MAX_RECORD_LEN];
    let mut tx_buf = vec![0u8; MAX_RECORD_LEN];
    loop {
        let rec = match read_record(&mut stream).await {
            Ok(r) => r,
            Err(e) => {
                debug!("[{peer}] read loop ending: {e}");
                break;
            }
        };
        if rec[0] == TLS_CT_CHANGE_CIPHER_SPEC {
            // Some clients keep sending CCS dummies; just ignore.
            continue;
        }
        if rec[0] != TLS_CT_APPLICATION_DATA {
            warn!("[{peer}] unexpected post-handshake record type {}", rec[0]);
            break;
        }

        let plaintext = match session.decrypt_app_data(&rec, &mut plain_buf) {
            Ok(p) => p.to_vec(),
            Err(e) => {
                warn!("[{peer}] decrypt_app_data failed: {e:?}");
                break;
            }
        };
        info!(
            "[{peer}] received {} bytes: {:?}",
            plaintext.len(),
            String::from_utf8_lossy(&plaintext),
        );

        // Echo a tagged response so the client can confirm the round-trip.
        let mut response: Vec<u8> = Vec::with_capacity(plaintext.len() + 6);
        response.extend_from_slice(b"echo: ");
        response.extend_from_slice(&plaintext);
        let cipher_len = {
            let bytes = session
                .encrypt_app_data(&response, &mut tx_buf)
                .map_err(|e| anyhow::anyhow!("encrypt_app_data: {e:?}"))?;
            bytes.len()
        };
        stream
            .write_all(&tx_buf[..cipher_len])
            .await
            .context("write echo")?;
        stream.flush().await?;
    }

    Ok(())
}

/// Read exactly one TLS 1.x record (5-byte header + body) from `stream`.
async fn read_record(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut header = [0u8; RECORD_HEADER_LEN];
    stream
        .read_exact(&mut header)
        .await
        .context("read record header")?;
    let body_len = u16::from_be_bytes([header[3], header[4]]) as usize;
    if body_len > MAX_RECORD_LEN - RECORD_HEADER_LEN {
        bail!(
            "record body too large: {} bytes (max {})",
            body_len,
            MAX_RECORD_LEN - RECORD_HEADER_LEN
        );
    }
    let mut record = Vec::with_capacity(RECORD_HEADER_LEN + body_len);
    record.extend_from_slice(&header);
    record.resize(RECORD_HEADER_LEN + body_len, 0);
    stream
        .read_exact(&mut record[RECORD_HEADER_LEN..])
        .await
        .context("read record body")?;
    Ok(record)
}
