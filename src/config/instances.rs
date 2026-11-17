use crate::instance::{InstanceKind, APPLICATIONS};
use anyhow::{bail, Result};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InstanceConfig {
    pub enabled: bool,
    pub autostart: bool,
    pub network_profile: String,
    pub isolation_policy: Option<String>,
    pub auth_browser: Option<String>,
}

impl Default for InstanceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            autostart: false,
            network_profile: "default".into(),
            isolation_policy: None,
            auth_browser: None,
        }
    }
}

pub fn policy(kind: InstanceKind) -> &'static str {
    match kind {
        InstanceKind::Web | InstanceKind::GeminiWeb => "managed-browser",
        InstanceKind::Desktop => "desktop-lite",
        InstanceKind::Code => "managed-cli",
        InstanceKind::VsCode => "managed-ide",
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct InstanceConfigs(BTreeMap<String, InstanceConfig>);

impl Default for InstanceConfigs {
    fn default() -> Self {
        Self(
            APPLICATIONS
                .iter()
                .map(|app| {
                    (
                        app.id.into(),
                        InstanceConfig {
                            isolation_policy: Some(policy(app.kind).into()),
                            auth_browser: (app.kind == InstanceKind::Desktop)
                                .then(|| "claude-web".into()),
                            ..InstanceConfig::default()
                        },
                    )
                })
                .collect(),
        )
    }
}

impl<'de> Deserialize<'de> for InstanceConfigs {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let input = BTreeMap::<String, InstanceConfig>::deserialize(deserializer)?;
        let mut result = Self::default();
        for (id, mut config) in input {
            let default = result.0.get(&id).ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "unknown fixed instance ID: {id}; use canonical IDs"
                ))
            })?;
            if config.isolation_policy.is_none() {
                config
                    .isolation_policy
                    .clone_from(&default.isolation_policy);
            }
            if config.auth_browser.is_none() {
                config.auth_browser.clone_from(&default.auth_browser);
            }
            result.0.insert(id, config);
        }
        result.validate().map_err(serde::de::Error::custom)?;
        Ok(result)
    }
}

impl InstanceConfigs {
    pub fn get(&self, kind: InstanceKind) -> &InstanceConfig {
        &self.0[kind.definition().id]
    }

    pub fn validate(&self) -> Result<()> {
        for app in &APPLICATIONS {
            let config = self.get(app.kind);
            if config.network_profile != "default" {
                bail!("{}: only network_profile=default is supported", app.id);
            }
            if config.isolation_policy.as_deref() != Some(policy(app.kind)) {
                bail!("{}: isolation_policy must be {}", app.id, policy(app.kind));
            }
            if config.autostart && !config.enabled {
                bail!("{}: disabled instance cannot autostart", app.id);
            }
            match app.kind {
                InstanceKind::Desktop if config.auth_browser.as_deref() == Some("claude-web") => {}
                InstanceKind::Desktop => bail!("{}: auth_browser must be claude-web", app.id),
                _ if config.auth_browser.is_some() => {
                    bail!("{}: auth_browser is only supported for Desktop", app.id)
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn require_enabled(&self, kind: InstanceKind) -> Result<()> {
        if !self.get(kind).enabled {
            bail!(
                "{} is disabled by instance configuration",
                kind.definition().id
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_defaults_share_network_but_not_desktop_auth() {
        let configs = InstanceConfigs::default();
        let gemini = configs.get(InstanceKind::GeminiWeb);
        assert!(gemini.enabled);
        assert!(!gemini.autostart);
        assert_eq!(gemini.network_profile, "default");
        assert_eq!(gemini.isolation_policy.as_deref(), Some("managed-browser"));
        assert!(gemini.auth_browser.is_none());
        assert!(
            toml::from_str::<InstanceConfigs>("[claude-desktop]\nauth_browser = 'gemini-web'")
                .is_err()
        );
    }

    #[test]
    fn partial_configs_fill_defaults_without_enabling_autostart() {
        let configs: InstanceConfigs = toml::from_str("[claude-web]\nenabled = false").unwrap();
        assert!(!configs.get(InstanceKind::Web).enabled);
        assert_eq!(
            configs.get(InstanceKind::Web).isolation_policy.as_deref(),
            Some("managed-browser")
        );
        assert!(configs.get(InstanceKind::Desktop).enabled);
        for kind in InstanceKind::ALL {
            assert!(!configs.get(kind).autostart);
        }
        assert!(configs
            .require_enabled(InstanceKind::parse("web").unwrap())
            .is_err());
        assert!(configs
            .require_enabled(InstanceKind::parse("claude-web").unwrap())
            .is_err());
    }

    #[test]
    fn rejects_unknown_ids_fields_and_unsafe_or_inconsistent_policies() {
        for text in [
            "[web]\nenabled = false",
            "[chatgpt-web]\nenabled = true",
            "[claude-web]\nenabeld = false",
            "[claude-web]\nnetwork_profile = 'direct'",
            "[claude-web]\nisolation_policy = 'desktop-lite'",
            "[claude-web]\nenabled = false\nautostart = true",
            "[claude-desktop]\nauth_browser = 'web'",
            "[claude-code]\nauth_browser = 'claude-web'",
            "[claude-web]\nprofile_dir = '/tmp/other'",
        ] {
            assert!(
                toml::from_str::<InstanceConfigs>(text).is_err(),
                "accepted {text}"
            );
        }
    }

    #[test]
    fn valid_autostart_and_auth_reference_round_trip() {
        let configs: InstanceConfigs =
            toml::from_str("[claude-desktop]\nautostart = true\nauth_browser = 'claude-web'")
                .unwrap();
        assert!(configs.get(InstanceKind::Desktop).autostart);
        let restored: InstanceConfigs =
            toml::from_str(&toml::to_string(&configs).unwrap()).unwrap();
        assert_eq!(
            restored.get(InstanceKind::Desktop).auth_browser.as_deref(),
            Some("claude-web")
        );
    }
}
