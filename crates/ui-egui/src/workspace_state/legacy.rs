//! Workspace files of earlier versions: EffectCraft's own dock tree (craft-libs 5c874d51 and
//! before), read into private copies of its types and turned into craft-ui workspaces.
//!
//! - Version 1 (`{version: 1, workspace, dock, floating, …, panel_windows}`) keeps the current
//!   arrangement, the named workspaces, focus and the maximized panel.
//! - The unversioned record (`{workspaces, floating}`) holds only named workspaces; the
//!   current arrangement stays the default.
//! - A stacked frame with several tabs becomes one stacked panel per tab; the shown tab keeps
//!   the frame's open state and height, the others are collapsed.
//! - A floating group in its own native window (`panel_windows`) keeps that place as the
//!   window's native rect.
//! - Closed panels had no recorded place: they stay closed and reopen at their default place.

use std::collections::BTreeMap;

use craft_ui::docking::{Floating, Node, StackEntry};
use craft_ui::layout::{SplitAxis, SplitSize};
use serde::Deserialize;

use super::{Snapshot, VERSION};
use crate::dock::{Layout, PanelKind, Workspace};
use crate::state::UiState;

#[derive(Deserialize)]
enum OldSplitSize {
    Ratio(f32),
    FixedA(f32),
    FixedB(f32),
}

/// A stacked frame; files from before stacked frames held tabs name one `panel`.
#[derive(Deserialize)]
struct OldStackEntry {
    #[serde(default)]
    panel: Option<PanelKind>,
    #[serde(default)]
    panels: Vec<PanelKind>,
    #[serde(default)]
    active: usize,
    open: bool,
    #[serde(default)]
    height: Option<f32>,
}

#[derive(Deserialize)]
enum OldNode {
    Split { vertical: bool, size: OldSplitSize, a: Box<OldNode>, b: Box<OldNode> },
    Tabs { panels: Vec<PanelKind>, active: usize },
    Stack { entries: Vec<OldStackEntry> },
}

#[derive(Deserialize)]
struct OldFloating {
    panels: Vec<PanelKind>,
    active: usize,
    rect: [f32; 4],
}

/// The floating group holding `panel` shows in its own native window at `rect`.
#[derive(Deserialize)]
struct OldPanelWindow {
    panel: PanelKind,
    rect: [f32; 4],
}

#[derive(Deserialize)]
struct OldSnapshot {
    workspace: String,
    dock: OldNode,
    floating: Vec<OldFloating>,
    focused: PanelKind,
    maximized: Option<PanelKind>,
    saved_workspaces: BTreeMap<String, OldNode>,
    saved_floating: BTreeMap<String, Vec<OldFloating>>,
    #[serde(default)]
    panel_windows: Vec<OldPanelWindow>,
}

#[derive(Deserialize)]
struct OldRecord {
    #[serde(default)]
    workspaces: BTreeMap<String, OldNode>,
    #[serde(default)]
    floating: BTreeMap<String, Vec<OldFloating>>,
}

/// Whether `value` is a file of an earlier version that [`migrate`] reads.
pub(super) fn is_legacy(value: &serde_json::Value) -> bool {
    match value.get("version") {
        Some(version) => version.as_u64() == Some(1),
        None => value.as_object().is_some_and(|fields| fields.contains_key("workspaces") || fields.contains_key("floating")),
    }
}

/// The snapshot an earlier version's file describes. `ui` supplies what an unversioned record
/// leaves out (the current arrangement). The caller validates the result.
pub(super) fn migrate(value: serde_json::Value, ui: &UiState) -> Result<Snapshot, String> {
    if value.get("version").is_none() {
        let record: OldRecord = serde_json::from_value(value).map_err(|e| e.to_string())?;
        let mut snapshot = Snapshot::capture(ui);
        snapshot.saved_workspaces = named(record.workspaces, record.floating)?;
        return Ok(snapshot);
    }
    let old: OldSnapshot = serde_json::from_value(value).map_err(|e| e.to_string())?;
    Ok(Snapshot {
        version: VERSION,
        workspace: old.workspace,
        dock: workspace(old.dock, old.floating, &old.panel_windows)?,
        focused: old.focused,
        maximized: old.maximized,
        saved_workspaces: named(old.saved_workspaces, old.saved_floating)?,
    })
}

/// Named workspaces with their floating groups; floating groups of no named workspace are
/// dropped, as the earlier version never showed them.
fn named(roots: BTreeMap<String, OldNode>, mut floating: BTreeMap<String, Vec<OldFloating>>) -> Result<BTreeMap<String, Workspace>, String> {
    roots
        .into_iter()
        .map(|(name, root)| {
            let groups = floating.remove(&name).unwrap_or_default();
            workspace(root, groups, &[]).map(|workspace| (name, workspace))
        })
        .collect()
}

fn workspace(root: OldNode, floating: Vec<OldFloating>, windows: &[OldPanelWindow]) -> Result<Workspace, String> {
    // Closing the last docked panel left an empty tab group as the root.
    let root = match root {
        OldNode::Tabs { panels, .. } if panels.is_empty() => None,
        root => Some(node(root, 0)?),
    };
    let floating = floating
        .into_iter()
        .map(|group| {
            let native = windows.iter().find(|window| group.panels.contains(&window.panel)).map(|window| window.rect);
            let mut window = Floating::single(group.panels, group.active, group.rect);
            window.native = native.map(crate::dock_ui::sanitized_window_rect);
            window
        })
        .collect();
    Ok(Workspace::new(Layout { root, floating }))
}

fn node(old: OldNode, depth: usize) -> Result<Node<PanelKind>, String> {
    if depth > craft_ui::docking::MAX_DEPTH {
        return Err("workspace dock tree exceeds supported limits".into());
    }
    Ok(match old {
        OldNode::Tabs { panels, active } => Node::Tabs { panels, active },
        OldNode::Split { vertical, size, a, b } => Node::Split {
            axis: if vertical { SplitAxis::Vertical } else { SplitAxis::Horizontal },
            size: match size {
                OldSplitSize::Ratio(value) => SplitSize::Ratio(value),
                OldSplitSize::FixedA(value) => SplitSize::FixedFirst(value),
                OldSplitSize::FixedB(value) => SplitSize::FixedSecond(value),
            },
            first: Box::new(node(*a, depth.saturating_add(1))?),
            second: Box::new(node(*b, depth.saturating_add(1))?),
        },
        OldNode::Stack { entries } => {
            let mut out = Vec::new();
            for entry in entries {
                let mut panels = entry.panels;
                if let Some(panel) = entry.panel
                    && !panels.contains(&panel)
                {
                    panels.insert(0, panel);
                }
                let shown = panels.get(entry.active).or(panels.first()).copied();
                for panel in panels {
                    let front = Some(panel) == shown;
                    out.push(StackEntry { panel, open: front && entry.open, height: entry.height.filter(|_| front) });
                }
            }
            Node::Stack { entries: out }
        }
    })
}
