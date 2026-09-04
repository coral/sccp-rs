use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::provider::{
    ConfigurationOrigin, ConfigurationProvider, ConfigurationProviderError,
    FileConfigurationProvider, StaticConfigurationSource,
};
use super::{
    ConfigOverlayKind, ConfigOverlaySection, ConfigOverlayValue, ConfigurationSource, ModuleConfig,
    normalize_name,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SorceryField {
    pub name: String,
    pub value: String,
}

impl SorceryField {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }

    pub fn indexed(name: &str, position: u32, value: impl Into<String>) -> Self {
        Self::new(format!("{name}.{position:04}"), value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OrderedSorceryObject {
    pub id: String,
    pub fields: Vec<SorceryField>,
}

impl OrderedSorceryObject {
    pub fn new(id: impl Into<String>, fields: Vec<SorceryField>) -> Self {
        Self {
            id: id.into(),
            fields,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SorceryInventory {
    pub devices: Vec<OrderedSorceryObject>,
    pub lines: Vec<OrderedSorceryObject>,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("{0}")]
pub struct SorcerySourceError(String);

impl SorcerySourceError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

pub trait SorceryObjectSource: Send + Sync {
    fn load_desired(&self) -> Result<SorceryInventory, SorcerySourceError>;

    fn load_last_known_good(&self) -> Result<Option<SorceryInventory>, SorcerySourceError>;

    fn store_last_known_good(&self, inventory: &SorceryInventory)
    -> Result<(), SorcerySourceError>;
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum SorceryObjectError {
    #[error("object ID is empty")]
    EmptyId,
    #[error("{kind} object {id:?} occurs more than once")]
    DuplicateObject { kind: &'static str, id: String },
    #[error("field name is empty")]
    EmptyFieldName,
    #[error("field {0:?} occurs more than once")]
    DuplicateField(String),
    #[error("field {field:?} has an invalid numeric position {position:?}")]
    InvalidPosition { field: String, position: String },
    #[error("field {field:?} repeats position {position} for {base}")]
    DuplicatePosition {
        field: String,
        base: String,
        position: u32,
    },
    #[error("repeatable field {0:?} requires a numeric suffix such as .0001")]
    MissingPosition(String),
    #[error("field {0:?} is not repeatable and cannot have a numeric suffix")]
    IndexedScalar(String),
    #[error("indexed field {0:?} must not be empty")]
    EmptyIndexedValue(String),
    #[error("field type is implicit in the Sorcery object path")]
    ExplicitType,
}

struct PendingCandidate {
    config: ModuleConfig,
    inventory: Option<SorceryInventory>,
}

#[derive(Default)]
struct ProviderState {
    pending: Option<PendingCandidate>,
    rejected_desired: Option<String>,
}

pub struct SorceryConfigurationProvider<F = FileConfigurationProvider> {
    base: F,
    source: Arc<dyn SorceryObjectSource>,
    state: Mutex<ProviderState>,
}

impl<F> SorceryConfigurationProvider<F>
where
    F: StaticConfigurationSource,
{
    pub fn new(base: F, source: Arc<dyn SorceryObjectSource>) -> Self {
        Self {
            base,
            source,
            state: Mutex::new(ProviderState::default()),
        }
    }

    pub fn last_rejected_desired(&self) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.rejected_desired.clone())
    }

    fn compile(
        &self,
        contents: &str,
        inventory: &SorceryInventory,
        origin: &str,
    ) -> Result<ModuleConfig, ConfigurationProviderError> {
        let overlays = inventory_overlays(inventory)
            .map_err(|error| ConfigurationProviderError::unavailable(origin, error.to_string()))?;
        ModuleConfig::parse_with_sorcery_overlays(contents, &overlays).map_err(|source| {
            ConfigurationProviderError::invalid(ConfigurationOrigin::Named(origin.into()), source)
        })
    }

    fn remember(
        &self,
        config: ModuleConfig,
        inventory: Option<SorceryInventory>,
        rejected_desired: Option<String>,
    ) -> Result<ModuleConfig, ConfigurationProviderError> {
        let mut state = self.state.lock().map_err(|_| {
            ConfigurationProviderError::unavailable("sorcery", "provider state is poisoned")
        })?;
        state.pending = Some(PendingCandidate {
            config: config.clone(),
            inventory,
        });
        state.rejected_desired = rejected_desired;
        Ok(config)
    }

    fn fallback(
        &self,
        contents: &str,
        desired_error: ConfigurationProviderError,
    ) -> Result<ModuleConfig, ConfigurationProviderError> {
        let desired_text = desired_error.to_string();
        let inventory = self
            .source
            .load_last_known_good()
            .map_err(|error| {
                ConfigurationProviderError::unavailable(
                    "sorcery fallback",
                    format!("desired rejected: {desired_text}; LKG load failed: {error}"),
                )
            })?
            .ok_or(desired_error)?;
        let config = self
            .compile(contents, &inventory, "sorcery last-known-good")
            .map_err(|error| {
                ConfigurationProviderError::unavailable(
                    "sorcery fallback",
                    format!("desired rejected: {desired_text}; LKG rejected: {error}"),
                )
            })?;
        self.remember(config, None, Some(desired_text))
    }

    fn reject(
        &self,
        error: ConfigurationProviderError,
    ) -> Result<ModuleConfig, ConfigurationProviderError> {
        if let Ok(mut state) = self.state.lock() {
            state.rejected_desired = Some(error.to_string());
        }
        Err(error)
    }

    fn load_candidate(
        &self,
        allow_fallback: bool,
    ) -> Result<ModuleConfig, ConfigurationProviderError> {
        let contents = self.base.read_source()?;
        let selected = ModuleConfig::configuration_source_from_source(&contents)
            .map_err(|error| ConfigurationProviderError::invalid(self.base.origin(), error))?;
        if selected != ConfigurationSource::Sorcery {
            return Err(ConfigurationProviderError::unavailable(
                "sorcery",
                "configuration_source must be sorcery",
            ));
        }

        let desired = match self.source.load_desired() {
            Ok(inventory) => inventory,
            Err(error) => {
                let error = ConfigurationProviderError::unavailable(
                    "sorcery desired inventory",
                    error.to_string(),
                );
                return if allow_fallback {
                    self.fallback(&contents, error)
                } else {
                    self.reject(error)
                };
            }
        };
        match self.compile(&contents, &desired, "sorcery desired inventory") {
            Ok(config) => self.remember(config, Some(desired), None),
            Err(error) if allow_fallback => self.fallback(&contents, error),
            Err(error) => self.reject(error),
        }
    }
}

impl<F> ConfigurationProvider for SorceryConfigurationProvider<F>
where
    F: StaticConfigurationSource,
{
    fn load(&self) -> Result<ModuleConfig, ConfigurationProviderError> {
        self.load_candidate(true)
    }

    fn refresh(&self) -> Result<ModuleConfig, ConfigurationProviderError> {
        self.load_candidate(false)
    }

    fn activated(&self, configuration: &ModuleConfig) -> Result<(), ConfigurationProviderError> {
        let mut state = self.state.lock().map_err(|_| {
            ConfigurationProviderError::unavailable("sorcery", "provider state is poisoned")
        })?;
        let pending = state.pending.as_ref().ok_or_else(|| {
            ConfigurationProviderError::unavailable(
                "sorcery activation",
                "configuration was not returned by the most recent load",
            )
        })?;
        if &pending.config != configuration {
            return Err(ConfigurationProviderError::unavailable(
                "sorcery activation",
                "configuration does not match the most recent candidate",
            ));
        }
        if let Some(inventory) = pending.inventory.as_ref() {
            self.source
                .store_last_known_good(inventory)
                .map_err(|error| {
                    ConfigurationProviderError::unavailable(
                        "sorcery last-known-good store",
                        error.to_string(),
                    )
                })?;
        }
        state.pending = None;
        Ok(())
    }
}

fn inventory_overlays(
    inventory: &SorceryInventory,
) -> Result<Vec<ConfigOverlaySection>, SorceryObjectError> {
    let mut overlays = Vec::with_capacity(inventory.devices.len() + inventory.lines.len());
    let mut object_ids = HashSet::new();
    for (kind, objects) in [
        (ConfigOverlayKind::Device, &inventory.devices),
        (ConfigOverlayKind::Line, &inventory.lines),
    ] {
        let mut objects = objects.iter().collect::<Vec<_>>();
        objects.sort_by_key(|object| object.id.to_ascii_lowercase());
        for object in objects {
            let identity = object.id.trim().to_ascii_lowercase();
            if !object_ids.insert(identity) {
                return Err(SorceryObjectError::DuplicateObject {
                    kind: kind_name(kind),
                    id: object.id.clone(),
                });
            }
            overlays.push(object_overlay(kind, object)?);
        }
    }
    Ok(overlays)
}

fn object_overlay(
    kind: ConfigOverlayKind,
    object: &OrderedSorceryObject,
) -> Result<ConfigOverlaySection, SorceryObjectError> {
    let id = object.id.trim();
    if id.is_empty() {
        return Err(SorceryObjectError::EmptyId);
    }
    let mut names = HashSet::new();
    let mut positions = HashSet::new();
    let mut scalars = Vec::new();
    let mut indexed = Vec::new();
    for (ordinal, field) in object.fields.iter().enumerate() {
        let name = field.name.trim();
        if name.is_empty() {
            return Err(SorceryObjectError::EmptyFieldName);
        }
        if !names.insert(name.to_ascii_lowercase()) {
            return Err(SorceryObjectError::DuplicateField(name.into()));
        }
        let (base, position) = split_position(name)?;
        let normalized = normalize_name(base);
        if normalized == "type" {
            return Err(SorceryObjectError::ExplicitType);
        }
        let repeatable = is_repeatable(kind, &normalized) || normalized == "template";
        match position {
            Some(position) => {
                if !repeatable {
                    return Err(SorceryObjectError::IndexedScalar(name.into()));
                }
                if field.value.is_empty() {
                    return Err(SorceryObjectError::EmptyIndexedValue(name.into()));
                }
                if !positions.insert((normalized.clone(), position)) {
                    return Err(SorceryObjectError::DuplicatePosition {
                        field: name.into(),
                        base: base.into(),
                        position,
                    });
                }
                indexed.push((
                    position,
                    ordinal,
                    base.to_ascii_lowercase(),
                    field.value.clone(),
                ));
            }
            None if repeatable => {
                return Err(SorceryObjectError::MissingPosition(name.into()));
            }
            None => scalars.push((base.to_ascii_lowercase(), field.value.clone())),
        }
    }
    indexed.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    scalars.extend(indexed.into_iter().map(|(_, _, name, value)| (name, value)));

    let mut parents = Vec::new();
    let mut values = Vec::new();
    for (name, value) in scalars {
        if normalize_name(&name) == "template" {
            parents.push(value);
        } else {
            values.push(ConfigOverlayValue {
                key: name,
                value: Some(value),
            });
        }
    }
    Ok(ConfigOverlaySection {
        name: id.into(),
        source: format!("sorcery {} {id}", kind_name(kind)),
        line: 1,
        kind: Some(kind),
        parents,
        delete: false,
        values,
    })
}

fn split_position(name: &str) -> Result<(&str, Option<u32>), SorceryObjectError> {
    let Some((base, suffix)) = name.rsplit_once('.') else {
        return Ok((name, None));
    };
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok((name, None));
    }
    if base.is_empty() {
        return Err(SorceryObjectError::EmptyFieldName);
    }
    let position = suffix
        .parse()
        .map_err(|_| SorceryObjectError::InvalidPosition {
            field: name.into(),
            position: suffix.into(),
        })?;
    Ok((base, Some(position)))
}

fn is_repeatable(kind: ConfigOverlayKind, normalized: &str) -> bool {
    matches!(normalized, "setvar" | "allow" | "disallow")
        || kind == ConfigOverlayKind::Device
            && matches!(
                normalized,
                "deny"
                    | "permit"
                    | "permithost"
                    | "featuredefault"
                    | "dndschedule"
                    | "line"
                    | "button"
            )
}

fn kind_name(kind: ConfigOverlayKind) -> &'static str {
    match kind {
        ConfigOverlayKind::Device => "device",
        ConfigOverlayKind::Line => "line",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use sccp_protocol::ButtonDefinition;

    use super::*;

    const BASE: &str = r#"
        [general]
        configuration_source = sorcery
        advertised_address = 192.0.2.10

        [desk](!)
        type = device
        description = inherited desk

        [office-line](!)
        type = line
        context = from-template

        [SEPAAAAAAAAAAAA]
        type = device
        line = static

        [static]
        type = line
    "#;

    #[derive(Clone)]
    struct MemoryStatic(&'static str);

    impl StaticConfigurationSource for MemoryStatic {
        fn origin(&self) -> ConfigurationOrigin {
            ConfigurationOrigin::Named("memory base".into())
        }

        fn read_source(&self) -> Result<String, ConfigurationProviderError> {
            Ok(self.0.into())
        }
    }

    struct MemorySorcery {
        desired: Mutex<Result<SorceryInventory, SorcerySourceError>>,
        lkg: Mutex<Option<SorceryInventory>>,
        stores: Mutex<Vec<SorceryInventory>>,
    }

    impl Default for MemorySorcery {
        fn default() -> Self {
            Self {
                desired: Mutex::new(Ok(SorceryInventory::default())),
                lkg: Mutex::new(None),
                stores: Mutex::new(Vec::new()),
            }
        }
    }

    impl MemorySorcery {
        fn with_desired(desired: SorceryInventory) -> Self {
            Self {
                desired: Mutex::new(Ok(desired)),
                ..Self::default()
            }
        }

        fn replace_desired(&self, desired: SorceryInventory) {
            *self.desired.lock().unwrap() = Ok(desired);
        }
    }

    impl SorceryObjectSource for MemorySorcery {
        fn load_desired(&self) -> Result<SorceryInventory, SorcerySourceError> {
            self.desired.lock().unwrap().clone()
        }

        fn load_last_known_good(&self) -> Result<Option<SorceryInventory>, SorcerySourceError> {
            Ok(self.lkg.lock().unwrap().clone())
        }

        fn store_last_known_good(
            &self,
            inventory: &SorceryInventory,
        ) -> Result<(), SorcerySourceError> {
            *self.lkg.lock().unwrap() = Some(inventory.clone());
            self.stores.lock().unwrap().push(inventory.clone());
            Ok(())
        }
    }

    fn field(name: &str, value: &str) -> SorceryField {
        SorceryField::new(name, value)
    }

    fn inventory(line_label: &str) -> SorceryInventory {
        SorceryInventory {
            devices: vec![OrderedSorceryObject::new(
                "SEP001122334455",
                vec![
                    field("template.0000", "desk"),
                    field("button.0010", "speed_dial, Last, 2010"),
                    field("line.0002", "1001"),
                    field("button.0007", "speed_dial, Middle, 2007"),
                ],
            )],
            lines: vec![OrderedSorceryObject::new(
                "1001",
                vec![
                    field("template.0000", "office-line"),
                    field("label", line_label),
                ],
            )],
        }
    }

    #[test]
    fn source_selection_defaults_to_file_and_accepts_sorcery() {
        assert_eq!(
            ModuleConfig::configuration_source_from_source("[general]\n").unwrap(),
            ConfigurationSource::File
        );
        assert_eq!(
            ModuleConfig::configuration_source_from_source(
                "[general]\nconfiguration_source = SoRcErY\n"
            )
            .unwrap(),
            ConfigurationSource::Sorcery
        );
        assert!(
            ModuleConfig::configuration_source_from_source(
                "[general]\nconfiguration_source = realtime\n"
            )
            .is_err()
        );
        assert!(
            ModuleConfig::configuration_source_from_source(
                "[general]\nconfiguration_source=sorcery\ndevicetable=devices\nlinetable=lines\n"
            )
            .is_err()
        );
    }

    #[test]
    fn provider_removes_static_objects_preserves_templates_and_orders_positions() {
        let source = Arc::new(MemorySorcery::with_desired(inventory("Sorcery line")));
        let provider = SorceryConfigurationProvider::new(
            MemoryStatic(BASE),
            Arc::<MemorySorcery>::clone(&source),
        );

        let config = provider.load().unwrap();
        let device = config.devices.values().next().unwrap();

        assert_eq!(config.devices.len(), 1);
        assert_eq!(config.lines.len(), 1);
        assert_eq!(device.description, "inherited desk");
        assert_eq!(config.lines["1001"].context, "from-template");
        assert_eq!(config.lines["1001"].label, "Sorcery line");
        assert!(
            matches!(&device.buttons[0], ButtonDefinition::Line(line) if line.number == "1001")
        );
        assert!(
            matches!(&device.buttons[1], ButtonDefinition::SpeedDial(speed) if speed.display_name == "Middle")
        );
        assert!(
            matches!(&device.buttons[2], ButtonDefinition::SpeedDial(speed) if speed.display_name == "Last")
        );
    }

    #[test]
    fn sorcery_allows_empty_inventory_and_line_first_provisioning() {
        let empty = Arc::new(MemorySorcery::with_desired(SorceryInventory::default()));
        let provider = SorceryConfigurationProvider::new(
            MemoryStatic(BASE),
            Arc::<MemorySorcery>::clone(&empty),
        );
        let config = provider.load().unwrap();
        assert!(config.devices.is_empty());
        assert!(config.lines.is_empty());

        empty.replace_desired(SorceryInventory {
            devices: Vec::new(),
            lines: vec![OrderedSorceryObject::new(
                "1001",
                vec![field("label", "Unassigned")],
            )],
        });
        let config = provider.refresh().unwrap();
        assert!(config.devices.is_empty());
        assert_eq!(config.lines["1001"].label, "Unassigned");

        let file_mode = BASE.replace(
            "configuration_source = sorcery",
            "configuration_source = file",
        );
        assert!(ModuleConfig::parse(&file_mode).is_ok());
        let unassigned_file = "[general]\nconfiguration_source=file\n[1001]\ntype=line\n";
        assert!(matches!(
            ModuleConfig::parse(unassigned_file),
            Err(super::super::ConfigError::Empty)
        ));
    }

    #[test]
    fn codec_rejects_missing_positions_empty_tombstones_and_duplicate_numeric_positions() {
        for fields in [
            vec![field("button", "speed_dial, Bad, 2000")],
            vec![field("button.0001", "")],
            vec![
                field("button.01", "speed_dial, One, 2001"),
                field("button.001", "speed_dial, Duplicate, 2002"),
            ],
            vec![field("description.0001", "indexed scalar")],
        ] {
            let error = object_overlay(
                ConfigOverlayKind::Device,
                &OrderedSorceryObject::new("SEP001122334455", fields),
            )
            .unwrap_err();
            assert!(matches!(
                error,
                SorceryObjectError::MissingPosition(_)
                    | SorceryObjectError::EmptyIndexedValue(_)
                    | SorceryObjectError::DuplicatePosition { .. }
                    | SorceryObjectError::IndexedScalar(_)
            ));
        }
    }

    #[test]
    fn codec_orders_interleaved_operations_by_numeric_position() {
        let overlay = object_overlay(
            ConfigOverlayKind::Device,
            &OrderedSorceryObject::new(
                "SEP001122334455",
                vec![
                    field("allow.0010", "g722"),
                    field("permit.0008", "192.0.2.0/24"),
                    field("allow.0007", "ulaw"),
                    field("disallow.0002", "all"),
                    field("deny.0001", "0.0.0.0/0"),
                ],
            ),
        )
        .unwrap();

        assert_eq!(
            overlay
                .values
                .iter()
                .map(|field| (field.key.as_str(), field.value.as_deref().unwrap()))
                .collect::<Vec<_>>(),
            [
                ("deny", "0.0.0.0/0"),
                ("disallow", "all"),
                ("allow", "ulaw"),
                ("permit", "192.0.2.0/24"),
                ("allow", "g722"),
            ]
        );
    }

    #[test]
    fn codec_orders_indexed_dnd_schedules() {
        let overlay = object_overlay(
            ConfigOverlayKind::Device,
            &OrderedSorceryObject::new(
                "SEP001122334455",
                vec![
                    field("dnd_schedule.0010", "23:00-09:00, fri-sun, reject"),
                    field("dnd_schedule.0001", "22:00-07:00, mon-thu, silent"),
                ],
            ),
        )
        .unwrap();

        assert_eq!(
            overlay
                .values
                .iter()
                .map(|field| (field.key.as_str(), field.value.as_deref().unwrap()))
                .collect::<Vec<_>>(),
            [
                ("dnd_schedule", "22:00-07:00, mon-thu, silent"),
                ("dnd_schedule", "23:00-09:00, fri-sun, reject"),
            ]
        );
    }

    #[test]
    fn object_ids_cannot_collide_across_types_or_with_static_templates() {
        let duplicate = SorceryInventory {
            devices: vec![OrderedSorceryObject::new("same", Vec::new())],
            lines: vec![OrderedSorceryObject::new("same", Vec::new())],
        };
        assert!(matches!(
            inventory_overlays(&duplicate),
            Err(SorceryObjectError::DuplicateObject { .. })
        ));

        let source = Arc::new(MemorySorcery::with_desired(SorceryInventory {
            devices: vec![OrderedSorceryObject::new("desk", Vec::new())],
            lines: Vec::new(),
        }));
        let provider = SorceryConfigurationProvider::new(
            MemoryStatic(BASE),
            Arc::<MemorySorcery>::clone(&source),
        );
        assert!(provider.load().is_err());
    }

    #[test]
    fn inventory_json_round_trip_preserves_field_order() {
        let inventory = inventory("JSON");
        let json = serde_json::to_string(&inventory).unwrap();
        assert_eq!(
            serde_json::from_str::<SorceryInventory>(&json).unwrap(),
            inventory
        );
    }

    #[test]
    fn invalid_desired_falls_back_and_only_activated_desired_is_persisted() {
        let valid = inventory("Last known good");
        let invalid = SorceryInventory {
            devices: vec![OrderedSorceryObject::new(
                "SEP001122334455",
                vec![field("line.0001", "missing")],
            )],
            lines: Vec::new(),
        };
        let source = Arc::new(MemorySorcery::with_desired(invalid));
        *source.lkg.lock().unwrap() = Some(valid);
        let provider = SorceryConfigurationProvider::new(
            MemoryStatic(BASE),
            Arc::<MemorySorcery>::clone(&source),
        );

        let fallback = provider.load().unwrap();
        assert_eq!(fallback.lines["1001"].label, "Last known good");
        assert!(provider.last_rejected_desired().is_some());
        provider.activated(&fallback).unwrap();
        assert!(source.stores.lock().unwrap().is_empty());

        let desired = inventory("New desired");
        source.replace_desired(desired.clone());
        let candidate = provider.refresh().unwrap();
        assert!(source.stores.lock().unwrap().is_empty());
        provider.activated(&candidate).unwrap();
        assert_eq!(source.stores.lock().unwrap().as_slice(), [desired]);
    }

    #[test]
    fn invalid_live_refresh_is_reported_instead_of_reactivating_lkg() {
        let source = Arc::new(MemorySorcery::with_desired(inventory("Active")));
        let provider = SorceryConfigurationProvider::new(
            MemoryStatic(BASE),
            Arc::<MemorySorcery>::clone(&source),
        );
        let active = provider.load().unwrap();
        provider.activated(&active).unwrap();
        source.replace_desired(SorceryInventory {
            devices: vec![OrderedSorceryObject::new(
                "SEP001122334455",
                vec![field("line.0001", "missing")],
            )],
            lines: Vec::new(),
        });

        assert!(provider.refresh().is_err());
        assert!(provider.last_rejected_desired().is_some());
        assert_eq!(source.stores.lock().unwrap().len(), 1);
    }

    #[test]
    fn activation_rejects_a_configuration_not_returned_by_latest_load() {
        let source = Arc::new(MemorySorcery::with_desired(inventory("First")));
        let provider = SorceryConfigurationProvider::new(
            MemoryStatic(BASE),
            Arc::<MemorySorcery>::clone(&source),
        );
        let first = provider.load().unwrap();
        source.replace_desired(inventory("Second"));
        let second = provider.refresh().unwrap();

        assert!(provider.activated(&first).is_err());
        provider.activated(&second).unwrap();
    }
}
