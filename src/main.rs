mod model;
mod parser;
mod store;
mod ui;

use gtk::prelude::*;

fn main() -> gtk::glib::ExitCode {
    let app = gtk::Application::builder()
        .application_id("dev.relaybar.RelayBar")
        .build();
    app.connect_activate(ui::build);
    app.run()
}
