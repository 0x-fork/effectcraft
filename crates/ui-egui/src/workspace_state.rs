//! Persistent workspace layouts, separate from document-bound and transient UI state.

use std::collections::BTreeMap;

use effectcraft_engine::config::ConfigStore;
use serde::{Deserialize, Serialize};

use crate::dock::{PanelKind, Workspace};
use crate::state::UiState;

const FILE: &str = "workspaces.json";
const MAX_BYTES: usize = 1024 * 1024;
const MAX_WORKSPACES: usize = 64;
/// Leave room for new viewers without approaching the u32 allocator limit.
const MAX_VIEWER_ID: u32 = 65_535;
/// The snapshot version: 2 holds craft-ui docking workspaces.
const VERSION: u32 = 2;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Snapshot {
    version: u32,
    workspace: String,
    dock: Workspace,
    focused: PanelKind,
    maximized: Option<PanelKind>,
    saved_workspaces: BTreeMap<String, Workspace>,
}

impl Snapshot {
    fn capture(ui: &UiState) -> Self {
        Self {
            version: VERSION,
            workspace: ui.workspace.clone(),
            dock: ui.dock.clone(),
            focused: ui.focused,
            maximized: ui.maximized,
            saved_workspaces: ui.saved_workspaces.clone(),
        }
    }

    fn apply(self, ui: &mut UiState) {
        ui.workspace = self.workspace;
        ui.dock = self.dock;
        ui.focused = self.focused;
        ui.maximized = self.maximized;
        ui.saved_workspaces = self.saved_workspaces;
        // A native window's place is made safe to hand to the windowing system.
        for workspace in std::iter::once(&mut ui.dock).chain(ui.saved_workspaces.values_mut()) {
            for window in &mut workspace.layout.floating {
                window.native = window.native.map(crate::dock_ui::sanitized_window_rect);
            }
        }
        // Reserve restored viewer slots without restoring any project-specific composition
        // links. New Viewer allocates from this map, including after switching named layouts.
        for workspace in std::iter::once(&ui.dock).chain(ui.saved_workspaces.values()) {
            for panel in crate::dock::referenced_panels(workspace) {
                if let PanelKind::Viewer(id) = panel {
                    ui.viewers.entry(id).or_insert(None);
                }
            }
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != VERSION {
            return Err(format!("unsupported workspace version {}", self.version));
        }
        let name_ok = |name: &str| !name.trim().is_empty() && name.len() <= 256;
        if !name_ok(&self.workspace) || self.saved_workspaces.len() > MAX_WORKSPACES || self.saved_workspaces.keys().any(|name| !name_ok(name)) {
            return Err("workspace names or count exceed supported limits".into());
        }
        if !crate::dock::WORKSPACES.contains(&self.workspace.as_str()) && !self.saved_workspaces.contains_key(&self.workspace) {
            return Err("current workspace has no saved definition".into());
        }
        let panel_ok = |panel: PanelKind| match panel {
            PanelKind::Viewer(id) => (1..=MAX_VIEWER_ID).contains(&id),
            _ => true,
        };
        if !panel_ok(self.focused) || self.maximized.is_some_and(|panel| !panel_ok(panel)) {
            return Err("workspace viewer ID is outside the supported range 1–65535".into());
        }
        for workspace in std::iter::once(&self.dock).chain(self.saved_workspaces.values()) {
            // craft-ui checks the trees, selections, sizes, rects and limits.
            workspace.validate().map_err(|error| error.to_string())?;
            if crate::dock::referenced_panels(workspace).into_iter().any(|panel| !panel_ok(panel)) {
                return Err("workspace viewer ID is outside the supported range 1–65535".into());
            }
        }
        Ok(())
    }
}

pub(crate) struct WorkspaceStore {
    saved: Snapshot,
    last_attempt: f64,
    writable: bool,
    pub(crate) error: Option<String>,
}

impl WorkspaceStore {
    pub(crate) fn load(config: Option<&dyn ConfigStore>, ui: &mut UiState) -> Self {
        let loaded = config.map_or(Ok(None), |c| c.read_bounded(FILE, MAX_BYTES)).map_err(|e| e.to_string()).and_then(|text| {
            text.map(|text| {
                let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
                // D2: migrate the version 1 snapshot (EffectCraft's own dock tree, craft-libs
                // 5c874d51) and the unversioned `{workspaces, floating}` record here. Until then
                // they load as an error: this session keeps the default layout and leaves the
                // file untouched.
                let version = value.get("version").and_then(serde_json::Value::as_u64);
                if version.is_none_or(|v| v < u64::from(VERSION)) {
                    return Err("this workspace file is from an earlier EffectCraft version".to_string());
                }
                let snapshot = serde_json::from_value::<Snapshot>(value).map_err(|e| e.to_string())?;
                snapshot.validate()?;
                Ok(snapshot)
            })
            .transpose()
        });
        let error = match loaded {
            Ok(Some(snapshot)) => {
                snapshot.apply(ui);
                None
            }
            Ok(None) => None,
            Err(e) => Some(format!("Could not load workspaces: {e}. The existing file will be preserved for this session.")),
        };
        if let Some(error) = &error {
            log::warn!("{error}");
            ui.status = error.clone();
        }
        // An unreadable or newer file is never replaced by this session, even after a later
        // layout edit. Ordinary failed writes remain dirty and retryable.
        Self { saved: Snapshot::capture(ui), last_attempt: f64::NEG_INFINITY, writable: error.is_none(), error }
    }

