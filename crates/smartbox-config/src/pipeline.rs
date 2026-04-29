use crate::active_slot::ConfigSlotId;
use crate::compiler::{compile_config, CompileError, CompiledConfig};
use crate::ids::BoardId;
use crate::schema::CanonicalConfig;
use crate::serialization::{BlobKind, CodecError};
use crate::storage::{load_active_blob, prepare_candidate_slot, ConfigStore, ConfigStoreError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparedCandidate {
    pub slot: ConfigSlotId,
    pub schema_version: u16,
    pub config_version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareCandidateError<E> {
    Compile(CompileError),
    Store(ConfigStoreError<E>),
    DiscardAfterStoreFailure {
        store: ConfigStoreError<E>,
        discard: E,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootConfigResult<T, E> {
    Ready {
        compiled: T,
        schema_version: u16,
        config_version: u32,
    },
    SafeBoot {
        reason: SafeBootReason<E>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafeBootReason<E> {
    Store(E),
    Codec(CodecError),
    SchemaVersionMismatch { expected: u16, found: u16 },
    ConfigVersionMismatch { expected: u32, found: u32 },
}

pub fn compile_and_prepare_candidate<
    S,
    const MAX_BOARDS: usize,
    const MAX_CONTROLLERS: usize,
    const MAX_DEVICES: usize,
    const MAX_DEPENDENCIES: usize,
    const MAX_RULES: usize,
    const MAX_ACTIONS: usize,
    const MAX_STEPS: usize,
>(
    store: &mut S,
    config: &CanonicalConfig<
        MAX_BOARDS,
        MAX_CONTROLLERS,
        MAX_DEVICES,
        MAX_DEPENDENCIES,
        MAX_RULES,
        MAX_ACTIONS,
        MAX_STEPS,
    >,
    target_board_id: BoardId,
    candidate_slot: ConfigSlotId,
    scratch: &mut [u8],
) -> Result<PreparedCandidate, PrepareCandidateError<S::Error>>
where
    S: ConfigStore,
{
    let compiled =
        compile_config(config, target_board_id).map_err(PrepareCandidateError::Compile)?;

    prepare_candidate_blob(
        store,
        candidate_slot,
        BlobKind::CanonicalConfig,
        config.schema_version,
        config.config_version,
        config,
        scratch,
    )?;

    prepare_candidate_blob(
        store,
        candidate_slot,
        BlobKind::CompiledArtifacts,
        config.schema_version,
        config.config_version,
        &compiled,
        scratch,
    )?;

    Ok(PreparedCandidate {
        slot: candidate_slot,
        schema_version: config.schema_version,
        config_version: config.config_version,
    })
}

pub fn load_active_compiled_config<
    'de,
    S,
    const MAX_DEVICES: usize,
    const MAX_CONTROLLERS: usize,
    const MAX_DEPENDENCIES: usize,
    const MAX_RULES: usize,
    const MAX_ACTIONS: usize,
    const MAX_STEPS: usize,
>(
    store: &S,
    expected_schema_version: u16,
    expected_config_version: u32,
    scratch: &'de mut [u8],
) -> BootConfigResult<
    CompiledConfig<
        MAX_DEVICES,
        MAX_CONTROLLERS,
        MAX_DEPENDENCIES,
        MAX_RULES,
        MAX_ACTIONS,
        MAX_STEPS,
    >,
    S::Error,
>
where
    S: ConfigStore,
{
    let decoded = match load_active_blob::<
        _,
        CompiledConfig<
            MAX_DEVICES,
            MAX_CONTROLLERS,
            MAX_DEPENDENCIES,
            MAX_RULES,
            MAX_ACTIONS,
            MAX_STEPS,
        >,
    >(store, scratch, BlobKind::CompiledArtifacts)
    {
        Ok(decoded) => decoded,
        Err(ConfigStoreError::Store(error)) => {
            return BootConfigResult::SafeBoot {
                reason: SafeBootReason::Store(error),
            }
        }
        Err(ConfigStoreError::Codec(error)) => {
            return BootConfigResult::SafeBoot {
                reason: SafeBootReason::Codec(error),
            }
        }
    };

    if decoded.schema_version != expected_schema_version {
        return BootConfigResult::SafeBoot {
            reason: SafeBootReason::SchemaVersionMismatch {
                expected: expected_schema_version,
                found: decoded.schema_version,
            },
        };
    }

    if decoded.config_version != expected_config_version {
        return BootConfigResult::SafeBoot {
            reason: SafeBootReason::ConfigVersionMismatch {
                expected: expected_config_version,
                found: decoded.config_version,
            },
        };
    }

    BootConfigResult::Ready {
        compiled: decoded.value,
        schema_version: decoded.schema_version,
        config_version: decoded.config_version,
    }
}

fn prepare_candidate_blob<S, T>(
    store: &mut S,
    slot: ConfigSlotId,
    kind: BlobKind,
    schema_version: u16,
    config_version: u32,
    value: &T,
    scratch: &mut [u8],
) -> Result<(), PrepareCandidateError<S::Error>>
where
    S: ConfigStore,
    T: serde::Serialize,
{
    match prepare_candidate_slot(
        store,
        slot,
        kind,
        schema_version,
        config_version,
        value,
        scratch,
    ) {
        Ok(()) => Ok(()),
        Err(error) => Err(discard_after_store_failure(store, slot, error)),
    }
}

fn discard_after_store_failure<S>(
    store: &mut S,
    slot: ConfigSlotId,
    error: ConfigStoreError<S::Error>,
) -> PrepareCandidateError<S::Error>
where
    S: ConfigStore,
{
    match store.discard_slot(slot) {
        Ok(()) => PrepareCandidateError::Store(error),
        Err(discard) => PrepareCandidateError::DiscardAfterStoreFailure {
            store: error,
            discard,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        compile_and_prepare_candidate, load_active_compiled_config, BootConfigResult,
        PrepareCandidateError, SafeBootReason,
    };
    use crate::active_slot::ConfigSlotId;
    use crate::compiler::CompiledConfig;
    use crate::ids::{BoardId, ControllerId, DeviceId, HouseId, RuleId};
    use crate::schema::{
        Action, BoardConfig, BoardRole, BrokerConfig, CanonicalConfig, ControllerConfig,
        ControllerFamily, DeviceConfig, DeviceKind, NetworkConfig, PortRef, PropertyKind, RuleBody,
        RuleConfig, RuleScope, Value, SUPPORTED_SCHEMA_VERSION,
    };
    use crate::serialization::BlobKind;
    use crate::storage::{
        commit_candidate_slot, load_active_blob, mock::MockStore, prepare_candidate_slot,
        ConfigStoreError,
    };
    use heapless::{String, Vec};

    type TestConfig = CanonicalConfig<4, 4, 8, 4, 4, 4, 4>;
    type TestCompiled = CompiledConfig<8, 4, 4, 4, 4, 4>;

    fn test_config() -> TestConfig {
        let mut boards = Vec::new();
        boards
            .push(BoardConfig {
                board_id: BoardId(1),
                role: BoardRole::Coordinator,
            })
            .unwrap();

        let mut controllers = Vec::new();
        controllers
            .push(ControllerConfig {
                controller_id: ControllerId(1),
                owner_board_id: BoardId(1),
                family: ControllerFamily::SmartBox,
            })
            .unwrap();

        let mut devices = Vec::new();
        devices
            .push(DeviceConfig {
                device_id: DeviceId(10),
                owner_board_id: BoardId(1),
                kind: DeviceKind::Relay,
                port: Some(PortRef {
                    controller_id: ControllerId(1),
                    port: 1,
                }),
            })
            .unwrap();

        let mut actions = Vec::new();
        actions
            .push(Action::SetDevice {
                device_id: DeviceId(10),
                property: PropertyKind::State,
                value: Value::Bool(true),
            })
            .unwrap();

        let mut rules = Vec::new();
        rules
            .push(RuleConfig {
                rule_id: RuleId(1),
                scope: RuleScope::BoardLocal {
                    board_id: BoardId(1),
                },
                enabled: true,
                trigger: None,
                condition: None,
                body: RuleBody::Act { actions },
                cooldown_ms: 0,
                version: 1,
            })
            .unwrap();

        CanonicalConfig {
            schema_version: SUPPORTED_SCHEMA_VERSION,
            config_version: 9,
            house_id: HouseId(100),
            boards,
            controllers,
            devices,
            dependencies: Vec::new(),
            rules,
            broker: BrokerConfig {
                host_name: String::try_from("smartbox-coordinator").unwrap(),
                tls_required: true,
            },
            network: NetworkConfig {
                dhcp: true,
                static_ipv4: None,
            },
        }
    }

    #[test]
    fn pipeline_writes_canonical_and_compiled_without_committing_active_slot() {
        let mut store = MockStore {
            active: 1,
            ..MockStore::default()
        };
        let config = test_config();
        let mut scratch = [0u8; 2048];

        let prepared = compile_and_prepare_candidate(
            &mut store,
            &config,
            BoardId(1),
            ConfigSlotId(2),
            &mut scratch,
        )
        .unwrap();

        assert_eq!(prepared.slot, ConfigSlotId(2));
        assert_eq!(prepared.config_version, 9);
        assert_eq!(store.active, 1);
        assert_eq!(store.active_write_count, 0);
        assert!(!store.slot_2_canonical.is_empty());
        assert!(!store.slot_2_compiled.is_empty());

        store.active = 2;
        let decoded_canonical =
            load_active_blob::<_, TestConfig>(&store, &mut scratch, BlobKind::CanonicalConfig)
                .unwrap();
        assert_eq!(decoded_canonical.value, config);

        let decoded_compiled =
            load_active_blob::<_, TestCompiled>(&store, &mut scratch, BlobKind::CompiledArtifacts)
                .unwrap();
        assert_eq!(decoded_compiled.value.local_inventory.devices.len(), 1);
        assert_eq!(decoded_compiled.value.rules.len(), 1);
    }

    #[test]
    fn compile_error_does_not_touch_candidate_or_active_slot() {
        let mut store = MockStore {
            active: 1,
            ..MockStore::default()
        };
        let mut config = test_config();
        config.house_id = HouseId(0);
        let mut scratch = [0u8; 2048];

        let err = compile_and_prepare_candidate(
            &mut store,
            &config,
            BoardId(1),
            ConfigSlotId(2),
            &mut scratch,
        )
        .unwrap_err();

        assert!(matches!(err, PrepareCandidateError::Compile(_)));
        assert_eq!(store.active, 1);
        assert_eq!(store.active_write_count, 0);
        assert!(store.slot_2_canonical.is_empty());
        assert!(store.slot_2_compiled.is_empty());
    }

    #[test]
    fn compiled_write_failure_discards_candidate_and_keeps_active_slot() {
        let mut store = MockStore {
            active: 1,
            fail_write_kind: Some(BlobKind::CompiledArtifacts),
            ..MockStore::default()
        };
        let config = test_config();
        let mut scratch = [0u8; 2048];

        let err = compile_and_prepare_candidate(
            &mut store,
            &config,
            BoardId(1),
            ConfigSlotId(2),
            &mut scratch,
        )
        .unwrap_err();

        assert_eq!(
            err,
            PrepareCandidateError::Store(ConfigStoreError::Store(()))
        );
        assert_eq!(store.active, 1);
        assert_eq!(store.active_write_count, 0);
        assert_eq!(store.discard_count, 1);
        assert!(store.slot_2_canonical.is_empty());
        assert!(store.slot_2_compiled.is_empty());
    }

    #[test]
    fn boot_load_ready_returns_compiled_artifacts() {
        let mut store = MockStore {
            active: 1,
            ..MockStore::default()
        };
        let config = test_config();
        let mut scratch = [0u8; 2048];
        compile_and_prepare_candidate(
            &mut store,
            &config,
            BoardId(1),
            ConfigSlotId(2),
            &mut scratch,
        )
        .unwrap();
        commit_candidate_slot(&mut store, ConfigSlotId(2)).unwrap();

        let boot = load_active_compiled_config::<_, 8, 4, 4, 4, 4, 4>(
            &store,
            SUPPORTED_SCHEMA_VERSION,
            config.config_version,
            &mut scratch,
        );

        match boot {
            BootConfigResult::Ready {
                compiled,
                schema_version,
                config_version,
            } => {
                assert_eq!(schema_version, SUPPORTED_SCHEMA_VERSION);
                assert_eq!(config_version, 9);
                assert_eq!(compiled.local_inventory.devices.len(), 1);
            }
            BootConfigResult::SafeBoot { reason } => panic!("unexpected safe boot: {:?}", reason),
        }
    }

    #[test]
    fn boot_load_schema_mismatch_returns_safe_boot() {
        let mut store = MockStore::default();
        let config = test_config();
        let compiled = crate::compiler::compile_config(&config, BoardId(1)).unwrap();
        let mut scratch = [0u8; 2048];
        prepare_candidate_slot(
            &mut store,
            ConfigSlotId(1),
            BlobKind::CompiledArtifacts,
            SUPPORTED_SCHEMA_VERSION + 1,
            config.config_version,
            &compiled,
            &mut scratch,
        )
        .unwrap();
        commit_candidate_slot(&mut store, ConfigSlotId(1)).unwrap();

        let boot = load_active_compiled_config::<_, 8, 4, 4, 4, 4, 4>(
            &store,
            SUPPORTED_SCHEMA_VERSION,
            config.config_version,
            &mut scratch,
        );

        assert_eq!(
            boot,
            BootConfigResult::SafeBoot {
                reason: SafeBootReason::SchemaVersionMismatch {
                    expected: SUPPORTED_SCHEMA_VERSION,
                    found: SUPPORTED_SCHEMA_VERSION + 1,
                }
            }
        );
    }

    #[test]
    fn boot_load_config_version_mismatch_returns_safe_boot() {
        let mut store = MockStore {
            active: 1,
            ..MockStore::default()
        };
        let config = test_config();
        let mut scratch = [0u8; 2048];
        compile_and_prepare_candidate(
            &mut store,
            &config,
            BoardId(1),
            ConfigSlotId(2),
            &mut scratch,
        )
        .unwrap();
        commit_candidate_slot(&mut store, ConfigSlotId(2)).unwrap();

        let boot = load_active_compiled_config::<_, 8, 4, 4, 4, 4, 4>(
            &store,
            SUPPORTED_SCHEMA_VERSION,
            config.config_version + 1,
            &mut scratch,
        );

        assert_eq!(
            boot,
            BootConfigResult::SafeBoot {
                reason: SafeBootReason::ConfigVersionMismatch {
                    expected: config.config_version + 1,
                    found: config.config_version,
                }
            }
        );
    }

    #[test]
    fn boot_load_corrupt_compiled_blob_returns_safe_boot() {
        let mut store = MockStore {
            active: 1,
            ..MockStore::default()
        };
        let config = test_config();
        let mut scratch = [0u8; 2048];
        compile_and_prepare_candidate(
            &mut store,
            &config,
            BoardId(1),
            ConfigSlotId(2),
            &mut scratch,
        )
        .unwrap();
        commit_candidate_slot(&mut store, ConfigSlotId(2)).unwrap();
        let last = store.slot_2_compiled.len() - 1;
        store.slot_2_compiled[last] ^= 0x33;

        let boot = load_active_compiled_config::<_, 8, 4, 4, 4, 4, 4>(
            &store,
            SUPPORTED_SCHEMA_VERSION,
            config.config_version,
            &mut scratch,
        );

        assert!(matches!(
            boot,
            BootConfigResult::SafeBoot {
                reason: SafeBootReason::Codec(_)
            }
        ));
    }
}
