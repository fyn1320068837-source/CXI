//! Built-in application identities. Registration grants no launch privileges.
//! Legacy keys remain the storage, status and directory keys during migration.
use super::{CodeAdapter, DesktopAdapter, InstanceAdapter, VsCodeAdapter, WebAdapter};
use crate::geoip::SharedGeoProfile;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum InstanceKind {
    Code,
    Desktop,
    VsCode,
    Web,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    Cli,
    Desktop,
    Ide,
    Browser,
}

#[derive(Debug, Serialize)]
pub struct ApplicationDefinition {
    pub id: &'static str,
    pub product: &'static str,
    pub display_name: &'static str,
    pub runtime_kind: RuntimeKind,
    pub legacy_alias: &'static str,
    #[serde(skip)]
    pub kind: InstanceKind,
    #[serde(skip)]
    pub(super) factory: fn(String, SharedGeoProfile) -> Arc<dyn InstanceAdapter>,
    #[serde(skip)]
    pub(super) requires_geo_profile: bool,
    #[serde(skip)]
    pub(super) cleanup_priority: u8,
}

pub static APPLICATIONS: [ApplicationDefinition; 4] = [
    ApplicationDefinition {
        id: "claude-code",
        product: "claude",
        display_name: "Claude Code",
        runtime_kind: RuntimeKind::Cli,
        legacy_alias: "code",
        kind: InstanceKind::Code,
        factory: |root, geo| Arc::new(CodeAdapter::new_with_geo_profile(root, geo)),
        requires_geo_profile: false,
        cleanup_priority: 30,
    },
    ApplicationDefinition {
        id: "claude-desktop",
        product: "claude",
        display_name: "Claude Desktop",
        runtime_kind: RuntimeKind::Desktop,
        legacy_alias: "desktop",
        kind: InstanceKind::Desktop,
        factory: |root, geo| Arc::new(DesktopAdapter::new_with_geo_profile(root, geo)),
        requires_geo_profile: false,
        cleanup_priority: 20,
    },
    ApplicationDefinition {
        id: "claude-vscode",
        product: "claude",
        display_name: "Claude VS Code",
        runtime_kind: RuntimeKind::Ide,
        legacy_alias: "vscode",
        kind: InstanceKind::VsCode,
        factory: |root, geo| {
            Arc::new(VsCodeAdapter::new_with_geo_profile(
                format!("{root}/data"),
                format!("{root}/extensions"),
                geo,
            ))
        },
        requires_geo_profile: false,
        cleanup_priority: 40,
    },
    ApplicationDefinition {
        id: "claude-web",
        product: "claude",
        display_name: "Claude Web",
        runtime_kind: RuntimeKind::Browser,
        legacy_alias: "web",
        kind: InstanceKind::Web,
        factory: |root, geo| Arc::new(WebAdapter::new_with_geo_profile(root, geo)),
        requires_geo_profile: true,
        cleanup_priority: 10,
    },
];

impl ApplicationDefinition {
    pub(super) fn profile_root(&self, home: &str) -> String {
        // IDs change at the API boundary, never on disk during this migration.
        format!("{home}/.cim/instances/{}", self.legacy_alias)
    }

    pub(super) fn create_adapter(
        &self,
        home: &str,
        geo: SharedGeoProfile,
    ) -> Arc<dyn InstanceAdapter> {
        (self.factory)(self.profile_root(home), geo)
    }
}

impl InstanceKind {
    pub const ALL: [Self; 4] = [Self::Code, Self::Desktop, Self::VsCode, Self::Web];

    pub fn definition(self) -> &'static ApplicationDefinition {
        APPLICATIONS
            .iter()
            .find(|app| app.kind == self)
            .expect("every built-in instance kind must be registered")
    }

    /// Compatibility key: do not replace persisted keys or directory names.
    pub fn as_str(self) -> &'static str {
        self.definition().legacy_alias
    }

    pub fn parse(value: &str) -> Option<Self> {
        APPLICATIONS
            .iter()
            .find(|app| app.id == value || app.legacy_alias == value)
            .map(|app| app.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn factories_keep_legacy_profile_roots_and_geo_gate() {
        let geo = Arc::new(tokio::sync::RwLock::new(crate::geoip::GeoProfile::default()));
        for (id, expected) in [
            ("claude-code", "/fixture/.cim/instances/code"),
            ("claude-desktop", "/fixture/.cim/instances/desktop"),
            ("claude-vscode", "/fixture/.cim/instances/vscode"),
            ("claude-web", "/fixture/.cim/instances/web"),
        ] {
            let app = InstanceKind::parse(id).unwrap().definition();
            assert_eq!(app.profile_root("/fixture"), expected);
            assert_eq!(app.requires_geo_profile, id == "claude-web");
            let adapter = app.create_adapter("/fixture", geo.clone());
            assert_eq!(Arc::strong_count(&adapter), 1);
        }
    }

    #[test]
    fn registry_is_complete_and_identifiers_do_not_collide() {
        let mut names = HashSet::new();
        let mut kinds = HashSet::new();
        for app in &APPLICATIONS {
            assert!(names.insert(app.id));
            assert!(names.insert(app.legacy_alias));
            assert!(kinds.insert(app.kind));
            assert_eq!(InstanceKind::parse(app.id), Some(app.kind));
            assert_eq!(InstanceKind::parse(app.legacy_alias), Some(app.kind));
        }
        assert_eq!(kinds, InstanceKind::ALL.into_iter().collect());
    }

    #[test]
    fn canonical_ids_preserve_legacy_storage_and_serialization() {
        for (id, key, serialized) in [
            ("claude-code", "code", "Code"),
            ("claude-desktop", "desktop", "Desktop"),
            ("claude-vscode", "vscode", "VsCode"),
            ("claude-web", "web", "Web"),
        ] {
            let kind = InstanceKind::parse(id).unwrap();
            assert_eq!(kind.as_str(), key);
            assert_eq!(serde_json::to_value(kind).unwrap(), serialized);
            assert_eq!(
                serde_json::from_value::<InstanceKind>(serde_json::json!(serialized)).unwrap(),
                kind
            );
        }
    }

    #[test]
    fn unknown_or_ambiguous_identifiers_are_not_normalized() {
        for value in [
            "claude",
            "chatgpt-web",
            "CLAUDE-WEB",
            " web",
            "web/../desktop",
            "",
        ] {
            assert_eq!(InstanceKind::parse(value), None);
        }
    }
}
