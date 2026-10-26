use super::doc_state::{CompSelection, EditorState};
use super::error::{EngineError, Result};
use super::event::Event;
use super::*;

/// `t` as a current time of `comp`: a frame within its duration.
fn comp_frame_at(comp: &Comp, t: Tick) -> Tick {
    comp.frame_rate.snap_nearest(t.min(comp.duration - comp.frame_duration()).max(Tick::ZERO))
}

impl Session {
    pub fn new() -> Session {
        Session::default()
    }

    /// Run a command by id.
    pub fn execute(&mut self, id: &str, params: Value) -> Result<Value> {
        let spec = commands::find(id).ok_or_else(|| EngineError::UnknownCommand(id.to_string()))?;
        if let Err(why) = (spec.enabled)(self) {
            // Explicit targets (agents, scripts) don't need a UI selection.
            let explicit = (["layer", "layers", "prop", "keys"].iter().any(|k| params.get(k).is_some()) && commands::has_comp(self).is_ok())
                || ["item", "items"].iter().any(|k| params.get(k).is_some())
                // An explicit composition (the enablement looks at the active one).
                || params.get("comp").is_some_and(|c| !c.is_null() && self.resolve_comp(Some(c)).is_ok());
            if !explicit {
                return Err(EngineError::Disabled(id.to_string(), why));
            }
        }
        // Last-resort guard: a panicking command reports an error instead of crashing the app.
        let r = guard::guarded(&format!("command `{id}`"), || (spec.run)(self, &params))?;
        self.learn_observe(id, &params);
        if spec.journal {
            self.journal.push((id.to_string(), params));
            if self.journal.len() > 10_000 {
                self.journal.drain(..1000);
            }
        }
        Ok(r)
    }

    /// [`Session::execute`] for agents and scripts (MCP, CLI, control channel): unknown top-level
    /// parameters are rejected with the accepted keys instead of being silently ignored.
    pub fn execute_checked(&mut self, id: &str, params: Value) -> Result<Value> {
        let spec = commands::find(id).ok_or_else(|| EngineError::UnknownCommand(id.to_string()))?;
        commands::check_params(spec, &params)?;
        self.execute(id, params)
    }

    /// [`Session::execute`] for a command a script asked for (the JavaScript engine's
    /// `app.run`, ScriptUI callbacks…). The command and everything it runs count as the script's:
    /// it may not change the settings that limit scripts (Settings ▸ Scripting & Expressions ▸
    /// Application Scripting), which only the user can, from the UI, CLI or control channel.
    pub fn execute_for_script(&mut self, id: &str, params: Value) -> Result<Value> {
        self.script_calls.0 = self.script_calls.0.saturating_add(1);
        let r = self.execute(id, params);
        self.script_calls.0 = self.script_calls.0.saturating_sub(1);
        r
    }

    /// Whether the command running was issued by a script ([`Session::execute_for_script`]).
    pub fn running_for_script(&self) -> bool {
        self.script_calls.0 > 0
    }

    /// Whether `id` names a known command whose `enabled` predicate returns `Ok` for this session;
    /// an unknown id or a predicate error reads as disabled.
    pub fn is_enabled(&self, id: &str) -> bool {
        commands::find(id).is_some_and(|c| (c.enabled)(self).is_ok())
    }

    /// Apply an undoable edit. `merge`: consecutive edits with the same key fold into one step.
    ///
    /// Every edit is an external boundary for the on-canvas text editor: if it replaced the
    /// document the editor is showing, the selection is clamped and the editor gets a fresh IME
    /// epoch. The editor's own typing goes through `edit_text_doc`, which owns that
    /// document and must not treat its own keystrokes as a replacement.
    pub fn edit<T>(&mut self, label: &str, merge: Option<&str>, f: impl FnOnce(&mut Project, &mut EditorState) -> Result<T>) -> Result<T> {
        let edited = commands::text_edit::active_editor_snapshot(self);
        let out = self.edit_inner(label, merge, f)?;
        commands::text_edit::after_external_change(self, edited.as_ref());
        Ok(out)
    }

    /// [`Session::edit`] for the on-canvas editor's own text changes. Its document is the one
    /// the editor owns, so a change here is typing, not a replacement behind the IME's back.
    pub(crate) fn edit_text_doc<T>(&mut self, label: &str, merge: Option<&str>, f: impl FnOnce(&mut Project, &mut EditorState) -> Result<T>) -> Result<T> {
        self.edit_inner(label, merge, f)
    }

