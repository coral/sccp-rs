//! Shared mechanics for policy-specific configuration section parsers.

use std::collections::HashSet;

use super::{ConfigError, RawSection, RawValue, invalid_option, normalize_name};

/// A typed view of one raw section with semantic-slot tracking.
///
/// Individual section parsers still own their accepted names and value types.
/// This helper centralizes duplicate/alias accounting and makes diagnostics
/// redact values based on the option name instead of caller discipline.
pub(super) struct SectionValues<'a> {
    section: &'a RawSection,
    claimed: HashSet<&'static str>,
}

impl<'a> SectionValues<'a> {
    pub(super) fn new(section: &'a RawSection) -> Self {
        Self {
            section,
            claimed: HashSet::new(),
        }
    }

    pub(super) fn claim_alias(
        &mut self,
        identity: &'static str,
        entry: &RawValue,
    ) -> Result<(), ConfigError> {
        if self.claimed.insert(identity) {
            return Ok(());
        }
        Err(invalid_option(
            entry.diagnostic_key(),
            self.diagnostic_value(entry),
            "one value (aliases may not be combined)",
            self.sensitive(entry),
        ))
    }

    pub(super) fn set_once<T>(
        &self,
        setting: &mut Option<T>,
        key: &str,
        raw: &str,
        value: T,
    ) -> Result<(), ConfigError> {
        if setting.is_some() {
            let sensitive = raw == "<redacted>" || sensitive_option_name(key);
            return Err(invalid_option(
                self.section.diagnostic_key(key),
                if sensitive { "<redacted>" } else { raw },
                "one value (duplicates and aliases may not be combined)",
                sensitive,
            ));
        }
        *setting = Some(value);
        Ok(())
    }

    fn diagnostic_value<'b>(&self, entry: &'b RawValue) -> &'b str {
        if self.sensitive(entry) {
            "<redacted>"
        } else {
            &entry.value
        }
    }

    fn sensitive(&self, entry: &RawValue) -> bool {
        sensitive_option_name(&entry.key)
    }
}

pub(super) fn sensitive_option_name(name: &str) -> bool {
    matches!(
        normalize_name(name).as_str(),
        "secret"
            | "password"
            | "authorization"
            | "authtoken"
            | "token"
            | "key"
            | "accountcode"
            | "mobilitypin"
            | "pin"
            | "setvar"
            | "forwardall"
            | "forwardbusy"
            | "forwardnoanswer"
            | "hotlineextension"
            | "vmnum"
            | "voicemailnumber"
            | "trnsfvm"
            | "voicemailtransfer"
            | "transfertovoicemail"
            | "certfile"
            | "tlscombinedpem"
            | "tlsprivatekey"
            | "tlsprivatekeyfile"
            | "tlstruststore"
            | "tlscafile"
            | "backgroundimageurl"
            | "backgroundthumbnailurl"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(values: &[(&str, &str)]) -> RawSection {
        RawSection {
            name: "general".to_owned(),
            line: 1,
            values: values
                .iter()
                .enumerate()
                .map(|(index, (key, value))| RawValue {
                    key: (*key).to_owned(),
                    value: (*value).to_owned(),
                    line: index + 2,
                    section: "general".to_owned(),
                })
                .collect(),
            ..RawSection::default()
        }
    }

    #[test]
    fn aliases_share_a_slot_and_sensitive_unknown_values_are_redacted() {
        let qos_section = section(&[("sccptos", "ef"), ("signalingdscp", "46")]);
        let mut values = SectionValues::new(&qos_section);
        values
            .claim_alias("signaling_dscp", &qos_section.values[0])
            .unwrap();
        assert!(matches!(
            values.claim_alias("signaling_dscp", &qos_section.values[1]),
            Err(ConfigError::InvalidValue { .. })
        ));

        assert!(sensitive_option_name("Secret"));
        assert!(sensitive_option_name("p-in"));
        assert!(sensitive_option_name("background_image_url"));
        assert!(sensitive_option_name("background_thumbnail_url"));
    }
}
