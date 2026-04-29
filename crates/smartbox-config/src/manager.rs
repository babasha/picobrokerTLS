use crate::active_slot::ConfigSlotId;
use crate::compiler::CompiledConfig;
use crate::ids::BoardId;
use crate::pipeline::{
    compile_and_prepare_candidate, load_active_compiled_config, BootConfigResult,
    PrepareCandidateError, PreparedCandidate,
};
use crate::schema::CanonicalConfig;
use crate::storage::{commit_candidate_slot, ConfigStore, ConfigStoreError};

pub struct ConfigManager<S> {
    store: S,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivatePreparedError<E> {
    Store(ConfigStoreError<E>),
    SlotMismatch {
        prepared: ConfigSlotId,
        requested: ConfigSlotId,
    },
}

impl<S> ConfigManager<S>
where
    S: ConfigStore,
{
    pub const fn new(store: S) -> Self {
        Self { store }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    pub fn into_store(self) -> S {
        self.store
    }

    pub fn prepare_update<
        const MAX_BOARDS: usize,
        const MAX_CONTROLLERS: usize,
        const MAX_DEVICES: usize,
        const MAX_DEPENDENCIES: usize,
        const MAX_RULES: usize,
        const MAX_ACTIONS: usize,
        const MAX_STEPS: usize,
    >(
        &mut self,
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
    ) -> Result<PreparedCandidate, PrepareCandidateError<S::Error>> {
        compile_and_prepare_candidate(
            &mut self.store,
            config,
            target_board_id,
            candidate_slot,
            scratch,
        )
    }

    pub fn activate_prepared(
        &mut self,
        prepared: PreparedCandidate,
        requested_slot: ConfigSlotId,
    ) -> Result<(), ActivatePreparedError<S::Error>> {
        if prepared.slot != requested_slot {
            return Err(ActivatePreparedError::SlotMismatch {
                prepared: prepared.slot,
                requested: requested_slot,
            });
        }

        commit_candidate_slot(&mut self.store, prepared.slot).map_err(ActivatePreparedError::Store)
    }

    pub fn boot_load<
        'de,
        const MAX_DEVICES: usize,
        const MAX_CONTROLLERS: usize,
        const MAX_DEPENDENCIES: usize,
        const MAX_RULES: usize,
        const MAX_ACTIONS: usize,
        const MAX_STEPS: usize,
    >(
        &self,
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
    > {
        load_active_compiled_config(
            &self.store,
            expected_schema_version,
            expected_config_version,
            scratch,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{ActivatePreparedError, ConfigManager};
    use crate::active_slot::ConfigSlotId;
    use crate::ids::{BoardId, ControllerId, DeviceId, HouseId, RuleId};
    use crate::pipeline::BootConfigResult;
    use crate::schema::{
        Action, BoardConfig, BoardRole, BrokerConfig, CanonicalConfig, ControllerConfig,
        ControllerFamily, DeviceConfig, DeviceKind, NetworkConfig, PortRef, PropertyKind, RuleBody,
        RuleConfig, RuleScope, Value, SUPPORTED_SCHEMA_VERSION,
    };
    use crate::storage::mock::MockStore;
    use heapless::{String, Vec};

    type TestConfig = CanonicalConfig<4, 4, 8, 4, 4, 4, 4>;

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
            config_version: 14,
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
    fn manager_prepares_activates_and_loads_config() {
        let store = MockStore {
            active: 1,
            ..MockStore::default()
        };
        let mut manager = ConfigManager::new(store);
        let config = test_config();
        let mut scratch = [0u8; 2048];

        let prepared = manager
            .prepare_update(&config, BoardId(1), ConfigSlotId(2), &mut scratch)
            .unwrap();
        assert_eq!(manager.store().active, 1);
        assert_eq!(manager.store().active_write_count, 0);

        manager
            .activate_prepared(prepared, ConfigSlotId(2))
            .unwrap();
        assert_eq!(manager.store().active, 2);
        assert_eq!(manager.store().active_write_count, 1);

        let boot = manager.boot_load::<8, 4, 4, 4, 4, 4>(
            SUPPORTED_SCHEMA_VERSION,
            config.config_version,
            &mut scratch,
        );

        match boot {
            BootConfigResult::Ready { compiled, .. } => {
                assert_eq!(compiled.local_inventory.devices.len(), 1);
                assert_eq!(compiled.rules.len(), 1);
            }
            BootConfigResult::SafeBoot { reason } => panic!("unexpected safe boot: {:?}", reason),
        }
    }

    #[test]
    fn manager_rejects_activation_of_unexpected_slot() {
        let store = MockStore::default();
        let mut manager = ConfigManager::new(store);
        let config = test_config();
        let mut scratch = [0u8; 2048];
        let prepared = manager
            .prepare_update(&config, BoardId(1), ConfigSlotId(2), &mut scratch)
            .unwrap();

        let err = manager
            .activate_prepared(prepared, ConfigSlotId(1))
            .unwrap_err();

        assert_eq!(
            err,
            ActivatePreparedError::SlotMismatch {
                prepared: ConfigSlotId(2),
                requested: ConfigSlotId(1),
            }
        );
        assert_eq!(manager.store().active_write_count, 0);
    }
}
