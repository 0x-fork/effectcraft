//! New 3D Primitive dialog (Layer ▸ New ▸ 3D Primitive…): the primitive's type, name and
//! dimensions. OK runs `layer.newPrimitive`, which switches a Classic 3D comp to Advanced 3D;
//! the layer's Geometry Options edit it afterwards.

use std::collections::BTreeMap;

use crate::i18n::tr;
use effectcraft_engine::project::build::{Ids, primitive_geometry_options};
use effectcraft_engine::project::{ParamUi, PrimitiveKind, Property};
use egui::{Color32, vec2};
use serde_json::{Value, json};

use crate::theme::Tokens;
use crate::{Dialog, EffectcraftApp};

#[derive(Clone, Debug, Default)]
pub struct PrimitiveDraft {
    pub kind: PrimitiveKind,
    pub name: String,
    /// Whether the name was typed; until then it follows the type.
    pub named: bool,
    /// Dimensions by Geometry Options match id, kept when the type changes (a sphere's radius
    /// carries over to a cone).
    pub dims: BTreeMap<String, f64>,
}

/// The dimensions a primitive type has: its Geometry Options as the engine builds them.
fn dimensions(kind: PrimitiveKind) -> Vec<Property> {
    let mut next = 0;
    primitive_geometry_options(&mut Ids(&mut next), kind).props().cloned().collect()
}

/// Open the dialog for a new primitive in the active comp.
pub fn open(app: &mut EffectcraftApp) -> Result<(), String> {
    app.session.active_comp().ok_or("no composition is open")?;
    let kind = PrimitiveKind::Cube;
    app.dialog_state.primitive = PrimitiveDraft { kind, name: kind.label().to_string(), named: false, dims: BTreeMap::new() };
    app.dialog = Some(Dialog::NewPrimitive);
    Ok(())
}

pub fn dialog(app: &mut EffectcraftApp, ctx: &egui::Context, t: &Tokens) {
    let mut d = app.dialog_state.primitive.clone();
    let (mut ok, mut close) = (false, false);
    super::dialogs::modal(ctx, "New 3D Primitive", vec2(420.0, 300.0), t, |ui| {
        egui::Grid::new("primitive-grid").num_columns(2).spacing([14.0, 9.0]).show(ui, |ui| {
            ui.label(tr("Type"));
            let combo = egui::ComboBox::from_id_salt("primitive-type").selected_text(tr(d.kind.label())).show_ui(ui, |ui| {
                for (i, k) in PrimitiveKind::ALL.into_iter().enumerate() {
                    let item = ui.selectable_value(&mut d.kind, k, tr(k.label()));
                    app.auto.add(&format!("dialog.primitive.typeItem.{i}"), item.rect, k.label());
                }
            });
            app.auto.add_response("dialog.primitive.type", &combo.response, "Type");
            if !d.named {
                d.name = d.kind.label().to_string();
            }
            ui.end_row();
            ui.label(tr("Name"));
            let r = ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(220.0));
            d.named |= r.changed();
            app.auto.add_response("dialog.primitive.name", &r, "Name");
            ui.end_row();
            for p in dimensions(d.kind) {
                let ParamUi::Slider { min, max, decimals, .. } = p.ui else { continue };
                let v = d.dims.entry(p.match_id.clone()).or_insert(p.value.as_f64());
                ui.label(tr(&p.name));
                // Counts (segments, rings) are whole numbers; sizes are pixels.
                let suffix = if decimals == 0 { "" } else { " px" };
                let r = ui.add(crate::widgets::drag_value(egui::DragValue::new(v).range(min..=max).max_decimals(decimals as usize).suffix(suffix)));
                app.auto.add_response(&format!("dialog.primitive.{}", p.match_id), &r, &p.name);
                ui.end_row();
            }
        });
        ui.add_space(14.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let b = ui.add(egui::Button::new(egui::RichText::new(tr("   OK   ")).color(Color32::WHITE)).fill(t.accent));
            app.auto.add("dialog.primitive.ok", b.rect, "OK");
            ok |= b.clicked();
            let c = ui.button(tr("Cancel"));
            app.auto.add("dialog.primitive.cancel", c.rect, "Cancel");
            close |= c.clicked();
        });
    });
    app.dialog_state.primitive = d.clone();
    if ok || super::dialogs::ui_enter(ctx) {
        let mut p = json!({"kind": d.kind.label().to_ascii_lowercase(), "name": d.name});
        for dim in dimensions(d.kind) {
            if let Some(v) = d.dims.get(&dim.match_id) {
                p[dim.match_id.as_str()] = json!(v);
            }
        }
        if let Err(e) = app.session.execute("layer.newPrimitive", p) {
            app.ui.status = e.to_string();
        }
        close = true;
    }
    if close {
        app.dialog = None;
    }
}

/// Menu routing: New 3D Primitive without parameters opens the dialog. Returns true when it did.
pub fn route(app: &mut EffectcraftApp, id: &str, params: &Value) -> Result<bool, String> {
    if id != "layer.newPrimitive" || !params.as_object().is_none_or(|m| m.is_empty()) {
        return Ok(false);
    }
    open(app).map(|_| true)
}
