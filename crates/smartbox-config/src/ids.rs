use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HouseId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BoardId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ControllerId(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RuleId(pub u32);

impl HouseId {
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }
}

impl BoardId {
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }
}

impl DeviceId {
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }
}

impl RuleId {
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }
}
