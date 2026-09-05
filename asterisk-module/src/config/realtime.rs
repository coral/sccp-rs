//! Ordered, policy-free realtime configuration values and backend port.
//!
//! This domain boundary deliberately preserves distinctions that a generic map
//! would lose: result-row order, field order, repeated field names, database
//! `NULL`, and explicit empty strings. Predicates are bounded owned text and
//! remain in caller order. Concrete Asterisk allocation and traversal belongs
//! to the native adapter.

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
use std::ffi::CString;
use std::ffi::NulError;
use std::str::Utf8Error;

use thiserror::Error;

/// One lookup condition. Names may include backend-supported operators such as
/// `position >=`, and repeated names are retained in their original order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RealtimePredicate {
    pub name: String,
    pub value: String,
}

impl RealtimePredicate {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// One name/value occurrence from a realtime row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RealtimeField {
    pub name: String,
    /// `None` is a database null. `Some("")` is an explicitly empty value.
    pub value: Option<String>,
}

/// One row, retaining backend field order and duplicate names.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RealtimeRow {
    pub fields: Vec<RealtimeField>,
}

impl RealtimeRow {
    /// Returns every occurrence of `name` without collapsing repeated fields.
    pub fn values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Option<String>> + 'a {
        self.fields
            .iter()
            .filter(move |field| field.name == name)
            .map(|field| &field.value)
    }
}

/// One backend read, optionally tied to an atomic configuration revision.
///
/// Sources backed by versioned snapshots return the same revision for every
/// family in one candidate. Sources without snapshot metadata may leave it
/// unset; their rows retain the legacy best-effort behavior.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RealtimeLoad {
    pub revision: Option<String>,
    pub rows: Vec<RealtimeRow>,
}

/// Owned realtime query boundary used by normalized configuration providers.
///
/// Implementations retain row order, field order, duplicate names, database
/// nulls, and explicit empty strings. No Asterisk pointer, status code, or
/// allocation handle is part of this port.
pub trait RealtimeConfigurationSource: Send + Sync {
    fn load_many(
        &self,
        family: &str,
        predicates: &[RealtimePredicate],
    ) -> Result<RealtimeLoad, RealtimeError>;
}

#[derive(Debug, Error)]
pub enum RealtimeError {
    #[error("{field} contains a NUL byte")]
    InvalidText {
        field: String,
        #[source]
        source: NulError,
    },

    #[error("realtime family must not be empty")]
    EmptyFamily,

    #[error("at least one realtime predicate is required")]
    MissingPredicates,

    #[error("realtime predicate {index} has an empty name")]
    EmptyPredicateName { index: usize },

    #[error("no realtime backend is configured for the requested family")]
    BackendUnavailable,

    #[error("realtime backend retrieval failed")]
    BackendFailure,

    #[error("native realtime data at {location} is not UTF-8")]
    InvalidNativeText {
        location: String,
        #[source]
        source: Utf8Error,
    },

    #[error("native realtime result has no field name at row {row}, field {field}")]
    MissingFieldName { row: usize, field: usize },

    #[error("versioned realtime row {row} is malformed: {message}")]
    InvalidSnapshotRow { row: usize, message: String },