    /// The shared body of [`Session::edit`] and [`Session::edit_text_doc`].
    ///
    /// Runs `f` on a clone of the project and editor state, then synchronizes derived data (global
    /// light and Essential Graphics), queues invalidated Warp Stabilizer/camera-tracker work and
    /// Roto Brush instances needing re-propagation in the `*_pending` lists. If the clone has the same
    /// content the project and history are left as they were (only editor-state changes from `f`
    /// are kept) and no revision is bumped; otherwise the previous project is recorded as an
    /// undo step under `label`, unless it merges with the current merge key (then the redo branch
    /// is dropped). An `Err` from `f` propagates with the session unchanged.
    fn edit_inner<T>(&mut self, label: &str, merge: Option<&str>, f: impl FnOnce(&mut Project, &mut EditorState) -> Result<T>) -> Result<T> {
        let before = self.project.clone();
        let mut p = (*self.project).clone();
        let mut st = self.state.clone();
        let r = f(&mut p, &mut st)?;
        // Layer styles: one Global Light per comp, whichever layer edited it.
        effectcraft_project::styles::sync_global_light(&before, &mut p);
        // Essential Properties of precomp layers follow their comps' Essential Graphics.
        effectcraft_project::essential::sync_project(&before, &mut p);
        // Warp Stabilizer analyses made from other frames are cleared (and queued again).
        for w in warp::invalidate(&before, &mut p) {
            if !self.warp_pending.contains(&w) {
                self.warp_pending.push(w);
            }
        }
        // So are 3D Camera Tracker tracks (and solves made with other settings).
        for w in camera_track::invalidate(&before, &mut p) {
            if !self.camera_pending.contains(&w) {
                self.camera_pending.push(w);
            }
        }
        // Roto Brush Input Keys follow the frames; edited instances propagate again.
        for r in roto::sync(&before, &mut p) {
            if !self.roto_pending.contains(&r) {
                self.roto_pending.push(r);
            }
        }
        // Nothing changed (a value set to what it already was): no undo step, not modified.
        if p.same_content(&before) {
            self.state = st;
            return Ok(r);
        }
        let same = merge.is_some() && merge.map(str::to_string) == self.history.merge_key;
        if !same {
            // Undone steps aren't lost: they stay in the History panel as a branch.
            let levels = self.prefs.general.undo_levels.max(1) as usize;
            self.history.record(label, before, levels);
        } else {
            self.history.abandon_redo(&before);
        }
        self.history.merge_key = merge.map(str::to_string);
        self.project = Arc::new(p);
        self.state = st;
        self.bump();
        Ok(r)
    }

    /// Note a change to the on-canvas editor's committed text or session.
    ///
    /// Frontends compare this with [`commands::text_edit::ime_stamp_revision`]; an unrelated
    /// project change must not advance it.
    pub(crate) fn note_text_edit_change(&mut self) {
        // Checked process-wide allocation: a stamp is never wrapped or reused. On exhaustion
        // keep the previous value; an unavailable before/after pair then compares equal and
        // earns no receipt instead of accepting a stale revision.
        if let Some(revision) = commands::text_edit::next_ime_revision() {
            self.ime_revision = self.ime_revision.max(revision);
        }
    }

    pub fn bump(&mut self) {
        self.revision += 1;
        // Back at the saved state (undo, redo, a history jump or a rolled-back batch).
        if Arc::ptr_eq(&self.project, &self.saved_project) {
            self.saved_revision = self.revision;
        }
        self.refresh_disk_salt(false);
        self.events.push(Event::ProjectChanged { revision: self.revision });
    }

