use serde::{Deserialize, Serialize};

const MAGIC: [u8; 4] = *b"SBCF";
const HEADER_LEN: usize = 20;

pub const FORMAT_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum BlobKind {
    CanonicalConfig = 1,
    CompiledArtifacts = 2,
}

impl BlobKind {
    const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::CanonicalConfig),
            2 => Some(Self::CompiledArtifacts),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedBlob<T> {
    pub kind: BlobKind,
    pub schema_version: u16,
    pub config_version: u32,
    pub value: T,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    BufferTooSmall,
    BadMagic,
    UnsupportedFormatVersion(u8),
    UnexpectedKind { expected: BlobKind, found: BlobKind },
    UnknownKind(u8),
    LengthMismatch,
    CrcMismatch { expected: u32, actual: u32 },
    PostcardSerialize,
    PostcardDeserialize,
}

pub fn encode_enveloped<'a, T: Serialize>(
    kind: BlobKind,
    schema_version: u16,
    config_version: u32,
    value: &T,
    out: &'a mut [u8],
) -> Result<&'a mut [u8], CodecError> {
    if out.len() < HEADER_LEN {
        return Err(CodecError::BufferTooSmall);
    }

    let payload_len = {
        let payload = postcard::to_slice(value, &mut out[HEADER_LEN..])
            .map_err(|_| CodecError::PostcardSerialize)?;
        payload.len()
    };

    let total_len = HEADER_LEN + payload_len;
    let crc = crc32(&out[HEADER_LEN..total_len]);

    out[0..4].copy_from_slice(&MAGIC);
    out[4] = FORMAT_VERSION;
    out[5] = kind as u8;
    out[6..8].copy_from_slice(&schema_version.to_be_bytes());
    out[8..12].copy_from_slice(&config_version.to_be_bytes());
    out[12..16].copy_from_slice(&(payload_len as u32).to_be_bytes());
    out[16..20].copy_from_slice(&crc.to_be_bytes());

    Ok(&mut out[..total_len])
}

pub fn decode_enveloped<'de, T: Deserialize<'de>>(
    expected_kind: BlobKind,
    input: &'de [u8],
) -> Result<DecodedBlob<T>, CodecError> {
    if input.len() < HEADER_LEN {
        return Err(CodecError::BufferTooSmall);
    }
    if input[0..4] != MAGIC {
        return Err(CodecError::BadMagic);
    }
    if input[4] != FORMAT_VERSION {
        return Err(CodecError::UnsupportedFormatVersion(input[4]));
    }

    let Some(kind) = BlobKind::from_u8(input[5]) else {
        return Err(CodecError::UnknownKind(input[5]));
    };
    if kind != expected_kind {
        return Err(CodecError::UnexpectedKind {
            expected: expected_kind,
            found: kind,
        });
    }

    let schema_version = u16::from_be_bytes([input[6], input[7]]);
    let config_version = u32::from_be_bytes([input[8], input[9], input[10], input[11]]);
    let payload_len = u32::from_be_bytes([input[12], input[13], input[14], input[15]]) as usize;
    let expected_crc = u32::from_be_bytes([input[16], input[17], input[18], input[19]]);
    let total_len = HEADER_LEN
        .checked_add(payload_len)
        .ok_or(CodecError::LengthMismatch)?;

    if input.len() != total_len {
        return Err(CodecError::LengthMismatch);
    }

    let payload = &input[HEADER_LEN..total_len];
    let actual_crc = crc32(payload);
    if actual_crc != expected_crc {
        return Err(CodecError::CrcMismatch {
            expected: expected_crc,
            actual: actual_crc,
        });
    }

    let value = postcard::from_bytes(payload).map_err(|_| CodecError::PostcardDeserialize)?;
    Ok(DecodedBlob {
        kind,
        schema_version,
        config_version,
        value,
    })
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in bytes.iter().copied() {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::{decode_enveloped, encode_enveloped, BlobKind, CodecError};
    use crate::ids::{BoardId, HouseId};
    use crate::schema::{
        BoardConfig, BoardRole, BrokerConfig, CanonicalConfig, NetworkConfig,
        SUPPORTED_SCHEMA_VERSION,
    };
    use heapless::{String, Vec};

    type TinyConfig = CanonicalConfig<1, 1, 1, 1, 1, 1, 1>;

    fn tiny_config() -> TinyConfig {
        let mut boards = Vec::new();
        boards
            .push(BoardConfig {
                board_id: BoardId(1),
                role: BoardRole::Coordinator,
            })
            .unwrap();

        CanonicalConfig {
            schema_version: SUPPORTED_SCHEMA_VERSION,
            config_version: 11,
            house_id: HouseId(7),
            boards,
            controllers: Vec::new(),
            devices: Vec::new(),
            dependencies: Vec::new(),
            rules: Vec::new(),
            broker: BrokerConfig {
                host_name: String::try_from("smartbox").unwrap(),
                tls_required: true,
            },
            network: NetworkConfig {
                dhcp: true,
                static_ipv4: None,
            },
        }
    }

    #[test]
    fn envelope_round_trip_preserves_config() {
        let config = tiny_config();
        let mut bytes = [0u8; 512];

        let encoded = encode_enveloped(
            BlobKind::CanonicalConfig,
            config.schema_version,
            config.config_version,
            &config,
            &mut bytes,
        )
        .unwrap();
        let decoded = decode_enveloped::<TinyConfig>(BlobKind::CanonicalConfig, encoded).unwrap();

        assert_eq!(decoded.schema_version, SUPPORTED_SCHEMA_VERSION);
        assert_eq!(decoded.config_version, 11);
        assert_eq!(decoded.value, config);
    }

    #[test]
    fn wrong_kind_is_rejected() {
        let config = tiny_config();
        let mut bytes = [0u8; 512];
        let encoded = encode_enveloped(
            BlobKind::CanonicalConfig,
            config.schema_version,
            config.config_version,
            &config,
            &mut bytes,
        )
        .unwrap();

        assert_eq!(
            decode_enveloped::<TinyConfig>(BlobKind::CompiledArtifacts, encoded).unwrap_err(),
            CodecError::UnexpectedKind {
                expected: BlobKind::CompiledArtifacts,
                found: BlobKind::CanonicalConfig,
            }
        );
    }

    #[test]
    fn corrupt_payload_is_rejected_by_crc() {
        let config = tiny_config();
        let mut bytes = [0u8; 512];
        let encoded = encode_enveloped(
            BlobKind::CanonicalConfig,
            config.schema_version,
            config.config_version,
            &config,
            &mut bytes,
        )
        .unwrap();

        let mut corrupted = [0u8; 512];
        corrupted[..encoded.len()].copy_from_slice(encoded);
        let last = encoded.len() - 1;
        corrupted[last] ^= 0x55;

        assert!(matches!(
            decode_enveloped::<TinyConfig>(BlobKind::CanonicalConfig, &corrupted[..encoded.len()]),
            Err(CodecError::CrcMismatch { .. })
        ));
    }
}