    #[error("versioned realtime result contains more than one revision")]
    MixedSnapshotRevisions { expected: String, actual: String },
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
fn field_value<'a>(row: &'a RealtimeRow, name: &str) -> Option<&'a Option<String>> {
    row.fields
        .iter()
        .find(|field| field.name == name)
        .map(|field| &field.value)
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
fn required_text(row: &RealtimeRow, row_index: usize, name: &str) -> Result<String, RealtimeError> {
    field_value(row, name)
        .and_then(Option::as_ref)
        .cloned()
        .ok_or_else(|| RealtimeError::InvalidSnapshotRow {
            row: row_index,
            message: format!("{name} is missing or NULL"),
        })
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
fn decode_hex_text(value: &str, row: usize) -> Result<String, RealtimeError> {
    if !value.len().is_multiple_of(2) {
        return Err(RealtimeError::InvalidSnapshotRow {
            row,
            message: "_field_value contains odd-length hexadecimal text".to_owned(),
        });
    }
    let bytes = value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digit = |byte: u8| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            };
            digit(pair[0])
                .zip(digit(pair[1]))
                .map(|(high, low)| high << 4 | low)
                .ok_or_else(|| RealtimeError::InvalidSnapshotRow {
                    row,
                    message: "_field_value contains non-hexadecimal text".to_owned(),
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    String::from_utf8(bytes).map_err(|_| RealtimeError::InvalidSnapshotRow {
        row,
        message: "_field_value does not encode UTF-8".to_owned(),
    })
}

/// Restores versioned schema rows into ordinary ordered configuration rows.
///
/// Unversioned results pass through unchanged. Versioned results are sorted by
/// their schema order key, checked for one revision, grouped by section, and
/// decoded without losing repeated fields or the distinction between SQL NULL
/// and an empty string.
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn decode_snapshot_rows(rows: Vec<RealtimeRow>) -> Result<RealtimeLoad, RealtimeError> {
    if !rows
        .iter()
        .any(|row| field_value(row, "_revision").is_some())
    {
        return Ok(RealtimeLoad {
            revision: None,
            rows,
        });
    }

    let mut ordered_rows = rows
        .into_iter()
        .enumerate()
        .map(|(row_index, row)| {
            let order = required_text(&row, row_index, "_row_order")?
                .parse::<u64>()
                .map_err(|_| RealtimeError::InvalidSnapshotRow {
                    row: row_index,
                    message: "_row_order is not an unsigned integer".to_owned(),
                })?;
            Ok((order, row_index, row))
        })
        .collect::<Result<Vec<_>, _>>()?;
    ordered_rows.sort_unstable_by_key(|(order, _, _)| *order);
    if let Some(window) = ordered_rows
        .windows(2)
        .find(|window| window[0].0 == window[1].0)
    {
        return Err(RealtimeError::InvalidSnapshotRow {
            row: window[1].1,
            message: format!("_row_order {} occurs more than once", window[1].0),
        });
    }

    let mut revision = None::<String>;
    let mut decoded = Vec::<RealtimeRow>::new();
    let mut current_section = None::<String>;
    for (_, row_index, row) in ordered_rows {
        let actual_revision = required_text(&row, row_index, "_revision")?;
        if let Some(expected) = revision.as_ref()
            && expected != &actual_revision
        {
            return Err(RealtimeError::MixedSnapshotRevisions {
                expected: expected.clone(),
                actual: actual_revision,
            });
        }
        revision.get_or_insert(actual_revision);

        let metadata = field_value(&row, "_metadata")
            .and_then(Option::as_deref)
            .is_some_and(|value| value == "1");
        if metadata {
            continue;
        }

        let section = required_text(&row, row_index, "name")?;
        let name = required_text(&row, row_index, "_field_name")?;
        let kind = required_text(&row, row_index, "_field_kind")?;
        let encoded_value = required_text(&row, row_index, "_field_value")?;
        let value = match kind.as_str() {
            "null" => None,
            "empty" => Some(String::new()),
            "value" => Some(decode_hex_text(&encoded_value, row_index)?),
            _ => {
                return Err(RealtimeError::InvalidSnapshotRow {
                    row: row_index,
                    message: "_field_kind has an unknown value".to_owned(),
                });
            }
        };

        if current_section.as_deref() != Some(section.as_str()) {
            decoded.push(RealtimeRow {
                fields: vec![RealtimeField {
                    name: "name".to_owned(),
                    value: Some(section.clone()),
                }],
            });
            current_section = Some(section);
        }
        decoded
            .last_mut()
            .expect("section was inserted")
            .fields
            .push(RealtimeField { name, value });
    }
    Ok(RealtimeLoad {
        revision,
        rows: decoded,
    })
}

#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn validate_query(
    family: &str,
    predicates: &[RealtimePredicate],
) -> Result<(), RealtimeError> {
    if family.is_empty() {
        return Err(RealtimeError::EmptyFamily);
    }
    CString::new(family).map_err(|source| RealtimeError::InvalidText {
        field: "family".to_owned(),
        source,
    })?;
    if predicates.is_empty() {
        return Err(RealtimeError::MissingPredicates);
    }
    for (index, predicate) in predicates.iter().enumerate() {
        if predicate.name.is_empty() {
            return Err(RealtimeError::EmptyPredicateName { index });
        }
        CString::new(predicate.name.as_str()).map_err(|source| RealtimeError::InvalidText {
            field: format!("predicate[{index}].name"),
            source,
        })?;
        CString::new(predicate.value.as_str()).map_err(|source| RealtimeError::InvalidText {
            field: format!("predicate[{index}].value"),
            source,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    type CapturedQuery = Arc<Mutex<Option<(String, Vec<RealtimePredicate>)>>>;

    #[derive(Clone)]
    struct FakeRealtime {
        result: Result<Vec<RealtimeRow>, &'static str>,
        query: CapturedQuery,
    }

    impl RealtimeConfigurationSource for FakeRealtime {
        fn load_many(
            &self,
            family: &str,
            predicates: &[RealtimePredicate],
        ) -> Result<RealtimeLoad, RealtimeError> {
            validate_query(family, predicates)?;
            *self.query.lock().unwrap() = Some((family.to_owned(), predicates.to_vec()));
            self.result
                .clone()
                .map(|rows| RealtimeLoad {
                    revision: Some("7".to_owned()),
                    rows,
                })
                .map_err(|_| RealtimeError::BackendFailure)
        }
    }

    fn predicate(name: &str, value: &str) -> RealtimePredicate {
        RealtimePredicate::new(name, value)
    }

    fn field(name: &str, value: Option<&str>) -> RealtimeField {
        RealtimeField {
            name: name.to_owned(),
            value: value.map(str::to_owned),
        }
    }

    fn snapshot_row(fields: &[(&str, Option<&str>)]) -> RealtimeRow {
        RealtimeRow {
            fields: fields
                .iter()
                .map(|(name, value)| field(name, *value))
                .collect(),
        }
    }

    #[test]
    fn typed_backend_preserves_query_row_field_and_repeated_field_order() {
        let query = Arc::new(Mutex::new(None));
        let source = FakeRealtime {
            result: Ok(vec![
                RealtimeRow {
                    fields: vec![
                        field("name", Some("SEP001")),
                        field("button", Some("line,1000")),
                        field("button", Some("speeddial,Support,2000")),
                    ],
                },
                RealtimeRow {
                    fields: vec![
                        field("name", Some("SEP002")),
                        field("button", Some("line,1001")),
                    ],
                },
            ]),
            query: Arc::clone(&query),
        };
        let predicates = [predicate("tenant", "west"), predicate("tenant", "fallback")];
        let load = source.load_many("devices", &predicates).unwrap();

        assert_eq!(load.revision.as_deref(), Some("7"));
        let rows = load.rows;
        assert_eq!(
            rows[0].values("button").cloned().collect::<Vec<_>>(),
            vec![
                Some("line,1000".to_owned()),
                Some("speeddial,Support,2000".to_owned()),
            ]
        );
        assert_eq!(rows[1].fields[0].value.as_deref(), Some("SEP002"));
        assert_eq!(
            query.lock().unwrap().as_ref().unwrap(),
            &("devices".to_owned(), predicates.to_vec())
        );
    }

    #[test]
    fn typed_rows_distinguish_null_empty_and_repeated_values() {
        let row = RealtimeRow {
            fields: vec![
                field("description", None),
                field("description", Some("")),
                field("description", Some("Desk")),
            ],
        };
        assert_eq!(
            row.values("description").cloned().collect::<Vec<_>>(),
            vec![None, Some(String::new()), Some("Desk".to_owned())]
        );
    }

    #[test]
    fn snapshot_rows_restore_order_repeats_null_and_empty() {
        let load = decode_snapshot_rows(vec![
            snapshot_row(&[
                ("_row_order", Some("0")),
                ("_revision", Some("12")),
                ("_metadata", Some("1")),
            ]),
            snapshot_row(&[
                ("_row_order", Some("2")),
                ("_revision", Some("12")),
                ("_metadata", Some("0")),
                ("name", Some("SEP001")),
                ("_field_name", Some("button")),
                ("_field_kind", Some("value")),
                (
                    "_field_value",
                    Some("73706565645f6469616c2c537570706f72742c32303030"),
                ),
            ]),
            snapshot_row(&[
                ("_row_order", Some("1")),
                ("_revision", Some("12")),
                ("_metadata", Some("0")),
                ("name", Some("SEP001")),
                ("_field_name", Some("button")),
                ("_field_kind", Some("value")),
                ("_field_value", Some("6c696e652c31303030")),
            ]),
            snapshot_row(&[
                ("_row_order", Some("3")),
                ("_revision", Some("12")),
                ("_metadata", Some("0")),
                ("name", Some("SEP001")),
                ("_field_name", Some("description")),
                ("_field_kind", Some("null")),
                ("_field_value", Some("_")),
            ]),
            snapshot_row(&[
                ("_row_order", Some("4")),
                ("_revision", Some("12")),
                ("_metadata", Some("0")),
                ("name", Some("SEP001")),
                ("_field_name", Some("label")),
                ("_field_kind", Some("empty")),
                ("_field_value", Some("_")),
            ]),
        ])
        .unwrap();

        assert_eq!(load.revision.as_deref(), Some("12"));
        assert_eq!(load.rows.len(), 1);
        assert_eq!(
            load.rows[0].values("button").cloned().collect::<Vec<_>>(),
            [
                Some("line,1000".to_owned()),
                Some("speed_dial,Support,2000".to_owned())
            ]
        );
        assert_eq!(load.rows[0].values("description").next(), Some(&None));
        assert_eq!(
            load.rows[0].values("label").next(),
            Some(&Some(String::new()))
        );
    }

    #[test]
    fn snapshot_rows_reject_mixed_revisions() {
        let error = decode_snapshot_rows(vec![
            snapshot_row(&[
                ("_row_order", Some("0")),
                ("_revision", Some("12")),
                ("_metadata", Some("1")),
            ]),
            snapshot_row(&[
                ("_row_order", Some("1")),
                ("_revision", Some("13")),
                ("_metadata", Some("1")),
            ]),
        ])
        .unwrap_err();

        assert!(matches!(
            error,
            RealtimeError::MixedSnapshotRevisions {
                expected,
                actual
            } if expected == "12" && actual == "13"
        ));
    }

    #[test]
    fn validates_queries_before_backend_dispatch() {
        assert!(matches!(
            validate_query("", &[predicate("name", "SEP001")]),
            Err(RealtimeError::EmptyFamily)
        ));
        assert!(matches!(
            validate_query("bad\0family", &[predicate("name", "SEP001")]),
            Err(RealtimeError::InvalidText { .. })
        ));
        assert!(matches!(
            validate_query("devices", &[]),
            Err(RealtimeError::MissingPredicates)
        ));
        assert!(matches!(
            validate_query("devices", &[predicate("", "value")]),
            Err(RealtimeError::EmptyPredicateName { index: 0 })
        ));
        assert!(matches!(
            validate_query("devices", &[predicate("name", "bad\0value")]),
            Err(RealtimeError::InvalidText { .. })
        ));
    }

    #[test]
    fn typed_backend_failures_remain_distinct_from_no_rows() {
        let query = Arc::new(Mutex::new(None));
        let empty = FakeRealtime {
            result: Ok(Vec::new()),
            query: Arc::clone(&query),
        };
        assert!(
            empty
                .load_many("devices", &[predicate("name", "SEP001")])
                .unwrap()
                .rows
                .is_empty()
        );

        let failing = FakeRealtime {
            result: Err("backend"),
            query,
        };
        assert!(matches!(
            failing.load_many("devices", &[predicate("name", "SEP001")]),
            Err(RealtimeError::BackendFailure)
        ));
    }
}