    /// Salt the disk cache's layer keys with the project's footage files
    /// ([`effectcraft_render::disk_cache::footage_salt`]). Taking it reads the size and time of
    /// every file, every frame of an image sequence included, so it is taken again only when the
    /// footage items changed, or with `force` (a project opened, footage reloaded, the disk cache
    /// attached), not at every edit: with a few large sequences that cost each keystroke in a
    /// text field over half a second (#1926).
    pub fn refresh_disk_salt(&mut self, force: bool) -> Option<u64> {
        use std::hash::{Hash, Hasher};
        if self.disk_cache.is_none() {
            self.disk_salt = None;
            return None;
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for (id, it) in &self.project.items {
            if let effectcraft_project::ItemKind::Footage(f) = &it.kind {
                id.hash(&mut h);
                format!("{f:?}").hash(&mut h);
            }
        }
        let footage = h.finish();
        if let Some((seen, salt)) = self.disk_salt
            && seen == footage
            && !force
        {
            return Some(salt);
        }
        let salt = effectcraft_render::disk_cache::footage_salt(&self.project);
        self.disk_salt = Some((footage, salt));
        self.layer_cache.set_disk_salt(salt);
        Some(salt)
    }

    /// Set a comp's Resolution. As in After Effects it is saved with the project (which becomes
    /// modified) but is no undo step, and it changes no pixels: the comp's cached frames stay
    /// valid ([`effectcraft_project::Comp::same_pixels`]).
    pub fn set_resolution(&mut self, comp: ItemId, r: effectcraft_project::Resolution) -> Result<()> {
        if self.project.comp(comp).ok_or(EngineError::NoComp)?.resolution == r {
            return Ok(());
        }
        Arc::make_mut(&mut self.project).comp_mut(comp).ok_or(EngineError::NoComp)?.resolution = r;
        self.bump();
        Ok(())
    }

    /// Keep what lives outside the undo history (each comp's Resolution) as it is in `from`, the
    /// project before undo, redo or a history jump restored another state.
    pub(crate) fn keep_view_settings(&mut self, from: &Project) {
        let keep: Vec<(ItemId, effectcraft_project::Resolution)> =
            self.project.comps().filter_map(|(id, c)| from.comp(*id).map(|f| (*id, f.resolution)).filter(|(_, r)| *r != c.resolution)).collect();
        if keep.is_empty() {
            return;
        }
        let p = Arc::make_mut(&mut self.project);
        for (id, r) in keep {
            if let Some(c) = p.comp_mut(id) {
                c.resolution = r;
            }
        }
    }

    /// Undoes one step: the current project goes onto the redo stack, per-comp Resolution is kept
    /// as it was, the merge key is cleared, selections are sanitized for the replaced state (the
    /// on-canvas editor gets a new IME epoch only when its own document changed) and the session is
    /// bumped. Returns `false`, changing nothing, when there is nothing to undo.
    pub fn undo(&mut self) -> bool {
        let Some((label, p)) = self.history.undo.pop() else { return false };
        let edited = commands::text_edit::active_editor_snapshot(self);
        let cur = std::mem::replace(&mut self.project, p);
        self.keep_view_settings(&cur);
        self.history.redo.push((label, cur));
        self.history.merge_key = None;
        self.sanitize_after_replacement(edited.as_ref());
        self.bump();
        true
    }

    /// Redoes one step: the current project goes back onto the undo stack, per-comp Resolution is
    /// kept, the merge key is cleared, selections are sanitized for the replaced state (a changed
    /// text document starts a new IME epoch) and the session is bumped. Returns `false`, changing
    /// nothing, when there is nothing to redo.
    pub fn redo(&mut self) -> bool {
        let Some((label, p)) = self.history.redo.pop() else { return false };
        let edited = commands::text_edit::active_editor_snapshot(self);
        let cur = std::mem::replace(&mut self.project, p);
        self.keep_view_settings(&cur);
        self.history.undo.push((label, cur));
        self.history.merge_key = None;
        self.sanitize_after_replacement(edited.as_ref());
        self.bump();
        true
    }

    /// Keep selections valid after the project was replaced by another state (undo, redo, a
    /// history jump, a rollback): the same layer id can name a different document, so the
    /// on-canvas editor gets a fresh IME epoch only when its own document changed.
    ///
    /// Normally `before` is [`commands::text_edit::active_editor_snapshot`] taken from the
    /// state this call is about to replace. Atomic batch rollback is the exception: it has
    /// already restored the pre-batch state and passes the snapshot of the failing intermediate
    /// state, so a transient restart or component switch inside the batch still rejects reviving
    /// the old composition. A restored state that still has the same composition, layer and
    /// editor session and shows byte/style-equivalent text (an opacity or other property undo)
    /// keeps the editor's owner and selection; a different document, comp, layer or session clamps the
    /// selection and starts a new epoch. A closed editor stays closed, and a fallback comp's
    /// matching layer id never inherits the old comp's session. `None` means no editor was open
    /// when the snapshot was taken; if one is open after the replacement it is treated as an
    /// unverified document replacement, so it never inherits an old composition.
    pub fn sanitize_after_replacement(&mut self, before: Option<&commands::text_edit::EditorSnapshot>) {
        self.sanitize_state();
        commands::text_edit::after_replacement(self, before);
    }

    /// Drop selections that no longer exist. Each comp keeps its own selection (#1001); a layer id
    /// counts only inside the comp that owns it, so the active comp never sees another comp's layers.
    pub fn sanitize_state(&mut self) {
        let p = self.project.clone();
        self.state.open_comps.retain(|c| p.comp(*c).is_some());
        self.state.comp_selections.retain(|cid, sel| {
            let Some(c) = p.comp(*cid) else { return false };
            let ids: Vec<LayerId> = c.layers.iter().map(|l| l.id).collect();
            sel.layers.retain(|l| ids.contains(l));
            sel.props.retain(|(l, _)| ids.contains(l));
            sel.keys.retain(|k| ids.contains(&k.layer));
            sel.vertices.retain(|v| ids.contains(&v.layer));
            !sel.is_empty()
        });
        if self.state.active_comp.is_some_and(|c| p.comp(c).is_none()) {
            self.clear_live_selection();
            // A matching layer id in the fallback comp is not the document being edited.
            self.state.text_edit = None;
            self.state.active_comp = self.state.open_comps.first().copied();
            if let Some(id) = self.state.active_comp {
                self.recall_selection(id);
            }
        }
        if let Some(c) = self.active_comp() {
            let ids: Vec<LayerId> = c.layers.iter().map(|l| l.id).collect();
            self.state.selected_layers.retain(|l| ids.contains(l));
            self.state.selected_props.retain(|(l, _)| ids.contains(l));
            self.state.selected_keys.retain(|k| ids.contains(&k.layer));
            self.state.selected_vertices.retain(|v| ids.contains(&v.layer));
        }
        if let Some((l, t)) = self.state.current_track
            && self.active_comp().and_then(|c| c.layer(l)).and_then(|l| l.tracker(t)).is_none()
        {
            self.state.current_track = None;
        }
        self.state.project_selection.retain(|i| p.item(*i).is_some());
        commands::text_edit::sanitize(self);
    }

    pub fn is_dirty(&self) -> bool {
        self.revision != self.saved_revision
    }

    /// The project as it is now is the saved one (after opening or saving it).
    pub fn mark_saved(&mut self) {
        self.saved_project = self.project.clone();
        self.saved_revision = self.revision;
    }

    /// The project has changes its file doesn't (a session restored with unsaved changes): it
    /// stays modified until it is saved, whatever is undone.
    pub fn mark_unsaved(&mut self) {
        self.saved_project = Arc::new(Project::default());
        self.bump();
    }

    pub fn active_comp(&self) -> Option<&Comp> {
        self.project.comp(self.state.active_comp?)
    }

    /// [`Session::active_comp`] as a shared snapshot (an `Arc` clone, not a deep copy).
    pub fn active_comp_arc(&self) -> Option<std::sync::Arc<Comp>> {
        self.project.comp_arc(self.state.active_comp?)
    }

    pub fn active_comp_id(&self) -> Option<ItemId> {
        self.state.active_comp.filter(|c| self.project.comp(*c).is_some())
    }

    /// Current time of a comp.
    pub fn time_of(&self, comp: ItemId) -> Tick {
        self.state.times.get(&comp).copied().unwrap_or(Tick::ZERO)
    }

    /// Current time of the active comp.
    pub fn time(&self) -> Tick {
        self.state.active_comp.and_then(|c| self.state.times.get(&c).copied()).unwrap_or(Tick::ZERO)
    }

    /// Move the active comp's current time.
    ///
    /// Time is an external boundary for the on-canvas text editor too: an animated Source Text
    /// can show a different document at another time. The document is snapshotted only while an
    /// editor is open, so playback with no text editor does no text copying.
    pub fn set_time(&mut self, t: Tick) {
        if let Some(c) = self.state.active_comp {
            let t = self.project.comp(c).map_or(t, |comp| comp_frame_at(comp, t));
            let edited = commands::text_edit::active_editor_snapshot(self);
            self.state.times.insert(c, t);
            if self.prefs.general.sync_time_related_items {
                // Direct call: this `set_time` owns the editor snapshot for both time changes,
                // so a synced move does not take a second snapshot or start a second epoch.
                self.sync_related_times_inner(c, t);
            }
            commands::text_edit::after_external_change(self, edited.as_ref());
        }
    }

    /// Settings ▸ General ▸ Synchronize Time of All Related Items: moving the current time of
    /// `comp` moves the current time of the comps nested in it (through their precomp layers'
    /// timing) and of the comps it is nested in, so every related viewer shows the same moment.
    pub fn sync_related_times(&mut self, comp: ItemId, t: Tick) {
        // Moving a related comp can move the active one, so the editor's document is checked
        // here too; the snapshot is skipped entirely when no text editor is open.
        let edited = commands::text_edit::active_editor_snapshot(self);
        self.sync_related_times_inner(comp, t);
        commands::text_edit::after_external_change(self, edited.as_ref());
    }

    /// Sets the time of every comp related to `comp` through precomp layers: down into the comps
    /// it nests (through each layer's timing) and up into the comps nesting it. A seen-set keeps
    /// cycles and diamonds finite; each found time is clamped to its comp's last frame, snapped to
    /// the nearest frame and stored in `state.times`. The caller owns the text-editor boundary.
    fn sync_related_times_inner(&mut self, comp: ItemId, t: Tick) {
        let mut seen = std::collections::BTreeSet::from([comp]);
        let mut todo = vec![(comp, t)];
        let mut found = vec![];
        while let Some((cid, ct)) = todo.pop() {
            if cid != comp {
                found.push((cid, ct));
            }
            let Some(c) = self.project.comp(cid) else { continue };
            // Down: the comps this one nests.
            for l in &c.layers {
                if let effectcraft_project::LayerSource::Comp { item } = &l.source
                    && seen.insert(*item)
                {
                    todo.push((*item, l.layer_time(ct)));
                }
            }
            // Up: the comps nesting this one.
            for iid in self.project.items.keys() {
                if let Some(pc) = self.project.comp(*iid)
                    && let Some(l) = pc.layers.iter().find(|l| matches!(&l.source, effectcraft_project::LayerSource::Comp { item } if *item == cid))
                    && seen.insert(*iid)
                {
                    todo.push((*iid, l.comp_time(ct)));
                }
            }
        }
        for (cid, ct) in found {
            if let Some(c) = self.project.comp(cid) {
                let ct = c.frame_rate.snap_nearest(ct.clamp(Tick::ZERO, (c.duration - c.frame_duration()).max(Tick::ZERO)));
                self.state.times.insert(cid, ct);
            }
        }
    }

    /// Make `id` the active comp, parking the previous comp's selection and restoring this one's.
    pub fn activate_comp(&mut self, id: ItemId) {
        if self.project.comp(id).is_none() || self.state.active_comp == Some(id) {
            return;
        }
        self.remember_selection();
        self.state.active_comp = Some(id);
        self.recall_selection(id);
        self.state.current_track = None;
        // A text-edit session belongs to the comp that owned it. Switching comps ends it; even
        // a same-numbered layer in the new comp must not inherit the old IME composition.
        self.state.text_edit = None;
    }

    /// Leave no comp active, keeping the one that was active in [`EditorState::comp_selections`].
    pub fn activate_comp_none(&mut self) {
        self.remember_selection();
        self.state.active_comp = None;
        self.clear_live_selection();
        self.state.current_track = None;
        self.state.text_edit = None;
    }

    fn remember_selection(&mut self) {
        let Some(id) = self.state.active_comp else { return };
        let sel = CompSelection {
            layers: self.state.selected_layers.clone(),
            props: self.state.selected_props.clone(),
            keys: self.state.selected_keys.clone(),
            vertices: self.state.selected_vertices.clone(),
        };
        if sel.is_empty() {
            self.state.comp_selections.remove(&id);
        } else {
            self.state.comp_selections.insert(id, sel);
        }
    }

    fn recall_selection(&mut self, id: ItemId) {
        match self.state.comp_selections.remove(&id) {
            Some(sel) => {
                self.state.selected_layers = sel.layers;
                self.state.selected_props = sel.props;
                self.state.selected_keys = sel.keys;
                self.state.selected_vertices = sel.vertices;
            }
            None => self.clear_live_selection(),
        }
    }

    fn clear_live_selection(&mut self) {
        self.state.selected_layers.clear();
        self.state.selected_props.clear();
        self.state.selected_keys.clear();
        self.state.selected_vertices.clear();
    }

    /// Open a comp in the viewer/timeline and make it active.
    pub fn open_comp(&mut self, id: ItemId) {
        if self.project.comp(id).is_none() {
            return;
        }
        if !self.state.open_comps.contains(&id) {
            self.state.open_comps.push(id);
        }
        if self.state.active_comp != Some(id) {
            let previous = self.state.active_comp;
            self.state.recent_comps.retain(|c| *c != id && Some(*c) != previous);
            if let Some(previous) = previous {
                self.state.recent_comps.insert(0, previous);
                self.state.recent_comps.truncate(32);
            }
        }
        self.activate_comp(id);
        self.events.push(Event::OpenComp(id));
    }

    /// The open compositions and their current times, as a project file records them
    /// ([`effectcraft_project::SavedView`]).
    pub fn saved_view(&self) -> effectcraft_project::SavedView {
        let comp_times = self.state.times.iter().filter(|(id, _)| self.project.comp(**id).is_some()).map(|(id, t)| (*id, *t)).collect();
        effectcraft_project::SavedView { active_comp: self.state.active_comp, open_comps: self.state.open_comps.clone(), comp_times }
    }

    /// Reopen the compositions a project file was saved with (#748), each at its saved current
    /// time (#1714). Ids that no longer name a comp are skipped; with none left the project stays
    /// on the comp [`Session::replace_project`] opened (its first), as files from before the view
    /// was saved do.
    pub fn restore_view(&mut self, view: &effectcraft_project::SavedView) {
        for (id, t) in &view.comp_times {
            if let Some(comp) = self.project.comp(*id) {
                let t = comp_frame_at(comp, *t);
                self.state.times.insert(*id, t);
            }
        }
        let mut open: Vec<ItemId> = Vec::new();
        for id in view.open_comps.iter().copied().filter(|id| self.project.comp(*id).is_some()) {
            if !open.contains(&id) {
                open.push(id);
            }
        }
        let Some(active) = view.active_comp.filter(|id| self.project.comp(*id).is_some()).or_else(|| open.first().copied()) else { return };
        self.state.open_comps = open;
        self.open_comp(active);
    }

    pub fn toast(&mut self, msg: impl Into<String>) {
        self.events.push(Event::Toast { message: msg.into(), error: false });
    }

    pub fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    /// The 3D view camera of a comp's viewer (None = the active camera).
    pub fn view_camera(&self, comp: ItemId) -> Option<effectcraft_render::three_d::CameraState> {
        let c = self.project.comp(comp)?;
        if !c.has_3d() {
            return None;
        }
        self.state.views3d.get(&comp)?.override_camera(c.width as f64, c.height as f64)
    }

    /// Render a comp frame with the session's footage + expression hosts.
    pub fn render(&self, comp: ItemId, t: Tick, opts: RenderOpts) -> Image {
        self.renderer(opts).comp_frame(comp, t)
    }

    /// The renderer [`Session::render`] uses: the session's footage, expressions, layer cache and
    /// accelerator, and the render prefs (nested switches, draft shadows) over `opts`.
    pub fn renderer(&self, opts: RenderOpts) -> Renderer<'_> {
        let opts = RenderOpts { nested_switches: self.prefs.general.switches_affect_nested_comps, draft_shadows: self.prefs.three_d.realtime_shadows, ..opts };
        let mut r = Renderer::new(&self.project, self.footage.as_ref(), opts);
        r.expr = self.expr.as_deref();
        r.cache = Some(&self.layer_cache);
        r.accel = self.accel.as_deref();
        r
    }

