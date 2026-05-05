// Phase 0 spike: reproduce RFC 8448 §4 PSK binder math from scratch.
//
// Decision gate: if every assertion below passes, we have an
// independent, correct understanding of the TLS 1.3 PSK key schedule
// and binder construction — the math part of server-mode is feasible.
//
// Run:  cargo run --manifest-path tools/tls-spike/Cargo.toml

use hex_literal::hex;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

// HKDF-Extract(salt, IKM) per RFC 5869.
fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> [u8; 32] {
    let zeros = [0u8; 32];
    let salt = if salt.is_empty() { &zeros[..] } else { salt };
    let mut mac = HmacSha256::new_from_slice(salt).expect("HMAC accepts any key length");
    mac.update(ikm);
    mac.finalize().into_bytes().into()
}

// HKDF-Expand-Label per RFC 8446 §7.1.
//   HkdfLabel = uint16 length || u8 label_len || ("tls13 " ++ label) || u8 ctx_len || context
fn hkdf_expand_label(secret: &[u8; 32], label: &str, context: &[u8], length: u16) -> Vec<u8> {
    let full_label = format!("tls13 {}", label);
    assert!(full_label.len() <= 255);
    assert!(context.len() <= 255);

    let mut info = Vec::with_capacity(2 + 1 + full_label.len() + 1 + context.len());
    info.extend_from_slice(&length.to_be_bytes());
    info.push(full_label.len() as u8);
    info.extend_from_slice(full_label.as_bytes());
    info.push(context.len() as u8);
    info.extend_from_slice(context);

    // HKDF-Expand: T(0)=empty; T(i) = HMAC(prk, T(i-1) || info || i)
    let mut out = Vec::with_capacity(length as usize);
    let mut prev: Vec<u8> = Vec::new();
    let mut counter: u8 = 1;
    while out.len() < length as usize {
        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(&prev);
        mac.update(&info);
        mac.update(&[counter]);
        prev = mac.finalize().into_bytes().to_vec();
        out.extend_from_slice(&prev);
        counter = counter.checked_add(1).expect("HKDF block counter overflow");
    }
    out.truncate(length as usize);
    out
}

// Derive-Secret per RFC 8446 §7.1.
//   Derive-Secret(Secret, Label, Messages) = HKDF-Expand-Label(Secret, Label, Hash(Messages), Hash.length)
fn derive_secret(secret: &[u8; 32], label: &str, messages: &[u8]) -> [u8; 32] {
    let messages_hash: [u8; 32] = Sha256::digest(messages).into();
    let v = hkdf_expand_label(secret, label, &messages_hash, 32);
    v.try_into().unwrap()
}

// 477-octet ClientHello prefix from RFC 8448 §4 (lines 921..944 of rfc8448.txt),
// loaded at compile time from the extracted hex blob.
const CLIENT_HELLO_PREFIX_HEX: &str = include_str!("../clienthello_prefix.hex");
// 512-octet full ClientHello payload (with binders) — lines 972..996.
const CLIENT_HELLO_FULL_HEX: &str = include_str!("../clienthello_full.hex");
// 96-octet ServerHello — lines 1110..1114.
const SERVER_HELLO_HEX: &str = include_str!("../serverhello.hex");

fn parse_hex_blob(s: &str) -> Vec<u8> {
    let cleaned: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    assert!(cleaned.len() % 2 == 0, "odd hex length");
    (0..cleaned.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).unwrap())
        .collect()
}

fn check(name: &str, got: &[u8], expected: &[u8]) {
    if got == expected {
        println!("  OK  {}", name);
    } else {
        println!("FAIL  {}", name);
        println!("       got:      {}", hex::encode_lower(got));
        println!("       expected: {}", hex::encode_lower(expected));
        std::process::exit(1);
    }
}

mod hex {
    pub fn encode_lower(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{:02x}", b));
        }
        s
    }
}

