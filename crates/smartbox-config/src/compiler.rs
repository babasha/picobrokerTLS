use crate::ids::{BoardId, DeviceId};
use crate::schema::{
    Action, BoardConfig, BoardRole, CanonicalConfig, ControllerConfig, DeviceConfig,
    DeviceProperty, RuleBody, RuleConfig, RuleScope, Stm32DependencyConfig, Trigger,
    SUPPORTED_SCHEMA_VERSION,
};
use heapless::Vec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompileError {
    UnsupportedSchemaVersion {
        found: u16,
        supported: u16,
    },
    EmptyHouseId,
    EmptyBoardId,
    EmptyDeviceId,
    EmptyRuleId,
    MissingBoard(BoardId),
    MissingTargetBoard(BoardId),
    MissingCoordinator,
    MultipleCoordinators,
    DuplicateBoardId(BoardId),
    DuplicateDeviceId(DeviceId),
    DuplicateRuleId(crate::ids::RuleId),
    ControllerBoardMissing(BoardId),
    DeviceBoardMissing(BoardId),
    DeviceControllerMissing,
    DependencyBoardMissing(BoardId),
    DependencyCrossesBoards,
    UnknownDevice(DeviceId),
    RuleScopeBoardMissing(BoardId),
    BoardLocalRuleTargetsRemoteDevice {
        rule_board_id: BoardId,
        device_id: DeviceId,
    },
    HouseLevelRuleOnZoneBoard,
    EmptyRuleBody,
    CapacityExceeded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalBoardInventory<const MAX_DEVICES: usize, const MAX_CONTROLLERS: usize> {
    pub board: BoardConfig,
    pub controllers: Vec<ControllerConfig, MAX_CONTROLLERS>,
    pub devices: Vec<DeviceConfig, MAX_DEVICES>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stm32DependencyLayout<const MAX_DEPENDENCIES: usize> {
    pub records: Vec<Stm32DependencyConfig, MAX_DEPENDENCIES>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledRule<const MAX_ACTIONS: usize, const MAX_STEPS: usize> {
    pub rule: RuleConfig<MAX_ACTIONS, MAX_STEPS>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyEntry {
    pub device_id: DeviceId,
    pub owner_board_id: BoardId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyCatalog<const MAX_DEVICES: usize> {
    pub entries: Vec<TopologyEntry, MAX_DEVICES>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompiledConfig<
    const MAX_DEVICES: usize,
    const MAX_CONTROLLERS: usize,
    const MAX_DEPENDENCIES: usize,
    const MAX_RULES: usize,
    const MAX_ACTIONS: usize,
    const MAX_STEPS: usize,
> {
    pub config_version: u32,
    pub local_inventory: LocalBoardInventory<MAX_DEVICES, MAX_CONTROLLERS>,
    pub stm32_dependencies: Stm32DependencyLayout<MAX_DEPENDENCIES>,
    pub rules: Vec<CompiledRule<MAX_ACTIONS, MAX_STEPS>, MAX_RULES>,
    pub topology: TopologyCatalog<MAX_DEVICES>,
}

pub fn validate_config<
    const MAX_BOARDS: usize,
    const MAX_CONTROLLERS: usize,
    const MAX_DEVICES: usize,
    const MAX_DEPENDENCIES: usize,
    const MAX_RULES: usize,
    const MAX_ACTIONS: usize,
    const MAX_STEPS: usize,
>(
    config: &CanonicalConfig<
        MAX_BOARDS,
        MAX_CONTROLLERS,
        MAX_DEVICES,
        MAX_DEPENDENCIES,
        MAX_RULES,
        MAX_ACTIONS,
        MAX_STEPS,
    >,
) -> Result<(), CompileError> {
    if config.schema_version != SUPPORTED_SCHEMA_VERSION {
        return Err(CompileError::UnsupportedSchemaVersion {
            found: config.schema_version,
            supported: SUPPORTED_SCHEMA_VERSION,
        });
    }
    if !config.house_id.is_valid() {
        return Err(CompileError::EmptyHouseId);
    }

    validate_boards(config)?;
    validate_controllers(config)?;
    validate_devices(config)?;
    validate_dependencies(config)?;
    validate_rules(config, None)?;
    Ok(())
}

pub fn compile_config<
    const MAX_BOARDS: usize,
    const MAX_CONTROLLERS: usize,
    const MAX_DEVICES: usize,
    const MAX_DEPENDENCIES: usize,
    const MAX_RULES: usize,
    const MAX_ACTIONS: usize,
    const MAX_STEPS: usize,
>(
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
) -> Result<
    CompiledConfig<
        MAX_DEVICES,
        MAX_CONTROLLERS,
        MAX_DEPENDENCIES,
        MAX_RULES,
        MAX_ACTIONS,
        MAX_STEPS,
    >,
    CompileError,
> {
    validate_config(config)?;

    let Some(board) = find_board(config, target_board_id).copied() else {
        return Err(CompileError::MissingTargetBoard(target_board_id));
    };

    let mut local_controllers = Vec::new();
    for controller in config
        .controllers
        .iter()
        .filter(|controller| controller.owner_board_id == target_board_id)
    {
        local_controllers
            .push(controller.clone())
            .map_err(|_| CompileError::CapacityExceeded)?;
    }

    let mut local_devices = Vec::new();
    for device in config
        .devices
        .iter()
        .filter(|device| device.owner_board_id == target_board_id)
    {
        local_devices
            .push(device.clone())
            .map_err(|_| CompileError::CapacityExceeded)?;
    }

    let mut stm32_dependencies = Vec::new();
    for dependency in config
        .dependencies
        .iter()
        .filter(|dependency| dependency.owner_board_id == target_board_id)
    {
        stm32_dependencies
            .push(*dependency)
            .map_err(|_| CompileError::CapacityExceeded)?;
    }

    let mut rules = Vec::new();
    for rule in config
        .rules
        .iter()
        .filter(|rule| rule_applies_to_board(rule, board))
    {
        rules
            .push(CompiledRule { rule: rule.clone() })
            .map_err(|_| CompileError::CapacityExceeded)?;
    }

    let mut topology_entries = Vec::new();
    for device in config.devices.iter() {
        topology_entries
            .push(TopologyEntry {
                device_id: device.device_id,
                owner_board_id: device.owner_board_id,
            })
            .map_err(|_| CompileError::CapacityExceeded)?;
    }

    Ok(CompiledConfig {
        config_version: config.config_version,
        local_inventory: LocalBoardInventory {
            board,
            controllers: local_controllers,
            devices: local_devices,
        },
        stm32_dependencies: Stm32DependencyLayout {
            records: stm32_dependencies,
        },
        rules,
        topology: TopologyCatalog {
            entries: topology_entries,
        },
    })
}

fn validate_boards<
    const MAX_BOARDS: usize,
    const MAX_CONTROLLERS: usize,
    const MAX_DEVICES: usize,
    const MAX_DEPENDENCIES: usize,
    const MAX_RULES: usize,
    const MAX_ACTIONS: usize,
    const MAX_STEPS: usize,
>(
    config: &CanonicalConfig<
        MAX_BOARDS,
        MAX_CONTROLLERS,
        MAX_DEVICES,
        MAX_DEPENDENCIES,
        MAX_RULES,
        MAX_ACTIONS,
        MAX_STEPS,
    >,
) -> Result<(), CompileError> {
    let mut coordinator_count = 0usize;

    for (index, board) in config.boards.iter().enumerate() {
        if !board.board_id.is_valid() {
            return Err(CompileError::EmptyBoardId);
        }
        if config
            .boards
            .iter()
            .skip(index + 1)
            .any(|other| other.board_id == board.board_id)
        {
            return Err(CompileError::DuplicateBoardId(board.board_id));
        }
        if board.role == BoardRole::Coordinator {
            coordinator_count += 1;
        }
    }

    match coordinator_count {
        0 => Err(CompileError::MissingCoordinator),
        1 => Ok(()),
        _ => Err(CompileError::MultipleCoordinators),
    }
}

fn validate_controllers<
    const MAX_BOARDS: usize,
    const MAX_CONTROLLERS: usize,
    const MAX_DEVICES: usize,
    const MAX_DEPENDENCIES: usize,
    const MAX_RULES: usize,
    const MAX_ACTIONS: usize,
    const MAX_STEPS: usize,
>(
    config: &CanonicalConfig<
        MAX_BOARDS,
        MAX_CONTROLLERS,
        MAX_DEVICES,
        MAX_DEPENDENCIES,
        MAX_RULES,
        MAX_ACTIONS,
        MAX_STEPS,
    >,
) -> Result<(), CompileError> {
    for controller in config.controllers.iter() {
        if find_board(config, controller.owner_board_id).is_none() {
            return Err(CompileError::ControllerBoardMissing(
                controller.owner_board_id,
            ));
        }
    }

    Ok(())
}

fn validate_devices<
    const MAX_BOARDS: usize,
    const MAX_CONTROLLERS: usize,
    const MAX_DEVICES: usize,
    const MAX_DEPENDENCIES: usize,
    const MAX_RULES: usize,
    const MAX_ACTIONS: usize,
    const MAX_STEPS: usize,
>(
    config: &CanonicalConfig<
        MAX_BOARDS,
        MAX_CONTROLLERS,
        MAX_DEVICES,
        MAX_DEPENDENCIES,
        MAX_RULES,
        MAX_ACTIONS,
        MAX_STEPS,
    >,
) -> Result<(), CompileError> {
    for (index, device) in config.devices.iter().enumerate() {
        if !device.device_id.is_valid() {
            return Err(CompileError::EmptyDeviceId);
        }
        if config
            .devices
            .iter()
            .skip(index + 1)
            .any(|other| other.device_id == device.device_id)
        {
            return Err(CompileError::DuplicateDeviceId(device.device_id));
        }
        if find_board(config, device.owner_board_id).is_none() {
            return Err(CompileError::DeviceBoardMissing(device.owner_board_id));
        }
        if let Some(port) = device.port {
            if config.controllers.iter().all(|controller| {
                controller.controller_id != port.controller_id
                    || controller.owner_board_id != device.owner_board_id
            }) {
                return Err(CompileError::DeviceControllerMissing);
            }
        }
    }

    Ok(())
}

fn validate_dependencies<
    const MAX_BOARDS: usize,
    const MAX_CONTROLLERS: usize,
    const MAX_DEVICES: usize,
    const MAX_DEPENDENCIES: usize,
    const MAX_RULES: usize,
    const MAX_ACTIONS: usize,
    const MAX_STEPS: usize,
>(
    config: &CanonicalConfig<
        MAX_BOARDS,
        MAX_CONTROLLERS,
        MAX_DEVICES,
        MAX_DEPENDENCIES,
        MAX_RULES,
        MAX_ACTIONS,
        MAX_STEPS,
    >,
) -> Result<(), CompileError> {
    for dependency in config.dependencies.iter() {
        if find_board(config, dependency.owner_board_id).is_none() {
            return Err(CompileError::DependencyBoardMissing(
                dependency.owner_board_id,
            ));
        }

        let source_owner = owner_of(config, dependency.source.device_id)?;
        let destination_owner = owner_of(config, dependency.destination.device_id)?;
        if source_owner != dependency.owner_board_id
            || destination_owner != dependency.owner_board_id
        {
            return Err(CompileError::DependencyCrossesBoards);
        }
    }

    Ok(())
}

fn validate_rules<
    const MAX_BOARDS: usize,
    const MAX_CONTROLLERS: usize,
    const MAX_DEVICES: usize,
    const MAX_DEPENDENCIES: usize,
    const MAX_RULES: usize,
    const MAX_ACTIONS: usize,
    const MAX_STEPS: usize,
>(
    config: &CanonicalConfig<
        MAX_BOARDS,
        MAX_CONTROLLERS,
        MAX_DEVICES,
        MAX_DEPENDENCIES,
        MAX_RULES,
        MAX_ACTIONS,
        MAX_STEPS,
    >,
    _target_board: Option<BoardConfig>,
) -> Result<(), CompileError> {
    for (index, rule) in config.rules.iter().enumerate() {
        if !rule.rule_id.is_valid() {
            return Err(CompileError::EmptyRuleId);
        }
        if config
            .rules
            .iter()
            .skip(index + 1)
            .any(|other| other.rule_id == rule.rule_id)
        {
            return Err(CompileError::DuplicateRuleId(rule.rule_id));
        }

        if let RuleScope::BoardLocal { board_id } = rule.scope {
            if find_board(config, board_id).is_none() {
                return Err(CompileError::RuleScopeBoardMissing(board_id));
            }
        }

        validate_trigger(config, rule.trigger)?;
        if let Some(condition) = rule.condition {
            validate_condition(config, condition)?;
        }
        validate_rule_body(config, rule)?;
    }

    Ok(())
}

fn validate_rule_body<
    const MAX_BOARDS: usize,
    const MAX_CONTROLLERS: usize,
    const MAX_DEVICES: usize,
    const MAX_DEPENDENCIES: usize,
    const MAX_RULES: usize,
    const MAX_ACTIONS: usize,
    const MAX_STEPS: usize,
>(
    config: &CanonicalConfig<
        MAX_BOARDS,
        MAX_CONTROLLERS,
        MAX_DEVICES,
        MAX_DEPENDENCIES,
        MAX_RULES,
        MAX_ACTIONS,
        MAX_STEPS,
    >,
    rule: &RuleConfig<MAX_ACTIONS, MAX_STEPS>,
) -> Result<(), CompileError> {
    match &rule.body {
        RuleBody::Act { actions } => {
            if actions.is_empty() {
                return Err(CompileError::EmptyRuleBody);
            }
            validate_actions(config, rule.scope, actions.iter().copied())
        }
        RuleBody::Steps { steps } => {
            if steps.is_empty() {
                return Err(CompileError::EmptyRuleBody);
            }
            for step in steps.iter() {
                if step.actions.is_empty() {
                    return Err(CompileError::EmptyRuleBody);
                }
                if let Some(precondition) = step.precondition {
                    validate_condition(config, precondition)?;
                }
                validate_actions(config, rule.scope, step.actions.iter().copied())?;
            }
            Ok(())
        }
    }
}

fn validate_actions<
    const MAX_BOARDS: usize,
    const MAX_CONTROLLERS: usize,
    const MAX_DEVICES: usize,
    const MAX_DEPENDENCIES: usize,
    const MAX_RULES: usize,
    const MAX_ACTIONS: usize,
    const MAX_STEPS: usize,
>(
    config: &CanonicalConfig<
        MAX_BOARDS,
        MAX_CONTROLLERS,
        MAX_DEVICES,
        MAX_DEPENDENCIES,
        MAX_RULES,
        MAX_ACTIONS,
        MAX_STEPS,
    >,
    scope: RuleScope,
    actions: impl Iterator<Item = Action>,
) -> Result<(), CompileError> {
    for action in actions {
        match action {
            Action::SetDevice { device_id, .. } => {
                let owner_board_id = owner_of(config, device_id)?;
                if let RuleScope::BoardLocal { board_id } = scope {
                    if owner_board_id != board_id {
                        return Err(CompileError::BoardLocalRuleTargetsRemoteDevice {
                            rule_board_id: board_id,
                            device_id,
                        });
                    }
                }
            }
            Action::SendBoardCommand { board_id, .. } => {
                if find_board(config, board_id).is_none() {
                    return Err(CompileError::MissingBoard(board_id));
                }
            }
            Action::PublishEvent { .. } => {}
        }
    }

    Ok(())
}

fn validate_trigger<
    const MAX_BOARDS: usize,
    const MAX_CONTROLLERS: usize,
    const MAX_DEVICES: usize,
    const MAX_DEPENDENCIES: usize,
    const MAX_RULES: usize,
    const MAX_ACTIONS: usize,
    const MAX_STEPS: usize,
>(
    config: &CanonicalConfig<
        MAX_BOARDS,
        MAX_CONTROLLERS,
        MAX_DEVICES,
        MAX_DEPENDENCIES,
        MAX_RULES,
        MAX_ACTIONS,
        MAX_STEPS,
    >,
    trigger: Option<Trigger>,
) -> Result<(), CompileError> {
    match trigger {
        Some(Trigger::DeviceChanged { property }) => validate_property(config, property),
        Some(Trigger::BoardEvent { board_id, .. }) => {
            if find_board(config, board_id).is_some() {
                Ok(())
            } else {
                Err(CompileError::MissingBoard(board_id))
            }
        }
        Some(Trigger::Schedule { .. }) | None => Ok(()),
    }
}

fn validate_condition<
    const MAX_BOARDS: usize,
    const MAX_CONTROLLERS: usize,
    const MAX_DEVICES: usize,
    const MAX_DEPENDENCIES: usize,
    const MAX_RULES: usize,
    const MAX_ACTIONS: usize,
    const MAX_STEPS: usize,
>(
    config: &CanonicalConfig<
        MAX_BOARDS,
        MAX_CONTROLLERS,
        MAX_DEVICES,
        MAX_DEPENDENCIES,
        MAX_RULES,
        MAX_ACTIONS,
        MAX_STEPS,
    >,
    condition: crate::schema::Condition,
) -> Result<(), CompileError> {
    match condition {
        crate::schema::Condition::DeviceProperty { property, .. } => {
            validate_property(config, property)
        }
    }
}

fn validate_property<
    const MAX_BOARDS: usize,
    const MAX_CONTROLLERS: usize,
    const MAX_DEVICES: usize,
    const MAX_DEPENDENCIES: usize,
    const MAX_RULES: usize,
    const MAX_ACTIONS: usize,
    const MAX_STEPS: usize,
>(
    config: &CanonicalConfig<
        MAX_BOARDS,
        MAX_CONTROLLERS,
        MAX_DEVICES,
        MAX_DEPENDENCIES,
        MAX_RULES,
        MAX_ACTIONS,
        MAX_STEPS,
    >,
    property: DeviceProperty,
) -> Result<(), CompileError> {
    owner_of(config, property.device_id).map(|_| ())
}

fn rule_applies_to_board<const MAX_ACTIONS: usize, const MAX_STEPS: usize>(
    rule: &&RuleConfig<MAX_ACTIONS, MAX_STEPS>,
    board: BoardConfig,
) -> bool {
    match rule.scope {
        RuleScope::BoardLocal { board_id } => board_id == board.board_id,
        RuleScope::HouseLevel => board.role == BoardRole::Coordinator,
    }
}

fn owner_of<
    const MAX_BOARDS: usize,
    const MAX_CONTROLLERS: usize,
    const MAX_DEVICES: usize,
    const MAX_DEPENDENCIES: usize,
    const MAX_RULES: usize,
    const MAX_ACTIONS: usize,
    const MAX_STEPS: usize,
>(
    config: &CanonicalConfig<
        MAX_BOARDS,
        MAX_CONTROLLERS,
        MAX_DEVICES,
        MAX_DEPENDENCIES,
        MAX_RULES,
        MAX_ACTIONS,
        MAX_STEPS,
    >,
    device_id: DeviceId,
) -> Result<BoardId, CompileError> {
    config
        .devices
        .iter()
        .find(|device| device.device_id == device_id)
        .map(|device| device.owner_board_id)
        .ok_or(CompileError::UnknownDevice(device_id))
}

fn find_board<
    const MAX_BOARDS: usize,
    const MAX_CONTROLLERS: usize,
    const MAX_DEVICES: usize,
    const MAX_DEPENDENCIES: usize,
    const MAX_RULES: usize,
    const MAX_ACTIONS: usize,
    const MAX_STEPS: usize,
>(
    config: &CanonicalConfig<
        MAX_BOARDS,
        MAX_CONTROLLERS,
        MAX_DEVICES,
        MAX_DEPENDENCIES,
        MAX_RULES,
        MAX_ACTIONS,
        MAX_STEPS,
    >,
    board_id: BoardId,
) -> Option<&BoardConfig> {
    config
        .boards
        .iter()
        .find(|board| board.board_id == board_id)
}

#[cfg(test)]
mod tests {
    use super::{compile_config, CompileError};
    use crate::ids::{BoardId, ControllerId, DeviceId, HouseId, RuleId};
    use crate::schema::{
        Action, BoardConfig, BoardRole, BrokerConfig, CanonicalConfig, ControllerConfig,
        ControllerFamily, DependencyDestinationCode, DependencySourceCode, DependencyTagType,
        DeviceConfig, DeviceKind, DeviceProperty, NetworkConfig, PortRef, PropertyKind, RuleBody,
        RuleConfig, RuleScope, Stm32DependencyConfig, Value, SUPPORTED_SCHEMA_VERSION,
    };
    use heapless::{String, Vec};

    type TestConfig = CanonicalConfig<4, 4, 8, 4, 4, 4, 4>;

    fn base_config() -> TestConfig {
        let mut boards = Vec::new();
        boards
            .push(BoardConfig {
                board_id: BoardId(1),
                role: BoardRole::Coordinator,
            })
            .unwrap();
        boards
            .push(BoardConfig {
                board_id: BoardId(2),
                role: BoardRole::Zone,
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
        controllers
            .push(ControllerConfig {
                controller_id: ControllerId(2),
                owner_board_id: BoardId(2),
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
        devices
            .push(DeviceConfig {
                device_id: DeviceId(20),
                owner_board_id: BoardId(2),
                kind: DeviceKind::Relay,
                port: Some(PortRef {
                    controller_id: ControllerId(2),
                    port: 1,
                }),
            })
            .unwrap();

        let mut dependencies = Vec::new();
        dependencies
            .push(Stm32DependencyConfig {
                owner_board_id: BoardId(1),
                source: DeviceProperty {
                    device_id: DeviceId(10),
                    property: PropertyKind::State,
                },
                source_code: DependencySourceCode::DeviceProperty,
                destination: DeviceProperty {
                    device_id: DeviceId(10),
                    property: PropertyKind::State,
                },
                destination_code: DependencyDestinationCode::SetDeviceProperty,
                tag: DependencyTagType::Generic,
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
            config_version: 42,
            house_id: HouseId(100),
            boards,
            controllers,
            devices,
            dependencies,
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
    fn compile_extracts_local_inventory_dependencies_rules_and_topology() {
        let config = base_config();

        let compiled = compile_config(&config, BoardId(1)).unwrap();

        assert_eq!(compiled.config_version, 42);
        assert_eq!(compiled.local_inventory.board.board_id, BoardId(1));
        assert_eq!(compiled.local_inventory.controllers.len(), 1);
        assert_eq!(compiled.local_inventory.devices.len(), 1);
        assert_eq!(compiled.stm32_dependencies.records.len(), 1);
        assert_eq!(compiled.rules.len(), 1);
        assert_eq!(compiled.topology.entries.len(), 2);
    }

    #[test]
    fn duplicate_device_id_is_rejected() {
        let mut config = base_config();
        let duplicate = config.devices[0].clone();
        config.devices.push(duplicate).unwrap();

        assert_eq!(
            compile_config(&config, BoardId(1)),
            Err(CompileError::DuplicateDeviceId(DeviceId(10)))
        );
    }

    #[test]
    fn board_local_rule_cannot_target_remote_device() {
        let mut config = base_config();
        let RuleBody::Act { actions } = &mut config.rules[0].body else {
            panic!("expected act body");
        };
        actions[0] = Action::SetDevice {
            device_id: DeviceId(20),
            property: PropertyKind::State,
            value: Value::Bool(true),
        };

        assert_eq!(
            compile_config(&config, BoardId(1)),
            Err(CompileError::BoardLocalRuleTargetsRemoteDevice {
                rule_board_id: BoardId(1),
                device_id: DeviceId(20),
            })
        );
    }

    #[test]
    fn dependency_cannot_cross_boards() {
        let mut config = base_config();
        config.dependencies[0].destination.device_id = DeviceId(20);

        assert_eq!(
            compile_config(&config, BoardId(1)),
            Err(CompileError::DependencyCrossesBoards)
        );
    }

    #[test]
    fn coordinator_receives_house_level_rule() {
        let mut config = base_config();
        config.rules[0].scope = RuleScope::HouseLevel;

        let compiled = compile_config(&config, BoardId(1)).unwrap();

        assert_eq!(compiled.rules.len(), 1);
    }

    #[test]
    fn zone_board_ignores_house_level_rule() {
        let mut config = base_config();
        config.rules[0].scope = RuleScope::HouseLevel;

        let compiled = compile_config(&config, BoardId(2)).unwrap();

        assert_eq!(compiled.rules.len(), 0);
        assert_eq!(compiled.local_inventory.devices.len(), 1);
    }

    #[test]
    fn device_port_must_reference_controller_on_same_board() {
        let mut config = base_config();
        config.devices[1].port = Some(PortRef {
            controller_id: ControllerId(1),
            port: 9,
        });

        assert_eq!(
            compile_config(&config, BoardId(2)),
            Err(CompileError::DeviceControllerMissing)
        );
    }
}