    /// Resolve a comp reference (item id number or name; `None`/null = the active comp).
    pub fn resolve_comp(&self, comp: Option<&Value>) -> Result<ItemId> {
        let p = match comp {
            Some(v) if !v.is_null() => serde_json::json!({ "comp": v }),
            _ => serde_json::json!({}),
        };
        commands::comp_id(self, &p)
    }

    /// Render a comp frame (comp time `t`) as opaque 8-bit RGBA over the comp background, scaled
    /// so the longest side is at most `max_side` pixels (0 = full size). Returns (width, height, rgba).
    pub fn render_rgba8(&self, comp: ItemId, t: Tick, max_side: u32) -> Result<(u32, u32, Vec<u8>)> {
        let c = self.project.comp(comp).ok_or(EngineError::NoComp)?;
        let long = c.width.max(c.height).max(1) as f64;
        let scale = if max_side == 0 { 1.0 } else { (max_side as f64 / long).min(1.0) };
        let img = self.render(comp, t, RenderOpts { scale, backend: effectcraft_render::Backend::Auto, ..Default::default() });
        Ok((img.width, img.height, img.to_rgba8_over(c.background)))
    }

    /// A Project panel thumbnail of an item (a comp at `t`, footage at its first frame), longest
    /// side at most `max_side`, as a job that owns everything it reads: frontends run it off the
    /// UI thread (a big comp takes far longer than a frame). `None` for items without pixels.
    pub fn thumbnail_job(&self, item: ItemId, t: Tick, max_side: u32) -> Option<ThumbnailJob> {
        let it = self.project.item(item)?;
        let footage = self.footage.clone();
        match &it.kind {
            effectcraft_project::ItemKind::Comp(c) => {
                let (project, expr, cache) = (self.project.clone(), self.expr.clone(), self.layer_cache.clone());
                let long = c.width.max(c.height).max(1) as f64;
                let scale = if max_side == 0 { 1.0 } else { (max_side as f64 / long).min(1.0) };
                let opts = RenderOpts {
                    scale,
                    nested_switches: self.prefs.general.switches_affect_nested_comps,
                    draft_shadows: self.prefs.three_d.realtime_shadows,
                    ..Default::default()
                };
                let bg = c.background;
                Some(Box::new(move || {
                    let mut r = Renderer::new(&project, footage.as_ref(), opts);
                    r.expr = expr.as_deref();
                    r.cache = Some(&cache);
                    let img = r.comp_frame(item, t);
                    Some((img.width, img.height, img.to_rgba8_over(bg)))
                }))
            }
            effectcraft_project::ItemKind::Footage(f) if f.has_video => {
                let f = f.clone();
                Some(Box::new(move || {
                    let img = footage.frame(item, &f, Tick(0))?;
                    // Point-sample down to at most `max_side` on the long side.
                    let step = (img.width.max(img.height) as f32 / max_side.max(1) as f32).ceil().max(1.0) as u32;
                    let (w, h) = (img.width.div_ceil(step), img.height.div_ceil(step));
                    let full = img.to_rgba8();
                    let mut px = Vec::with_capacity((w * h * 4) as usize);
                    for y in 0..h {
                        for x in 0..w {
                            let i = (((y * step) * img.width + x * step) * 4) as usize;
                            px.extend_from_slice(&full[i..i + 4]);
                        }
                    }
                    Some((w, h, px))
                }))
            }
            _ => None,
        }
    }

