use crate::active_slot::ConfigSlotId;
use crate::serialization::{decode_enveloped, encode_enveloped, BlobKind, CodecError, DecodedBlob};
use serde::{Deserialize, Serialize};

pub trait ConfigStore {
    type Error;

    fn active_slot(&self) -> Result<ConfigSlotId, Self::Error>;
    fn write_active_slot(&mut self, slot: ConfigSlotId) -> Result<(), Self::Error>;
    fn read_blob<'a>(
        &self,
        slot: ConfigSlotId,
        kind: BlobKind,
        out: &'a mut [u8],
    ) -> Result<&'a [u8], Self::Error>;
    fn write_blob(
        &mut self,
        slot: ConfigSlotId,
        kind: BlobKind,
        bytes: &[u8],
    ) -> Result<(), Self::Error>;
    fn discard_slot(&mut self, slot: ConfigSlotId) -> Result<(), Self::Error>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigStoreError<E> {
    Store(E),
    Codec(CodecError),
}

impl<E> From<CodecError> for ConfigStoreError<E> {
    fn from(value: CodecError) -> Self {
        Self::Codec(value)
    }
}

pub fn load_active_blob<'de, S, T>(
    store: &S,
    scratch: &'de mut [u8],
    expected_kind: BlobKind,
) -> Result<DecodedBlob<T>, ConfigStoreError<S::Error>>
where
    S: ConfigStore,
    T: Deserialize<'de>,
{
    let slot = store.active_slot().map_err(ConfigStoreError::Store)?;
    let bytes = store
        .read_blob(slot, expected_kind, scratch)
        .map_err(ConfigStoreError::Store)?;
    decode_enveloped(expected_kind, bytes).map_err(ConfigStoreError::Codec)
}

pub fn prepare_candidate_slot<S, T>(
    store: &mut S,
    slot: ConfigSlotId,
    kind: BlobKind,
    schema_version: u16,
    config_version: u32,
    value: &T,
    scratch: &mut [u8],
) -> Result<(), ConfigStoreError<S::Error>>
where
    S: ConfigStore,
    T: Serialize,
{
    let encoded = encode_enveloped(kind, schema_version, config_version, value, scratch)
        .map_err(ConfigStoreError::Codec)?;
    store
        .write_blob(slot, kind, encoded)
        .map_err(ConfigStoreError::Store)
}

pub fn commit_candidate_slot<S: ConfigStore>(
    store: &mut S,
    slot: ConfigSlotId,
) -> Result<(), ConfigStoreError<S::Error>> {
    store
        .write_active_slot(slot)
        .map_err(ConfigStoreError::Store)
}

#[cfg(test)]
pub(crate) mod mock {
    use super::ConfigStore;
    use crate::active_slot::ConfigSlotId;
    use crate::serialization::BlobKind;
    use heapless::Vec;

    #[derive(Default)]
    pub(crate) struct MockStore {
        pub(crate) active: u32,
        pub(crate) slot_1_canonical: Vec<u8, 1024>,
        pub(crate) slot_1_compiled: Vec<u8, 1024>,
        pub(crate) slot_2_canonical: Vec<u8, 1024>,
        pub(crate) slot_2_compiled: Vec<u8, 1024>,
        pub(crate) fail_write_kind: Option<BlobKind>,
        pub(crate) discard_count: usize,
        pub(crate) active_write_count: usize,
    }

    impl MockStore {
        fn blob_mut(&mut self, slot: ConfigSlotId, kind: BlobKind) -> &mut Vec<u8, 1024> {
            match (slot.0, kind) {
                (1, BlobKind::CanonicalConfig) => &mut self.slot_1_canonical,
                (1, BlobKind::CompiledArtifacts) => &mut self.slot_1_compiled,
                (2, BlobKind::CanonicalConfig) => &mut self.slot_2_canonical,
                (2, BlobKind::CompiledArtifacts) => &mut self.slot_2_compiled,
                _ => panic!("invalid test slot"),
            }
        }

