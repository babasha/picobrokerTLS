use crate::ids::{BoardId, ControllerId, DeviceId, HouseId, RuleId};
use heapless::{String, Vec};
use serde::{Deserialize, Serialize};

pub const SUPPORTED_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoardRole {
    Coordinator,
    Standby,
    Zone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControllerFamily {
    XController,
    IRGate,
    Dimmer,
    PWM,
    SmartBox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceKind {
    Relay,
    DiscreteInput,
    Adc,
    Dht,
    Ds18B20,
    DaliLight,
    DaliGroup,
    Pwm,
    TriacDimmer,
    IButton,
    IrGate,
    TuyaRelay,
    TasmotaIrGate,
    AquaProtect,
    ClimateControl,
    Curtains,
    Fan,
    GateValve,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PropertyKind {
    State,
    Brightness,
    Value,
    Temperature,
    Humidity,
    Counter,
    LastKey,
    Position,
    Mode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Value {
    Bool(bool),
    U8(u8),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceProperty {
    pub device_id: DeviceId,
    pub property: PropertyKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortRef {
    pub controller_id: ControllerId,
    pub port: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardConfig {
    pub board_id: BoardId,
    pub role: BoardRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerConfig {
    pub controller_id: ControllerId,
    pub owner_board_id: BoardId,
    pub family: ControllerFamily,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub device_id: DeviceId,
    pub owner_board_id: BoardId,
    pub kind: DeviceKind,
    pub port: Option<PortRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleScope {
    BoardLocal { board_id: BoardId },
    HouseLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trigger {
    DeviceChanged { property: DeviceProperty },
    Schedule { schedule_id: u32 },
    BoardEvent { board_id: BoardId, event_code: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompareOp {
    Eq,
    NotEq,
    Greater,
    GreaterOrEq,
    Less,
    LessOrEq,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Condition {
    DeviceProperty {
        property: DeviceProperty,
        op: CompareOp,
        value: Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    SetDevice {
        device_id: DeviceId,
        property: PropertyKind,
        value: Value,
    },
    SendBoardCommand {
        board_id: BoardId,
        action_code: u16,
    },
    PublishEvent {
        event_code: u16,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step<const MAX_ACTIONS: usize> {
    pub delay_ms: u32,
    pub actions: Vec<Action, MAX_ACTIONS>,
    pub precondition: Option<Condition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuleBody<const MAX_ACTIONS: usize, const MAX_STEPS: usize> {
    Act {
        actions: Vec<Action, MAX_ACTIONS>,
    },
    Steps {
        steps: Vec<Step<MAX_ACTIONS>, MAX_STEPS>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleConfig<const MAX_ACTIONS: usize, const MAX_STEPS: usize> {
    pub rule_id: RuleId,
    pub scope: RuleScope,
    pub enabled: bool,
    pub trigger: Option<Trigger>,
    pub condition: Option<Condition>,
    pub body: RuleBody<MAX_ACTIONS, MAX_STEPS>,
    pub cooldown_ms: u32,
    pub version: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DependencySourceCode {
    DeviceProperty,
    ControllerEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DependencyDestinationCode {
    SetDeviceProperty,
    DisableDevice,
    EnableDevice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DependencyTagType {
    Generic,
    AquaProtect,
    ClimateControl,
    Curtains,
    Fan,
    GateValve,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stm32DependencyConfig {
    pub owner_board_id: BoardId,
    pub source: DeviceProperty,
    pub source_code: DependencySourceCode,
    pub destination: DeviceProperty,
    pub destination_code: DependencyDestinationCode,
    pub tag: DependencyTagType,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerConfig {
    pub host_name: String<32>,
    pub tls_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkConfig {
    pub dhcp: bool,
    pub static_ipv4: Option<[u8; 4]>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalConfig<
    const MAX_BOARDS: usize,
    const MAX_CONTROLLERS: usize,
    const MAX_DEVICES: usize,
    const MAX_DEPENDENCIES: usize,
    const MAX_RULES: usize,
    const MAX_ACTIONS: usize,
    const MAX_STEPS: usize,
> {
    pub schema_version: u16,
    pub config_version: u32,
    pub house_id: HouseId,
    pub boards: Vec<BoardConfig, MAX_BOARDS>,
    pub controllers: Vec<ControllerConfig, MAX_CONTROLLERS>,
    pub devices: Vec<DeviceConfig, MAX_DEVICES>,
    pub dependencies: Vec<Stm32DependencyConfig, MAX_DEPENDENCIES>,
    pub rules: Vec<RuleConfig<MAX_ACTIONS, MAX_STEPS>, MAX_RULES>,
    pub broker: BrokerConfig,
    pub network: NetworkConfig,
}