    /// [`Session::render_rgba8`], or with `transparent` the frame's own straight alpha (what a
    /// render with RGB + Alpha channels writes) instead of compositing over the background.
    pub fn render_rgba8_alpha(&self, comp: ItemId, t: Tick, max_side: u32, transparent: bool) -> Result<(u32, u32, Vec<u8>)> {
        if !transparent {
            return self.render_rgba8(comp, t, max_side);
        }
        let c = self.project.comp(comp).ok_or(EngineError::NoComp)?;
        let long = c.width.max(c.height).max(1) as f64;
        let scale = if max_side == 0 { 1.0 } else { (max_side as f64 / long).min(1.0) };
        let img = self.render(comp, t, RenderOpts { scale, backend: effectcraft_render::Backend::Auto, ..Default::default() });
        Ok((img.width, img.height, img.to_rgba8()))
    }

    /// Make the media layer and the layer cache let go of what they decoded from `footage`'s
    /// files (it keys frames by path, so an overwritten file would keep showing its old pixels).
    pub(crate) fn forget_decoded(&self, footage: &effectcraft_project::Footage) {
        for p in std::iter::once(&footage.path).chain(&footage.sequence) {
            self.footage.forget(p);
        }
        self.layer_cache.clear();
    }

    /// Replace the whole project (open/new), resetting history and state.
    pub fn replace_project(&mut self, mut p: Project, path: Option<String>) {
        if self.stop_render()
            && let Some(mut job) = self.render_job.take()
        {
            // Let the worker notice the cancel; its updates refer to the old project.
            job.wait();
        }
        self.render_job = None;
        self.stored_paths = Default::default();
        self.stop_track();
        self.track_job = None;
        self.stop_mask_track();
        self.mask_job = None;
        self.stop_warp();
        self.warp_job = None;
        self.warp_pending.clear();
        self.stop_camera();
        self.camera_job = None;
        self.camera_pending.clear();
        self.stop_roto();
        self.roto_job = None;
        self.roto_pending.clear();
        self.drop_tasks();
        self.script_ui.project_epoch = self.script_ui.project_epoch.wrapping_add(1);
        p.fix_next_id();
        upgrade_effects(&mut p);
        // The media layer keys decoded frames by path: files of the new project may have changed
        // since an earlier project decoded them (#1070).
        for it in p.items.values() {
            if let effectcraft_project::ItemKind::Footage(f) = &it.kind {
                self.forget_decoded(f);
            }
        }
        self.layer_cache.clear();
        self.project = Arc::new(p);
        // Its files may have changed since they were last stamped.
        self.disk_salt = None;
        self.history = History::default();
        self.state = EditorState { snapping: true, view_layout: 1, ..Default::default() };
        self.path = path;
        self.bump();
        self.mark_saved();
        let first = self.project.comps().next().map(|(id, _)| *id);
        if let Some(c) = first {
            self.open_comp(c);
        }
    }
}

