use heapless::{String, Vec};
use sha2::{Digest, Sha256};

pub const MAX_CERTIFICATE_DER_LEN: usize = 1024;
pub const MAX_PRIVATE_KEY_DER_LEN: usize = 256;
pub const FINGERPRINT_HEX_LEN: usize = 95;
const STORAGE_MAGIC: [u8; 4] = *b"GMTL";
const STORAGE_VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsIdentity<const CERT_MAX: usize, const KEY_MAX: usize> {
    pub certificate_der: Vec<u8, CERT_MAX>,
    pub private_key_der: Vec<u8, KEY_MAX>,
}

impl<const CERT_MAX: usize, const KEY_MAX: usize> TlsIdentity<CERT_MAX, KEY_MAX> {
    pub fn new(certificate_der: &[u8], private_key_der: &[u8]) -> Result<Self, IdentityError> {
        Ok(Self {
            certificate_der: Vec::from_slice(certificate_der)
                .map_err(|_| IdentityError::CertificateTooLarge)?,
            private_key_der: Vec::from_slice(private_key_der)
                .map_err(|_| IdentityError::PrivateKeyTooLarge)?,
        })
    }

    pub fn fingerprint_sha256(&self) -> [u8; 32] {
        fingerprint_sha256(self.certificate_der.as_slice())
    }

    pub fn fingerprint_hex(&self) -> String<FINGERPRINT_HEX_LEN> {
        fingerprint_hex(&self.fingerprint_sha256())
    }

    pub fn encode<const STORAGE_MAX: usize>(&self) -> Result<Vec<u8, STORAGE_MAX>, IdentityError> {
        let cert_len: u16 = self
            .certificate_der
            .len()
            .try_into()
            .map_err(|_| IdentityError::CertificateTooLarge)?;
        let key_len: u16 = self
            .private_key_der
            .len()
            .try_into()
            .map_err(|_| IdentityError::PrivateKeyTooLarge)?;

        let total_len = 9 + usize::from(cert_len) + usize::from(key_len);
        if total_len > STORAGE_MAX {
            return Err(IdentityError::StorageBufferTooSmall);
        }

        let mut out = Vec::<u8, STORAGE_MAX>::new();
        out.extend_from_slice(&STORAGE_MAGIC)
            .map_err(|_| IdentityError::StorageBufferTooSmall)?;
        out.push(STORAGE_VERSION)
            .map_err(|_| IdentityError::StorageBufferTooSmall)?;
        out.extend_from_slice(&cert_len.to_le_bytes())
            .map_err(|_| IdentityError::StorageBufferTooSmall)?;
        out.extend_from_slice(&key_len.to_le_bytes())
            .map_err(|_| IdentityError::StorageBufferTooSmall)?;
        out.extend_from_slice(self.certificate_der.as_slice())
            .map_err(|_| IdentityError::StorageBufferTooSmall)?;
        out.extend_from_slice(self.private_key_der.as_slice())
            .map_err(|_| IdentityError::StorageBufferTooSmall)?;
        Ok(out)
    }

