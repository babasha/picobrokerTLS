# smartbox-config

`smartbox-config` is the firmware-side canonical configuration boundary for SmartBox.

The crate is intentionally `no_std` and keeps dependencies limited to `heapless`, `serde` and `postcard`. It contains:

- versioned canonical config structs;
- validation for IDs, ownership, references and rule scopes;
- a compiler from canonical config into runtime-ready artifacts;
- `postcard` serialization in a small SmartBox envelope with version, kind, length and CRC32;
- a storage trait boundary for loading/preparing/committing config slots;
- a high-level candidate preparation pipeline: `validate -> compile -> write canonical blob -> write compiled blob`;
- a boot/load pipeline that returns `Ready` or a controlled `SafeBoot` reason instead of panicking;
- `ConfigManager`, a firmware-facing wrapper around prepare/activate/boot-load operations;
- an active-slot state machine for `validate -> persist candidate -> commit pointer`.

It does not parse legacy JSON and does not implement a concrete flash backend. Those are separate layers:

- legacy import tool: reads old MQTT/config payloads and produces `CanonicalConfig`;
- persistence adapter: implements `ConfigStore` over the data partition / NVS pointer;
- boot loader/runtime: reads the active slot pointer and loads compiled artifacts.

The first compiler output is deliberately semantic, not byte-final:

- local board inventory;
- STM32 dependency layout;
- compiled rules;
- house topology catalog.

The actual SQI byte encoding for dependency apply, MQTT retained snapshot formatting and storage serialization can be added behind this boundary without changing who owns config lifecycle.

`compile_and_prepare_candidate()` never advances the active slot pointer. It only prepares both candidate blobs. Activation remains an explicit `commit_candidate_slot()` step after the caller decides the candidate is safe to activate.

`load_active_compiled_config()` reads only compiled artifacts from the active slot. If the blob is missing, corrupt or version-incompatible, it returns `BootConfigResult::SafeBoot`, so the runtime can start a reduced control mode without rules/automation.

`ConfigManager` is the API expected by `smartbox-p4`: `prepare_update()`, `activate_prepared()` and `boot_load()`. It owns the storage backend while preserving the explicit separation between candidate preparation and activation.
