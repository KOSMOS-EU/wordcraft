//! kosmos: the place of the OpenCore plugin's collaboration widget in the title bar. The widget
//! itself is the same in every Craft app (`craft-signal-egui`); here is only what this app adds:
//! the session's status in, its commands out, its language.

use egui::Ui;
use serde_json::json;

use crate::WordApp;

/// Draw the widget and do what the person asked for. Draws nothing when this build has no
/// collaboration transport.
pub(crate) fn show(ui: &mut Ui, app: &mut WordApp, height: f32) {
    let view = craft_signal::view::view(&app.session.signal.status());
    let look = craft_signal_egui::Look { height, accent: crate::theme::Tokens::get(ui.ctx()).accent };
    let Some(wish) = craft_signal_egui::show(ui, &view, look, &|text| crate::i18n::t(text).to_string()) else { return };
    let (command, params) = match wish {
        craft_signal_egui::Wish::Join => ("signal.join", json!({})),
        craft_signal_egui::Wish::Leave => ("signal.disconnect", json!({})),
        craft_signal_egui::Wish::Edit => ("signal.mode", json!({"value": "edit"})),
        craft_signal_egui::Wish::Read => ("signal.mode", json!({"value": "readOnly"})),
    };
    if let Err(e) = app.run(command, params) {
        app.status(e.to_string());
    }
}