    pub fn decode(storage: &[u8]) -> Result<Self, IdentityError> {
        if storage.len() < 9 {
            return Err(IdentityError::CorruptedStorage);
        }
        if storage[..4] != STORAGE_MAGIC {
            return Err(IdentityError::InvalidStorageMagic);
        }
        if storage[4] != STORAGE_VERSION {
            return Err(IdentityError::UnsupportedStorageVersion(storage[4]));
        }

        let cert_len = u16::from_le_bytes([storage[5], storage[6]]) as usize;
        let key_len = u16::from_le_bytes([storage[7], storage[8]]) as usize;
        let total_len = 9 + cert_len + key_len;
        if storage.len() != total_len {
            return Err(IdentityError::CorruptedStorage);
        }

        let cert_start = 9;
        let cert_end = cert_start + cert_len;
        let key_end = cert_end + key_len;
        Self::new(&storage[cert_start..cert_end], &storage[cert_end..key_end])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityError {
    CertificateTooLarge,
    PrivateKeyTooLarge,
    StorageBufferTooSmall,
    InvalidStorageMagic,
    UnsupportedStorageVersion(u8),
    CorruptedStorage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootstrapError<StoreError, GeneratorError> {
    Load(StoreError),
    Save(StoreError),
    Generate(GeneratorError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrappedIdentity<const CERT_MAX: usize, const KEY_MAX: usize> {
    pub identity: TlsIdentity<CERT_MAX, KEY_MAX>,
    pub fingerprint_sha256: [u8; 32],
    pub generated: bool,
}

pub trait TlsIdentityStore<const CERT_MAX: usize, const KEY_MAX: usize> {
    type Error;

    fn load(&mut self) -> Result<Option<TlsIdentity<CERT_MAX, KEY_MAX>>, Self::Error>;
    fn save(
        &mut self,
        identity: &TlsIdentity<CERT_MAX, KEY_MAX>,
    ) -> Result<(), Self::Error>;
}

pub trait TlsIdentityGenerator<const CERT_MAX: usize, const KEY_MAX: usize> {
    type Error;

    fn generate(&mut self) -> Result<TlsIdentity<CERT_MAX, KEY_MAX>, Self::Error>;
}

pub fn ensure_tls_identity<
    Store,
    Generator,
    const CERT_MAX: usize,
    const KEY_MAX: usize,
>(
    store: &mut Store,
    generator: &mut Generator,
) -> Result<BootstrappedIdentity<CERT_MAX, KEY_MAX>, BootstrapError<Store::Error, Generator::Error>>
where
    Store: TlsIdentityStore<CERT_MAX, KEY_MAX>,
    Generator: TlsIdentityGenerator<CERT_MAX, KEY_MAX>,
{
    if let Some(identity) = store.load().map_err(BootstrapError::Load)? {
        let fingerprint_sha256 = identity.fingerprint_sha256();
        return Ok(BootstrappedIdentity {
            identity,
            fingerprint_sha256,
            generated: false,
        });
    }

    let identity = generator.generate().map_err(BootstrapError::Generate)?;
    store.save(&identity).map_err(BootstrapError::Save)?;
    let fingerprint_sha256 = identity.fingerprint_sha256();
    Ok(BootstrappedIdentity {
        identity,
        fingerprint_sha256,
        generated: true,
    })
}

pub fn fingerprint_sha256(certificate_der: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(certificate_der);
    hasher.finalize().into()
}

pub fn fingerprint_hex(fingerprint: &[u8; 32]) -> String<FINGERPRINT_HEX_LEN> {
    let mut out = String::<FINGERPRINT_HEX_LEN>::new();
    for (index, byte) in fingerprint.iter().copied().enumerate() {
        if index > 0 {
            let _ = out.push(':');
        }

        push_hex_byte(&mut out, byte);
    }
    out
}

fn push_hex_byte(out: &mut String<FINGERPRINT_HEX_LEN>, byte: u8) {
    let _ = out.push(hex_nibble(byte >> 4));
    let _ = out.push(hex_nibble(byte & 0x0F));
}

const fn hex_nibble(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_tls_identity, fingerprint_hex, fingerprint_sha256, BootstrappedIdentity,
        BootstrapError, IdentityError, TlsIdentity, TlsIdentityGenerator, TlsIdentityStore,
    };
    use std::vec;

    const CERT_MAX: usize = 128;
    const KEY_MAX: usize = 64;
    const STORAGE_MAX: usize = 256;

    #[derive(Default)]
    struct MockStore {
        identity: Option<TlsIdentity<CERT_MAX, KEY_MAX>>,
        save_calls: usize,
        fail_load: bool,
        fail_save: bool,
    }

    impl TlsIdentityStore<CERT_MAX, KEY_MAX> for MockStore {
        type Error = &'static str;

        fn load(&mut self) -> Result<Option<TlsIdentity<CERT_MAX, KEY_MAX>>, Self::Error> {
            if self.fail_load {
                return Err("load failed");
            }
            Ok(self.identity.clone())
        }

        fn save(
            &mut self,
            identity: &TlsIdentity<CERT_MAX, KEY_MAX>,
        ) -> Result<(), Self::Error> {
            if self.fail_save {
                return Err("save failed");
            }
            self.save_calls += 1;
            self.identity = Some(identity.clone());
            Ok(())
        }
    }

    struct MockGenerator {
        identity: TlsIdentity<CERT_MAX, KEY_MAX>,
        calls: usize,
        fail: bool,
    }

    impl TlsIdentityGenerator<CERT_MAX, KEY_MAX> for MockGenerator {
        type Error = &'static str;

        fn generate(&mut self) -> Result<TlsIdentity<CERT_MAX, KEY_MAX>, Self::Error> {
            if self.fail {
                return Err("generate failed");
            }
            self.calls += 1;
            Ok(self.identity.clone())
        }
    }

    fn identity() -> TlsIdentity<CERT_MAX, KEY_MAX> {
        TlsIdentity::new(b"fake-cert-der", b"fake-key-der").unwrap()
    }

    #[test]
    fn ensure_tls_identity_loads_existing_identity_without_generation() {
        let existing = identity();
        let mut store = MockStore {
            identity: Some(existing.clone()),
            ..MockStore::default()
        };
        let mut generator = MockGenerator {
            identity: existing.clone(),
            calls: 0,
            fail: false,
        };

        let bootstrapped = ensure_tls_identity(&mut store, &mut generator).unwrap();

        assert_eq!(
            bootstrapped,
            BootstrappedIdentity {
                fingerprint_sha256: fingerprint_sha256(existing.certificate_der.as_slice()),
                identity: existing,
                generated: false,
            }
        );
        assert_eq!(generator.calls, 0);
        assert_eq!(store.save_calls, 0);
    }

    #[test]
    fn ensure_tls_identity_generates_and_persists_when_store_is_empty() {
        let generated = identity();
        let mut store = MockStore::default();
        let mut generator = MockGenerator {
            identity: generated.clone(),
            calls: 0,
            fail: false,
        };

        let bootstrapped = ensure_tls_identity(&mut store, &mut generator).unwrap();

        assert!(bootstrapped.generated);
        assert_eq!(bootstrapped.identity, generated);
        assert_eq!(generator.calls, 1);
        assert_eq!(store.save_calls, 1);
        assert_eq!(store.identity, Some(generated));
    }

    #[test]
    fn ensure_tls_identity_propagates_store_and_generator_errors() {
        let mut store = MockStore {
            fail_load: true,
            ..MockStore::default()
        };
        let mut generator = MockGenerator {
            identity: identity(),
            calls: 0,
            fail: false,
        };
        assert_eq!(
            ensure_tls_identity(&mut store, &mut generator),
            Err(BootstrapError::Load("load failed"))
        );

        let mut store = MockStore::default();
        let mut generator = MockGenerator {
            identity: identity(),
            calls: 0,
            fail: true,
        };
        assert_eq!(
            ensure_tls_identity(&mut store, &mut generator),
            Err(BootstrapError::Generate("generate failed"))
        );

        let mut store = MockStore {
            fail_save: true,
            ..MockStore::default()
        };
        let mut generator = MockGenerator {
            identity: identity(),
            calls: 0,
            fail: false,
        };
        assert_eq!(
            ensure_tls_identity(&mut store, &mut generator),
            Err(BootstrapError::Save("save failed"))
        );
    }

    #[test]
    fn identity_storage_round_trip_preserves_certificate_and_key() {
        let identity = identity();
        let encoded = identity.encode::<STORAGE_MAX>().unwrap();
        let decoded = TlsIdentity::<CERT_MAX, KEY_MAX>::decode(encoded.as_slice()).unwrap();

        assert_eq!(decoded, identity);
    }

    #[test]
    fn identity_decode_rejects_invalid_storage_header() {
        let mut bad_magic = vec![0u8; 9];
        bad_magic[..4].copy_from_slice(b"NOPE");
        assert_eq!(
            TlsIdentity::<CERT_MAX, KEY_MAX>::decode(&bad_magic),
            Err(IdentityError::InvalidStorageMagic)
        );

        let mut bad_version = vec![0u8; 9];
        bad_version[..4].copy_from_slice(b"GMTL");
        bad_version[4] = 9;
        assert_eq!(
            TlsIdentity::<CERT_MAX, KEY_MAX>::decode(&bad_version),
            Err(IdentityError::UnsupportedStorageVersion(9))
        );
    }

    #[test]
    fn fingerprint_hex_formats_sha256_in_uppercase_colon_separated_form() {
        let fingerprint = fingerprint_sha256(b"abc");
        let hex = fingerprint_hex(&fingerprint);

        assert_eq!(
            hex.as_str(),
            "BA:78:16:BF:8F:01:CF:EA:41:41:40:DE:5D:AE:22:23:B0:03:61:A3:96:17:7A:9C:B4:10:FF:61:F2:00:15:AD"
        );
    }
}
