//! Multiple monitors (#486): Move Panel to New Window records a floating panel's window as its
//! own native window, which can be placed on another screen, and back.
// D2: craft-ui's `native-windows` feature draws these windows; until then they are drawn in the
// main window and only their place is recorded and saved.
use effectcraft_engine::Session;
use effectcraft_ui_egui::EffectcraftApp;
use effectcraft_ui_egui::dock::{PanelKind, WorkspaceExt as _};
use effectcraft_ui_egui::dock_ui::sanitized_window_rect;
use serde_json::json;

fn invoke(app: &mut EffectcraftApp, ctx: &egui::Context, params: serde_json::Value) -> Result<serde_json::Value, String> {
    effectcraft_ui_egui::menus::invoke(app, ctx, "window.panelWindow", params)
}

fn native(app: &EffectcraftApp, panel: PanelKind) -> Option<[f32; 4]> {
    app.ui.dock.layout.floating.iter().find(|w| w.contains(&panel)).and_then(|w| w.native)
}

#[test]
fn panel_window_command_moves_a_docked_panel_into_its_own_window_and_back() {
    let mut app = EffectcraftApp::new(Session::default());
    let ctx = egui::Context::default();
    assert!(app.ui.dock.is_docked(PanelKind::Project));
    assert_eq!(invoke(&mut app, &ctx, json!({"panel": "Project", "window": true})).unwrap(), json!({"window": true}));
    // Undocked first, then given a window of its own.
    assert!(!app.ui.dock.is_docked(PanelKind::Project));
    assert!(app.ui.dock.layout.floating.iter().any(|w| w.panels().copied().collect::<Vec<_>>() == [PanelKind::Project]));
    assert!(app.in_panel_window(PanelKind::Project));
    let rect = native(&app, PanelKind::Project).unwrap();
    assert_eq!(rect, sanitized_window_rect(rect));
    // Asking again changes nothing; no `window` toggles.
    assert_eq!(invoke(&mut app, &ctx, json!({"panel": "Project", "window": true})).unwrap(), json!({"window": true}));
    assert_eq!(invoke(&mut app, &ctx, json!({"panel": "Project"})).unwrap(), json!({"window": false}));
    // Back in the main window, still floating where it was.
    assert!(!app.in_panel_window(PanelKind::Project));
    assert!(app.ui.dock.is_floating(PanelKind::Project));
    // The serialized panel form works too.
    assert_eq!(invoke(&mut app, &ctx, json!({"panel": PanelKind::Project})).unwrap(), json!({"window": true}));
    // Bad requests are errors and change nothing.
    let before = app.ui.dock.clone();
    for params in [json!({}), json!({"panel": "Nope"}), json!({"panel": 7}), json!({"panel": "Project", "window": "yes"}), json!({"panel": {"Viewer": 9}})] {
        assert!(invoke(&mut app, &ctx, params.clone()).is_err(), "{params} must be rejected");
    }
    assert_eq!(app.ui.dock, before);
    // Docking it back into the main window ends its window.
    effectcraft_ui_egui::menus::invoke(&mut app, &ctx, "window.dockPanel", json!({"panel": "Project", "anchor": "Timeline"})).unwrap();
    assert!(!app.in_panel_window(PanelKind::Project));
    assert!(app.ui.dock.is_docked(PanelKind::Project));
}

#[test]
fn panel_windows_survive_restart_with_hostile_places_made_safe() {
    use effectcraft_engine::config::MemoryConfig;
    use std::sync::Arc;
    let store = Arc::new(MemoryConfig::default());
    let mut app = EffectcraftApp::new(Session { config: Some(store.clone()), ..Default::default() });
    let ctx = egui::Context::default();
    invoke(&mut app, &ctx, json!({"panel": "Project", "window": true})).unwrap();
    let hostile = [90_000.0, -99_000.0, 99_000.0, 0.5];
    app.apply_dock(craft_ui::docking::Action::SetNative { panel: PanelKind::Project, rect: Some(hostile) }).unwrap();
    eframe::App::on_exit(&mut app);
    let restarted = EffectcraftApp::new(Session { config: Some(store), ..Default::default() });
    assert_eq!(native(&restarted, PanelKind::Project), Some(sanitized_window_rect(hostile)));
    let [x, y, w, h] = sanitized_window_rect(hostile);
    assert!(x <= 100_000.0 && y >= -100_000.0 && (160.0..=16_384.0).contains(&w) && (100.0..=16_384.0).contains(&h));
    assert!(sanitized_window_rect([f32::NAN, f32::INFINITY, f32::NAN, f32::NEG_INFINITY]).iter().all(|v| v.is_finite()));
}