fn main() {
    println!("Phase 0 spike — RFC 8448 §4 (Resumed 0-RTT) PSK binder math\n");

    // ── Inputs from RFC 8448 §4 ─────────────────────────────────────────────
    // PSK / IKM for early-secret extraction (line 878-879).
    let psk = hex!(
        "4ecd0eb6ec3b4d87f5d6028f922ca4c5"
        "851a277fd41311c9e62d2c9492e1c4f3"
    );

    let client_hello_prefix = parse_hex_blob(CLIENT_HELLO_PREFIX_HEX);
    assert_eq!(client_hello_prefix.len(), 477, "prefix must be 477 bytes");

    // ── Expected outputs from RFC 8448 §4 ──────────────────────────────────
    // early_secret (line 881-882)
    let exp_early_secret = hex!(
        "9b2188e9b2fc6d64d71dc329900e20bb"
        "41915000f678aa839cbb797cb7d8332c"
    );
    // binder_hash = SHA256(client_hello_prefix) (line 946-947)
    let exp_binder_hash = hex!(
        "63224b2e4573f2d3454ca84b9d009a04"
        "f6be9e05711a8396473aefa01e924a14"
    );
    // binder_key = Derive-Secret(early_secret, "res binder", "") (line 949-950, "PRK")
    let exp_binder_key = hex!(
        "69fe131a3bbad5d63c64eebcc30e395b"
        "9d8107726a13d074e389dbc8a4e47256"
    );
    // finished_key = HKDF-Expand-Label(binder_key, "finished", "", 32) (line 964-965, "expanded")
    let exp_finished_key = hex!(
        "5588673e72cb59c87d220caffe94f2de"
        "a9a3b1609f7d50e90a48227db9ed7eaa"
    );
    // binder = HMAC-SHA256(finished_key, binder_hash) (line 967-968, "finished")
    let exp_binder = hex!(
        "3add4fb2d8fdf822a0ca3cf7678ef5e8"
        "8dae990141c5924d57bb6fa31b9e5f9d"
    );

    // ── Computations ───────────────────────────────────────────────────────

    // early_secret = HKDF-Extract(salt=zeros, IKM=PSK)
    let early_secret = hkdf_extract(&[], &psk);
    check("early_secret = HKDF-Extract(0, PSK)", &early_secret, &exp_early_secret);

    // binder_key = Derive-Secret(early_secret, "res binder", "")
    let binder_key = derive_secret(&early_secret, "res binder", b"");
    check("binder_key   = Derive-Secret(early, \"res binder\", \"\")", &binder_key, &exp_binder_key);

    // binder_hash = SHA-256(ClientHello prefix)
    let binder_hash: [u8; 32] = Sha256::digest(&client_hello_prefix).into();
    check("binder_hash  = SHA-256(ClientHello-prefix)", &binder_hash, &exp_binder_hash);

    // finished_key = HKDF-Expand-Label(binder_key, "finished", "", 32)
    let finished_key = hkdf_expand_label(&binder_key, "finished", b"", 32);
    check("finished_key = HKDF-Expand-Label(binder_key, \"finished\", \"\", 32)", &finished_key, &exp_finished_key);

    // binder = HMAC-SHA256(finished_key, binder_hash)
    let mut mac = HmacSha256::new_from_slice(&finished_key).unwrap();
    mac.update(&binder_hash);
    let binder: [u8; 32] = mac.finalize().into_bytes().into();
    check("binder       = HMAC(finished_key, binder_hash)", &binder, &exp_binder);

    println!();
    println!("── Full server-side key schedule (§4 cont'd) ──────────────────────────");

    // ── ECDHE shared secret ────────────────────────────────────────────────
    // Server's static ephemeral private key (line 1098-1099) and client's public
    // key (line 871-872). The IKM at line 1147-1148 is the X25519 shared secret;
    // we recompute it independently and verify byte-for-byte.
    let server_priv = hex!(
        "de5b4476e7b490b2652d338acbf29480"
        "66f255f9440e23b98fc69835298dc107"
    );
    let client_pub = hex!(
        "e4ffb68ac05f8d96c99da26698346c6b"
        "e16482badddafe051a66b4f18d668f0b"
    );
    let exp_ecdhe_shared = hex!(
        "f44194756ff9ec9d25180635d66ea682"
        "4c6ab3bf179977be37f723570e7ccb2e"
    );

    let server_secret = x25519_dalek::StaticSecret::from(server_priv);
    let client_pub_pt = x25519_dalek::PublicKey::from(client_pub);
    let ecdhe_shared = server_secret.diffie_hellman(&client_pub_pt);
    check("ECDHE shared = X25519(server_priv, client_pub)", ecdhe_shared.as_bytes(), &exp_ecdhe_shared);

    // ── derived_for_handshake = Derive-Secret(early_secret, "derived", "")
    // (line 1139-1140)
    let exp_derived_for_handshake = hex!(
        "5f1790bbd82c5e7d376ed2e1e52f8e60"
        "38c9346db61b43be9a52f77ef3998e80"
    );
    let derived_for_handshake = derive_secret(&early_secret, "derived", b"");
    check("derived (handshake) = Derive-Secret(early, \"derived\", \"\")",
        &derived_for_handshake, &exp_derived_for_handshake);

    // ── handshake_secret = HKDF-Extract(salt=derived, IKM=ECDHE_shared)
    // (line 1150-1151)
    let exp_handshake_secret = hex!(
        "005cb112fd8eb4ccc623bb88a07c64b3"
        "ede1605363fc7d0df8c7ce4ff0fb4ae6"
    );
    let handshake_secret = hkdf_extract(&derived_for_handshake, ecdhe_shared.as_bytes());
    check("handshake_secret = HKDF-Extract(derived, ECDHE)",
        &handshake_secret, &exp_handshake_secret);

    // ── transcript_hash after ClientHello+ServerHello (line 1158-1159)
    let client_hello_full = parse_hex_blob(CLIENT_HELLO_FULL_HEX);
    let server_hello = parse_hex_blob(SERVER_HELLO_HEX);
    assert_eq!(client_hello_full.len(), 512);
    assert_eq!(server_hello.len(), 96);
    let exp_th_after_sh = hex!(
        "f736cb34fe25e701551bee6fd24c1cc7"
        "102a7daf9405cb15d97aafe16f757d03"
    );
    let mut th_after_sh_hasher = Sha256::new();
    th_after_sh_hasher.update(&client_hello_full);
    th_after_sh_hasher.update(&server_hello);
    let th_after_sh: [u8; 32] = th_after_sh_hasher.finalize().into();
    check("TH(CH||SH)", &th_after_sh, &exp_th_after_sh);

    // ── client_handshake_traffic_secret (line 1165-1166)
    let exp_chts = hex!(
        "2faac08f851d35fea3604fcb4de82dc6"
        "2c9b164a70974d0462e27f1ab278700f"
    );
    // Derive-Secret(handshake_secret, "c hs traffic", CH||SH)
    //   = HKDF-Expand-Label(handshake_secret, "c hs traffic", Hash(CH||SH), 32)
    let chts = hkdf_expand_label(&handshake_secret, "c hs traffic", &th_after_sh, 32);
    check("client_handshake_traffic_secret", &chts, &exp_chts);

    // ── server_handshake_traffic_secret (line 1187-1188)
    let exp_shts = hex!(
        "fe927ae271312e8bf0275b581c54eef0"
        "20450dc4ecffaa05a1a35d27518e7803"
    );
    let shts = hkdf_expand_label(&handshake_secret, "s hs traffic", &th_after_sh, 32);
    check("server_handshake_traffic_secret", &shts, &exp_shts);

    // ── derived_for_master = Derive-Secret(handshake_secret, "derived", "")
    // (line 1202-1203)
    let exp_derived_for_master = hex!(
        "e2f16030251df0874ba19b9aba257610"
        "bc6d531c1dd206df0ca6e84ae2a26742"
    );
    let derived_for_master = derive_secret(&handshake_secret, "derived", b"");
    check("derived (master) = Derive-Secret(handshake, \"derived\", \"\")",
        &derived_for_master, &exp_derived_for_master);

    // ── master_secret = HKDF-Extract(salt=derived_for_master, IKM=zeros)
    // (line 1213-1214)
    let exp_master = hex!(
        "e2d32d4ed66dd37897a0e80c84107503"
        "ce58bf8aad4cb55a5002d77ecb890ece"
    );
    let master_secret = hkdf_extract(&derived_for_master, &[0u8; 32]);
    check("master_secret = HKDF-Extract(derived, 0)", &master_secret, &exp_master);

    // ── Server's AEAD write keys for handshake-encrypted records:
    // server_handshake_key = HKDF-Expand-Label(shts, "key", "", 16) (line 1246-1247)
    let exp_server_hs_key = hex!("27c6bdc0a3dcea39a47326d79bc9e4ee");
    let server_hs_key = hkdf_expand_label(&shts.clone().try_into().unwrap(), "key", &[], 16);
    check("server handshake AEAD key", &server_hs_key, &exp_server_hs_key);

    // server_handshake_iv = HKDF-Expand-Label(shts, "iv", "", 12) (line 1251)
    let exp_server_hs_iv = hex!("9569ecdd4d0536705e9ef725");
    let server_hs_iv = hkdf_expand_label(&shts.clone().try_into().unwrap(), "iv", &[], 12);
    check("server handshake AEAD iv", &server_hs_iv, &exp_server_hs_iv);

    // server_finished_key = HKDF-Expand-Label(shts, "finished", "", 32) (line 1269-1270)
    let exp_server_finished_key = hex!(
        "4bb74cae7a5dc8914604c0bfbe2f0c06"
        "23968839 22bec8a15e2a9b532a5d392c"
    );
    let server_finished_key = hkdf_expand_label(&shts.clone().try_into().unwrap(), "finished", &[], 32);
    check("server_finished_key", &server_finished_key, &exp_server_finished_key);

    println!();
    println!("All RFC 8448 §4 server-side key-schedule values reproduced byte-for-byte.");
    println!("Phase 0 spike: GREEN — full TLS 1.3 PSK_DHE_KE math layer is correctly understood.");
}