    pub(crate) fn due(&mut self, now: f64) -> bool {
        if now - self.last_attempt < 1.0 {
            return false;
        }
        self.last_attempt = now;
        true
    }

    pub(crate) fn has_changes(&self, ui: &UiState) -> bool {
        if !self.writable {
            return false;
        }
        self.saved.workspace != ui.workspace
            || self.saved.dock != ui.dock
            || self.saved.focused != ui.focused
            || self.saved.maximized != ui.maximized
            || self.saved.saved_workspaces != ui.saved_workspaces
    }

    pub(crate) fn save(&mut self, config: Option<&dyn ConfigStore>, ui: &UiState) -> Result<bool, String> {
        let Some(config) = config else { return Ok(false) };
        if !self.has_changes(ui) {
            return Ok(false);
        }
        let snapshot = Snapshot::capture(ui);
        snapshot.validate()?;
        let text = serde_json::to_string_pretty(&snapshot).map_err(|e| format!("serialize workspaces: {e}"))?;
        if text.len() > MAX_BYTES {
            return Err("workspace file exceeds 1 MiB".into());
        }
        config.write(FILE, &text).map_err(|e| format!("save workspaces: {e}"))?;
        self.saved = snapshot;
        self.error = None;
        Ok(true)
    }

    /// An explicit save (Save Workspace, Save as New Workspace, Edit Workspaces): unlike
    /// [`Self::save`], no settings folder or a file blocked at load is an error, not "nothing to do".
    pub(crate) fn save_now(&mut self, config: Option<&dyn ConfigStore>, ui: &UiState) -> Result<(), String> {
        let Some(config) = config else {
            return Err("there is no settings folder to save them in".into());
        };
        if !self.writable {
            let path = config.dir().map_or_else(|| FILE.to_string(), |dir| dir.join(FILE).display().to_string());
            return Err(format!("{path} couldn't be loaded at startup, so it is left untouched; fix or move it and restart"));
        }
        self.save(Some(config), ui).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dock::{Floating, Layout, Node, StackEntry, WorkspaceExt as _};
    use craft_ui::docking::Action;
    use craft_ui::layout::{SplitAxis, SplitSize};
    use effectcraft_engine::config::MemoryConfig;

    fn minimal() -> Workspace {
        Workspace::new(crate::dock::workspace("Minimal"))
    }

    fn tabs(panels: &[PanelKind]) -> Node<PanelKind> {
        Node::Tabs { panels: panels.to_vec(), active: 0 }
    }

    #[test]
    fn corrupt_future_or_earlier_workspace_keeps_defaults_and_original_file() {
        let valid = Snapshot::capture(&UiState::default());
        // An earlier version's file (EffectCraft's own dock tree) waits for its migration.
        let earlier = r#"{"version":1,"workspace":"Default","dock":{"Tabs":{"panels":["Composition"],"active":0}},"floating":[],"focused":"Composition","maximized":null,"saved_workspaces":{},"saved_floating":{}}"#;
        let legacy = r#"{"workspaces":{"Mine":{"Tabs":{"panels":["Composition"],"active":0}}}}"#;
        for text in
            ["{".into(), serde_json::to_string(&Snapshot { version: VERSION + 1, ..valid }).unwrap(), " ".repeat(MAX_BYTES + 1), earlier.into(), legacy.into()]
        {
            let config = MemoryConfig::default();
            config.write(FILE, &text).unwrap();
            let mut ui = UiState::default();
            let mut store = WorkspaceStore::load(Some(&config), &mut ui);
            assert!(store.error.is_some());
            assert_eq!(ui.workspace, "Default");
            assert_eq!(ui.dock, UiState::default().dock);
            ui.focused = PanelKind::Project;
            ui.dock = minimal();
            assert!(!store.save(Some(&config), &ui).unwrap());
            assert!(store.save_now(Some(&config), &ui).unwrap_err().contains("couldn't be loaded"));
            assert_eq!(config.read(FILE).as_deref(), Some(text.as_str()));
        }
    }

    #[test]
    fn explicit_save_without_a_settings_folder_is_an_error() {
        let mut ui = UiState::default();
        let mut store = WorkspaceStore::load(None, &mut ui);
        ui.focused = PanelKind::Project;
        assert!(!store.save(None, &ui).unwrap());
        assert!(store.save_now(None, &ui).is_err());
    }

    #[test]
    fn explicit_save_with_nothing_new_succeeds() {
        let config = MemoryConfig::default();
        let mut ui = UiState::default();
        let mut store = WorkspaceStore::load(Some(&config), &mut ui);
        ui.focused = PanelKind::Project;
        store.save_now(Some(&config), &ui).unwrap();
        store.save_now(Some(&config), &ui).unwrap();
        assert!(config.read(FILE).is_some());
    }

    #[test]
    fn invalid_tabs_split_and_deep_tree_are_rejected() {
        let mut snapshot = Snapshot::capture(&UiState::default());
        snapshot.dock = Workspace::new(Layout { root: Some(Node::Tabs { panels: vec![PanelKind::Project], active: usize::MAX }), floating: vec![] });
        assert!(snapshot.validate().is_err());
        let split = |size, first, second| Node::Split { axis: SplitAxis::Horizontal, size, first: Box::new(first), second: Box::new(second) };
        snapshot.dock = Workspace::new(Layout {
            root: Some(split(SplitSize::FixedFirst(f32::NAN), tabs(&[PanelKind::Project]), tabs(&[PanelKind::Timeline]))),
            floating: vec![],
        });
        assert!(snapshot.validate().is_err());
        let mut root = tabs(&[PanelKind::Composition]);
        for _ in 0..=craft_ui::docking::MAX_DEPTH {
            root = split(SplitSize::Ratio(0.5), root, tabs(&[]));
        }
        snapshot.dock = Workspace::new(Layout { root: Some(root), floating: vec![] });
        assert!(snapshot.validate().is_err());
        // A panel in two places.
        snapshot.dock = Workspace::new(Layout {
            root: Some(tabs(&[PanelKind::Project])),
            floating: vec![Floating::single(vec![PanelKind::Project], 0, [0.0, 0.0, 200.0, 100.0])],
        });
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn closing_all_docked_panels_remains_a_persistable_layout() {
        let config = MemoryConfig::default();
        let mut ui = UiState::default();
        let mut store = WorkspaceStore::load(Some(&config), &mut ui);
        for panel in ui.dock.panels() {
            ui.dock.apply(Action::Close { panel }, crate::dock::permissions).unwrap();
        }
        assert!(ui.dock.layout.root.is_none());
        assert!(store.save(Some(&config), &ui).unwrap());
        let mut restored = UiState::default();
        let loaded = WorkspaceStore::load(Some(&config), &mut restored);
        assert!(loaded.error.is_none());
        assert_eq!(restored.dock, ui.dock);
    }

    #[test]
    fn floating_groups_and_named_snapshots_roundtrip_without_document_state() {
        let config = MemoryConfig::default();
        let mut ui = UiState::default();
        let mut store = WorkspaceStore::load(Some(&config), &mut ui);
        ui.workspace = "Floating".into();
        ui.dock.apply(Action::Float { panel: PanelKind::Preview, rect: [50.0, 80.0, 200.0, 160.0] }, crate::dock::permissions).unwrap();
        ui.dock.apply(Action::Close { panel: PanelKind::Align }, crate::dock::permissions).unwrap();
        ui.saved_workspaces.insert(ui.workspace.clone(), ui.dock.clone());
        ui.maximized = Some(PanelKind::Composition);
        ui.project_search = "do not persist".into();
        assert!(store.save(Some(&config), &ui).unwrap());
        let mut restored = UiState::default();
        WorkspaceStore::load(Some(&config), &mut restored);
        assert_eq!(Snapshot::capture(&restored), Snapshot::capture(&ui));
        // The closed panel's place comes back with it.
        assert!(restored.dock.hidden.iter().any(|(p, _)| *p == PanelKind::Align));
        assert!(restored.project_search.is_empty());
    }

    #[test]
    fn restored_viewer_slots_do_not_reuse_ids_or_restore_document_links() {
        let config = MemoryConfig::default();
        let mut ui = UiState::default();
        let mut store = WorkspaceStore::load(Some(&config), &mut ui);
        let two = |viewer| Workspace::new(Layout { root: Some(tabs(&[PanelKind::Composition, PanelKind::Viewer(viewer)])), floating: vec![] });
        ui.dock = two(2);
        ui.saved_workspaces.insert("Two views".into(), two(5));
        ui.viewers.insert(2, Some(42));
        assert!(store.save(Some(&config), &ui).unwrap());
        let mut app = crate::EffectcraftApp::new(effectcraft_engine::Session::default());
        WorkspaceStore::load(Some(&config), &mut app.ui);
        assert_eq!(app.ui.viewers.get(&2), Some(&None));
        assert_eq!(app.ui.viewers.get(&5), Some(&None));
        assert_eq!(crate::panels::viewers::new_viewer(&mut app), 6);
        assert!(app.ui.dock.is_docked(PanelKind::Viewer(2)));
    }

    #[test]
    fn invalid_viewer_ids_are_rejected_in_every_workspace_reference() {
        for id in [0, MAX_VIEWER_ID + 1, u32::MAX] {
            for location in 0..8 {
                let mut snapshot = Snapshot::capture(&UiState::default());
                let panel = PanelKind::Viewer(id);
                let docked = Workspace::new(Layout { root: Some(tabs(&[panel])), floating: vec![] });
                let stacked =
                    Workspace::new(Layout { root: Some(Node::Stack { entries: vec![StackEntry { panel, open: true, height: None }] }), floating: vec![] });
                let floating = Workspace::new(Layout {
                    root: Some(tabs(&[PanelKind::Composition])),
                    floating: vec![Floating::single(vec![panel], 0, [0.0, 0.0, 200.0, 100.0])],
                });
                let mut hidden = Workspace::new(Layout { root: Some(tabs(&[PanelKind::Composition, panel])), floating: vec![] });
                hidden.apply(Action::Close { panel }, crate::dock::permissions).unwrap();
                match location {
                    0 => snapshot.focused = panel,
                    1 => snapshot.maximized = Some(panel),
                    2 => snapshot.dock = docked,
                    3 => snapshot.dock = stacked,
                    4 => snapshot.dock = floating,
                    5 => snapshot.dock = hidden,
                    6 => {
                        snapshot.saved_workspaces.insert("Saved".into(), stacked);
                    }
                    _ => {
                        snapshot.saved_workspaces.insert("Saved".into(), floating);
                    }
                }
                assert!(snapshot.validate().is_err(), "viewer {id} at location {location} must be rejected");
            }
        }
    }

    #[test]
    fn hostile_native_window_places_are_made_safe_on_load() {
        let config = MemoryConfig::default();
        let mut ui = UiState::default();
        let mut store = WorkspaceStore::load(Some(&config), &mut ui);
        ui.dock.apply(Action::Float { panel: PanelKind::Project, rect: [10.0, 10.0, 300.0, 200.0] }, crate::dock::permissions).unwrap();
        ui.dock.apply(Action::SetNative { panel: PanelKind::Project, rect: Some([90_000.0, -90_000.0, 1.0e5, 0.5]) }, crate::dock::permissions).unwrap();
        assert!(store.save(Some(&config), &ui).unwrap());
        let mut restored = UiState::default();
        WorkspaceStore::load(Some(&config), &mut restored);
        let native = restored.dock.layout.floating.iter().find(|w| w.contains(&PanelKind::Project)).and_then(|w| w.native).unwrap();
        assert_eq!(native, crate::dock_ui::sanitized_window_rect(native));
        assert!(native[2] <= crate::dock_ui::PANEL_WINDOW_MAX && native[3] >= crate::dock_ui::PANEL_WINDOW_MIN[1]);
    }

    #[test]
    fn failed_write_keeps_previous_snapshot_and_retries() {
        struct Fails<'a>(&'a MemoryConfig);
        impl ConfigStore for Fails<'_> {
            fn read(&self, name: &str) -> Option<String> {
                self.0.read(name)
            }
            fn write(&self, _name: &str, _data: &str) -> std::io::Result<()> {
                Err(std::io::Error::other("synthetic full disk"))
            }
            fn remove(&self, _name: &str) -> std::io::Result<()> {
                Err(std::io::Error::other("unused"))
            }
        }
        let config = MemoryConfig::default();
        let mut ui = UiState::default();
        let mut store = WorkspaceStore::load(Some(&config), &mut ui);
        ui.focused = PanelKind::Project;
        assert!(store.save(Some(&config), &ui).unwrap());
        let previous = config.read(FILE).unwrap();
        ui.focused = PanelKind::Timeline;
        assert!(store.save(Some(&Fails(&config)), &ui).unwrap_err().contains("synthetic full disk"));
        assert_eq!(config.read(FILE).as_deref(), Some(previous.as_str()));
        assert!(store.has_changes(&ui));
        assert!(store.save(Some(&config), &ui).unwrap());
        let mut restored = UiState::default();
        WorkspaceStore::load(Some(&config), &mut restored);
        assert_eq!(restored.focused, PanelKind::Timeline);
    }
}
