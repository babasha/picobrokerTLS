#![no_std]

#[cfg(test)]
extern crate std;

pub mod active_slot;
pub mod compiler;
pub mod ids;
pub mod manager;
pub mod pipeline;
pub mod schema;
pub mod serialization;
pub mod storage;

pub use active_slot::{ActiveSlotError, ActiveSlotState, ConfigSlotId};
pub use compiler::{
    compile_config, validate_config, CompileError, CompiledConfig, CompiledRule,
    LocalBoardInventory, Stm32DependencyLayout, TopologyCatalog, TopologyEntry,
};
pub use ids::{BoardId, ControllerId, DeviceId, HouseId, RuleId};
pub use manager::{ActivatePreparedError, ConfigManager};
pub use pipeline::{
    compile_and_prepare_candidate, load_active_compiled_config, BootConfigResult,
    PrepareCandidateError, PreparedCandidate, SafeBootReason,
};
pub use schema::*;
pub use serialization::{
    decode_enveloped, encode_enveloped, BlobKind, CodecError, DecodedBlob, FORMAT_VERSION,
};
pub use storage::{
    commit_candidate_slot, load_active_blob, prepare_candidate_slot, ConfigStore, ConfigStoreError,
};
