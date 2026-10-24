//! "Edit Parameters…" in an occupied insert's menu reaches the generic plugin window, hosted or
//! built-in (a click on a hosted plugin's slot opens its own editor instead).

use egui::vec2;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;
use soundcraft_engine::Engine;
use soundcraft_model::{Insert, TrackId};
use soundcraft_ui_egui::{MainWindow, Services, SoundApp};

const HOSTED: &str = "clap:no.such.plugin";

fn harness(engine: Engine) -> Harness<'static, SoundApp> {
    let mut app = SoundApp::new(engine, None, Services::default());
    app.ui.window = MainWindow::Mix;
    app.run("view.mix_section", json!({"section": "instruments"})).unwrap();
    let mut h = Harness::builder().with_size(vec2(1400.0, 1000.0)).build_ui_state(
        |ui, app: &mut SoundApp| {
            app.logic(&ui.ctx().clone());
            app.ui(ui);
        },
        app,
    );
    h.run_steps(4);
    h
}

/// A track named "Vox" with `plugin` on insert A. `mix.insert` refuses an uninstalled hosted
/// plugin, so a hosted one is written into the session directly.
fn vox_with_insert(plugin: &str) -> (Engine, TrackId) {
    let mut engine = Engine::default();
    engine.execute("track.new", &json!({"name": "Vox"})).unwrap();
    let id = engine.session().tracks[0].id;
    let insert: Insert = serde_json::from_value(json!({"plugin": plugin})).unwrap();
    engine.session_mut().track_mut(id).unwrap().mixer.inserts[0] = Some(insert);
    (engine, id)
}

#[test]
fn edit_parameters_opens_the_plugin_window_of_a_built_in_insert() {
    let plugin = soundcraft_dsp::plugins().iter().find(|p| !p.is_instrument).unwrap();
    let (engine, id) = vox_with_insert(plugin.id);
    let mut h = harness(engine);
    h.get_by_label("Insert A").click_secondary();
    h.run_steps(3);
    h.get_by_label("Edit Parameters…").click();
    h.run_steps(3);
    assert_eq!(h.state().ui.plugin_windows, vec![(id, 0)]);
    assert!(h.query_by_label("Edit Parameters…").is_none(), "the menu closes");
}

#[test]
fn edit_parameters_is_offered_for_a_hosted_insert() {
    let (engine, _) = vox_with_insert(HOSTED);
    let mut h = harness(engine);
    h.get_by_label("Insert A").click_secondary();
    h.run_steps(3);
    h.get_by_label("Edit Parameters…").click();
    h.run_steps(3);
    assert!(h.query_by_label("Edit Parameters…").is_none(), "the menu closes");
}