/// Bring effect instances saved by earlier versions up to date with the effect registry
/// (renamed / regrouped / added parameters, reordered popups; see
/// [`effectcraft_effects::migrate`]), and a project from before
/// [`effectcraft_project::SCHEMA_EFFECT_SPACE`] into effect space
/// ([`effectcraft_effects::migrate::to_effect_space`]). Stamps the current schema.
pub fn upgrade_effects(p: &mut Project) {
    let to_effect_space = p.schema < effectcraft_project::SCHEMA_EFFECT_SPACE;
    // Effect bounds per layer: (size, top-left in layer coordinates).
    let mut bounds = std::collections::HashMap::new();
    for (cid, c) in p.comps() {
        for l in &c.layers {
            bounds.insert((*cid, l.id), effectcraft_render::effect_bounds(p, c, l));
        }
    }
    let mut next = p.next_id;
    for (cid, it) in p.items.iter_mut() {
        let effectcraft_project::ItemKind::Comp(c) = &mut it.kind else { continue };
        for l in &mut std::sync::Arc::make_mut(c).layers {
            let (size, origin) = bounds.get(&(*cid, l.id)).copied().unwrap_or(([100.0, 100.0], [0.0; 2]));
            // Effect space starts at the bounds' top-left: positions move by minus that corner.
            // Adjustment layers run their effects on the comp below as it is: nothing moves.
            let shift = |o: [f64; 2]| [-o[0], -o[1]];
            let d = if l.switches.adjustment { [0.0; 2] } else { shift(origin) };
            let clone_d = |src: Option<u64>| match src {
                Some(id) => bounds.get(&(*cid, effectcraft_project::LayerId(id))).map_or([0.0; 2], |b| shift(b.1)),
                None => d,
            };
            let Some(fx) = l.props.sub_mut("effects") else { continue };
            for n in &mut fx.children {
                let effectcraft_project::Node::Group(g) = n else { continue };
                let effectcraft_project::GroupKind::Effect { effect } = &g.kind else { continue };
                if let Some(spec) = effectcraft_effects::find(effect) {
                    effectcraft_effects::migrate::upgrade_instance(spec, g, &mut effectcraft_project::build::Ids(&mut next), size);
                    if to_effect_space {
                        effectcraft_effects::migrate::to_effect_space(spec, g, size, d, &clone_d);
                    }
                }
            }
        }
    }
    p.next_id = next;
    p.schema = effectcraft_project::SCHEMA_VERSION;
}
