//! `effect.pickCryptomatte` / `effect.cryptomatteAt` (#473): the Cryptomatte eyedropper reads the
//! aux channels the effect renders from (footage at its source time, a 3D precomp's generated
//! pass) and edits the Selection text in one undo step.

use std::sync::Arc;

use effectcraft_project::{Footage, FootageKind, ItemId};
use effectcraft_raster::channels3d::{crypto_float, crypto_hash};
use effectcraft_raster::{AuxChannels, Image};
use effectcraft_time::{FrameRate, Tick};
use serde_json::{Value, json};

use crate::Session;
use crate::render::FootageSource;

const COMMA: &str = "Green, \"Glossy\"";

/// 8 × 4 footage with a 16 × 8 Cryptomatte (two aux pixels per layer pixel). Layer pixels in x:
/// 0-1 `left` (0.6) over `right` (0.4); 2-3 `left` (0.3) under the comma name (0.7), ranks out of
/// order; 4-5 `star*`; 6 an ID missing from the manifest; 7 a 0.5 / 0.5 tie of `left` and
/// `right`. Rows 3+ are empty. From 1 s of source time `late` replaces columns 0-1.
struct Crypto;

fn aux_at(t: Tick) -> AuxChannels {
    let (w, h) = (16usize, 8usize);
    let mut planes = vec![vec![0.0f32; w * h]; 4];
    for (i, x) in (0..w * h).map(|i| (i, i % w)).filter(|(i, _)| i / w < 6) {
        let late = if t >= Tick::from_seconds_f64(1.0) { "late" } else { "left" };
        let (a, ca, b, cb) = match x {
            0..4 => (late, 0.6, "right", 0.4),
            4..8 => ("left", 0.3, COMMA, 0.7),
            8..12 => ("star*", 1.0, "", 0.0),
            12..14 => ("ghost", 1.0, "", 0.0),
            _ => ("left", 0.5, "right", 0.5),
        };
        for (k, v) in [crypto_float(crypto_hash(a)), ca, crypto_float(crypto_hash(b)), cb].into_iter().enumerate() {
            planes[k][i] = v;
        }
    }
    let mut aux = AuxChannels::new(w as u32, h as u32, 1.0);
    for (k, c) in ["R", "G", "B", "A"].iter().enumerate() {
        aux.channels.push((format!("CryptoObject00.{c}"), planes[k].clone()));
    }
    aux.manifests = vec![("CryptoObject".into(), ["left", "right", "late", COMMA, "star*"].iter().map(|n| (n.to_string(), crypto_hash(n))).collect())];
    aux
}

impl FootageSource for Crypto {
    fn frame(&self, _: ItemId, _: &Footage, _: Tick) -> Option<Arc<Image>> {
        Some(Arc::new(Image::new(8, 4)))
    }
    fn aux(&self, _: ItemId, _: &Footage, t: Tick) -> Option<Arc<AuxChannels>> {
        Some(Arc::new(aux_at(t)))
    }
}

struct Probe;
impl crate::Importer for Probe {
    fn probe(&self, path: &str) -> Result<Footage, String> {
        let duration = Tick::from_seconds_f64(2.0);
        Ok(Footage {
            path: path.into(),
            kind: FootageKind::Video,
            width: 8,
            height: 4,
            has_video: true,
            frame_rate: FrameRate::from_f64(24.0),
            duration,
            ..Default::default()
        })
    }
}

fn session() -> Session {
    let mut s = Session { footage: Arc::new(Crypto), importer: Some(Arc::new(Probe)), ..Default::default() };
    let item = s.execute("file.import", json!({"paths": ["/renders/crypto.exr"]})).unwrap()["items"][0].clone();
    s.execute("comp.new", json!({"name": "C", "width": 8, "height": 4, "frameRate": 24, "duration": 2})).unwrap();
    s.execute("layer.addItem", json!({"item": item})).unwrap();
    s.execute_checked("effect.apply", json!({"effect": "Cryptomatte"})).unwrap();
    s
}

fn pick(s: &mut Session, x: f64, mode: &str) -> Value {
    s.execute_checked("effect.pickCryptomatte", json!({"effect": 1, "x": x, "y": 1.5, "mode": mode})).unwrap()
}

fn selection(s: &Session) -> String {
    let l = &s.active_comp().unwrap().layers[0];
    let g = l.effects().unwrap().groups().next().unwrap();
    match &g.prop("selection").unwrap().value {
        effectcraft_keyframe::Value::Str(t) => t.clone(),
        v => panic!("{v:?}"),
    }
}