        fn blob(&self, slot: ConfigSlotId, kind: BlobKind) -> &Vec<u8, 1024> {
            match (slot.0, kind) {
                (1, BlobKind::CanonicalConfig) => &self.slot_1_canonical,
                (1, BlobKind::CompiledArtifacts) => &self.slot_1_compiled,
                (2, BlobKind::CanonicalConfig) => &self.slot_2_canonical,
                (2, BlobKind::CompiledArtifacts) => &self.slot_2_compiled,
                _ => panic!("invalid test slot"),
            }
        }
    }

    impl ConfigStore for MockStore {
        type Error = ();

        fn active_slot(&self) -> Result<ConfigSlotId, Self::Error> {
            Ok(ConfigSlotId(self.active))
        }

        fn write_active_slot(&mut self, slot: ConfigSlotId) -> Result<(), Self::Error> {
            self.active_write_count += 1;
            self.active = slot.0;
            Ok(())
        }

        fn read_blob<'a>(
            &self,
            slot: ConfigSlotId,
            kind: BlobKind,
            out: &'a mut [u8],
        ) -> Result<&'a [u8], Self::Error> {
            let stored = self.blob(slot, kind);
            out[..stored.len()].copy_from_slice(stored.as_slice());
            Ok(&out[..stored.len()])
        }

        fn write_blob(
            &mut self,
            slot: ConfigSlotId,
            kind: BlobKind,
            bytes: &[u8],
        ) -> Result<(), Self::Error> {
            if self.fail_write_kind == Some(kind) {
                return Err(());
            }

            let target = self.blob_mut(slot, kind);
            target.clear();
            target.extend_from_slice(bytes).map_err(|_| ())
        }

        fn discard_slot(&mut self, slot: ConfigSlotId) -> Result<(), Self::Error> {
            self.discard_count += 1;
            self.blob_mut(slot, BlobKind::CanonicalConfig).clear();
            self.blob_mut(slot, BlobKind::CompiledArtifacts).clear();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        commit_candidate_slot, load_active_blob, mock::MockStore, prepare_candidate_slot,
        ConfigStoreError,
    };
    use crate::active_slot::ConfigSlotId;
    use crate::ids::{BoardId, HouseId};
    use crate::schema::{
        BoardConfig, BoardRole, BrokerConfig, CanonicalConfig, NetworkConfig,
        SUPPORTED_SCHEMA_VERSION,
    };
    use crate::serialization::BlobKind;
    use heapless::{String, Vec};

    type TinyConfig = CanonicalConfig<1, 1, 1, 1, 1, 1, 1>;

    fn tiny_config(version: u32) -> TinyConfig {
        let mut boards = Vec::new();
        boards
            .push(BoardConfig {
                board_id: BoardId(1),
                role: BoardRole::Coordinator,
            })
            .unwrap();

        CanonicalConfig {
            schema_version: SUPPORTED_SCHEMA_VERSION,
            config_version: version,
            house_id: HouseId(9),
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
    fn prepare_then_commit_then_load_reads_candidate() {
        let mut store = MockStore::default();
        let config = tiny_config(3);
        let mut scratch = [0u8; 512];

        prepare_candidate_slot(
            &mut store,
            ConfigSlotId(2),
            BlobKind::CanonicalConfig,
            config.schema_version,
            config.config_version,
            &config,
            &mut scratch,
        )
        .unwrap();
        commit_candidate_slot(&mut store, ConfigSlotId(2)).unwrap();

        let decoded =
            load_active_blob::<_, TinyConfig>(&store, &mut scratch, BlobKind::CanonicalConfig)
                .unwrap();

        assert_eq!(decoded.value, config);
    }

    #[test]
    fn loading_wrong_kind_returns_codec_error() {
        let mut store = MockStore::default();
        let config = tiny_config(3);
        let mut scratch = [0u8; 512];

        prepare_candidate_slot(
            &mut store,
            ConfigSlotId(1),
            BlobKind::CanonicalConfig,
            config.schema_version,
            config.config_version,
            &config,
            &mut scratch,
        )
        .unwrap();
        commit_candidate_slot(&mut store, ConfigSlotId(1)).unwrap();

        assert!(matches!(
            load_active_blob::<_, TinyConfig>(&store, &mut scratch, BlobKind::CompiledArtifacts),
            Err(ConfigStoreError::Codec(_))
        ));
    }
}
