//! kosmos: the place of the OpenCore plugin's collaboration widget in the title bar. The widget
//! itself is the same in every Craft app (`craft-signal-egui`); here is only what this app adds:
//! the session's status in, its commands out, its language.

use egui::Ui;

use crate::WordApp;

/// Draw the widget and do what the person asked for. Draws nothing when this build has no
/// collaboration transport.
pub(crate) fn show(ui: &mut Ui, app: &mut WordApp, height: f32) {
    let view = craft_opencore::signal::view::view(&craft_opencore::session::collab::status(&app.session));
    let look = craft_opencore::egui::Look { height, accent: crate::theme::Tokens::get(ui.ctx()).accent };
    let Some(wish) = craft_opencore::egui::show(ui, &view, look, &|text| crate::i18n::t(text).to_string()) else { return };
    // The kit's command for the wish, run through the app's own path (status line, ui requests) as before.
    let (command, params) = craft_opencore::egui::wish_command(wish);
    if let Err(e) = app.run(command, params) {
        app.status(e.to_string());
    }
}