#[test]
fn picks_replace_add_and_remove_in_one_undo_step_each() {
    let mut s = session();
    let steps = s.history.undo.len();
    let r = pick(&mut s, 0.5, "replace");
    assert_eq!((r["name"].as_str(), r["selection"].as_str(), r["changed"].as_bool()), (Some("left"), Some("left"), Some(true)), "{r}");
    assert_eq!(r["hash"], format!("{:08x}", crypto_hash("left")));
    assert_eq!(s.history.undo.len(), steps + 1);
    // The highest coverage wins even when it is not rank 0; the name is quoted.
    assert_eq!(pick(&mut s, 2.5, "add")["selection"], r#"left, "Green, \"Glossy\"""#);
    // Adding it again changes nothing and leaves no undo step.
    let again = pick(&mut s, 3.5, "add");
    assert_eq!((again["changed"].as_bool(), s.history.undo.len()), (Some(false), steps + 2));
    // A `*` name and an unnamed ID go in as hashes; a tie goes to rank 0.
    pick(&mut s, 4.5, "add");
    assert_eq!(pick(&mut s, 6.5, "add")["selection"], format!(r#"left, "Green, \"Glossy\"", <{:08x}>, <{:08x}>"#, crypto_hash("star*"), crypto_hash("ghost")));
    assert_eq!(s.execute("effect.cryptomatteAt", json!({"effect": 1, "x": 7.5, "y": 0.5})).unwrap()["name"], "left");
    let r = pick(&mut s, 0.0, "remove");
    assert_eq!(r["selection"], format!(r#""Green, \"Glossy\"", <{:08x}>, <{:08x}>"#, crypto_hash("star*"), crypto_hash("ghost")));
    assert_eq!(r["selected"], false);
    // The text parses back to exactly the picked IDs.
    let manifest = aux_at(Tick::ZERO).manifests[0].1.clone();
    let got = effectcraft_effects::selection_hashes(&selection(&s), Some(&manifest));
    assert_eq!(got, [COMMA, "star*", "ghost"].map(crypto_hash));
    assert_eq!(s.history.undo.len(), steps + 5);
    s.execute("edit.undo", json!({})).unwrap();
    assert!(selection(&s).starts_with("left, "), "{}", selection(&s));
    pick(&mut s, 2.0, "replace");
    assert_eq!(selection(&s), r#""Green, \"Glossy\"""#);
    // The same selection with other spacing: no change, no undo step.
    s.execute_checked("prop.set", json!({"path": "effects/#1/selection", "value": " \"Green, \\\"Glossy\\\"\" "})).unwrap();
    let n = s.history.undo.len();
    assert_eq!((pick(&mut s, 2.0, "replace")["changed"].as_bool(), s.history.undo.len()), (Some(false), n));
}

#[test]
fn bad_points_layers_and_modes_are_errors_and_change_nothing() {
    let mut s = session();
    let at = |s: &mut Session, x: f64, y: f64| s.execute_checked("effect.pickCryptomatte", json!({"effect": 1, "x": x, "y": y}));
    for (x, y) in [(-0.1, 1.0), (8.0, 1.0), (1.0, 4.0), (1.0, 3.5), (1e300, -1e300)] {
        assert!(at(&mut s, x, y).is_err(), "{x},{y}");
        assert_eq!(s.execute("effect.cryptomatteAt", json!({"effect": 1, "x": x, "y": y})).unwrap(), Value::Null);
    }
    assert!(s.execute_checked("effect.pickCryptomatte", json!({"effect": 1, "x": 1, "y": 1, "mode": "toggle"})).is_err());
    assert!(s.execute_checked("effect.pickCryptomatte", json!({"effect": 1, "x": 1})).is_err());
    for scale in [0.0, -1.0, 2.0] {
        assert!(s.execute_checked("effect.pickCryptomatte", json!({"effect": 1, "x": 1, "y": 1, "scale": scale})).is_err(), "{scale}");
    }
    s.execute_checked("effect.apply", json!({"effect": "Gaussian Blur"})).unwrap();
    assert!(s.execute_checked("effect.pickCryptomatte", json!({"effect": 2, "x": 1, "y": 1})).is_err(), "not a Cryptomatte effect");
    assert_eq!(selection(&s), "");
    // A layer without aux channels.
    s.execute("layer.newSolid", json!({"name": "S"})).unwrap();
    s.execute_checked("effect.apply", json!({"effect": "Cryptomatte"})).unwrap();
    let e = s.execute_checked("effect.pickCryptomatte", json!({"effect": 1, "x": 1, "y": 1})).unwrap_err().to_string();
    assert!(e.contains("no Cryptomatte channels"), "{e}");
    // An expression-driven Selection would ignore the edit.
    let mut s = session();
    s.execute_checked("prop.setExpression", json!({"path": "effects/#1/selection", "expression": "\"right\""})).unwrap();
    assert!(s.execute_checked("effect.pickCryptomatte", json!({"effect": 1, "x": 1, "y": 1})).is_err());
    assert_eq!(s.execute("effect.cryptomatteAt", json!({"effect": 1, "x": 1, "y": 1})).unwrap()["selected"], false);
}

/// The pick reads the frame the render shows: Time Remap's source time, not the layer time.
#[test]
fn time_remap_picks_the_shown_source_frame() {
    let mut s = session();
    assert_eq!(pick(&mut s, 0.5, "replace")["name"], "left");
    s.execute_checked("layer.enableTimeRemap", json!({})).unwrap();
    s.execute_checked("prop.set", json!({"path": "timeRemap", "value": 1.5})).unwrap();
    assert_eq!(pick(&mut s, 0.5, "replace")["name"], "late");
}

/// A precomp of a 3D comp: the generated CryptoObject pass names its layers.
#[test]
fn picks_on_a_precomp_of_a_3d_comp() {
    let mut s = Session::default();
    s.execute("comp.new", json!({"name": "Outer", "width": 40, "height": 20, "frameRate": 24, "duration": 1})).unwrap();
    let l = s.execute("layer.newSolid", json!({"name": "Red, Ball", "width": 40, "height": 20})).unwrap()["layer"].clone();
    s.execute("layer.setSwitch", json!({"layers": [l], "switch": "threeD", "value": true})).unwrap();
    s.execute("layer.precompose", json!({"layers": [l], "name": "Inner"})).unwrap();
    s.execute("layer.select", json!({"layers": [s.active_comp().unwrap().layers[0].id.0]})).ok();
    s.execute_checked("effect.apply", json!({"effect": "Cryptomatte"})).unwrap();
    let r = s.execute_checked("effect.pickCryptomatte", json!({"effect": 1, "x": 20, "y": 10})).unwrap();
    assert_eq!((r["name"].as_str(), r["selection"].as_str()), (Some("Red, Ball"), Some("\"Red, Ball\"")), "{r}");
}
