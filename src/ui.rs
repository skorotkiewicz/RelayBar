use std::{
    cell::RefCell,
    rc::Rc,
    sync::mpsc::{self, Sender},
    time::Duration,
};

use gtk::{gdk, gio, glib, pango, prelude::*};
use ksni::blocking::TrayMethods;
use uuid::Uuid;

use crate::{
    model::{
        Tunnel, TunnelPhase, are_additional_arguments_safe, is_valid_destination_host,
        is_valid_host_target,
    },
    parser,
    store::{Action, Store},
};

const CSS: &str = r#"
window { background: #17191d; }
.header, .footer { padding: 12px 16px; background: #202329; }
.title { font-size: 18px; font-weight: 700; }
.muted { color: #969ba5; }
.error { color: #ff6b6b; }
.warning { color: #f3a83b; }
.card { padding: 12px; margin: 5px 10px; border-radius: 12px; background: #252930; }
.endpoint { font-family: monospace; font-size: 12px; color: #b2b7c1; }
.status-running { color: #45c66b; }
.status-working { color: #f3a83b; }
.status-failed { color: #ff5d5d; }
.status-stopped { color: #727985; }
.section { font-size: 11px; font-weight: 700; color: #969ba5; }
.editor { padding: 16px; }
"#;

type SharedStore = Rc<RefCell<Store>>;

#[derive(Clone, Copy, Debug, PartialEq)]
enum TrayCommand {
    ShowOrHide,
    Quit,
}

struct RelayTray {
    commands: Sender<TrayCommand>,
    running_count: usize,
}

impl ksni::Tray for RelayTray {
    fn id(&self) -> String {
        "relaybar".into()
    }

    fn title(&self) -> String {
        match self.running_count {
            0 => "RelayBar".into(),
            1 => "RelayBar · 1 active".into(),
            count => format!("RelayBar · {count} active"),
        }
    }

    fn icon_name(&self) -> String {
        "network-server".into()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.commands.send(TrayCommand::ShowOrHide);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{MenuItem, StandardItem};

        vec![
            StandardItem {
                label: match self.running_count {
                    0 => "No active tunnels".into(),
                    1 => "1 active tunnel".into(),
                    count => format!("{count} active tunnels"),
                },
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Show / hide RelayBar".into(),
                icon_name: "window-new".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.commands.send(TrayCommand::ShowOrHide);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.commands.send(TrayCommand::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

pub fn build(app: &gtk::Application) {
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }

    install_css();
    let state = Rc::new(RefCell::new(Store::load_default()));
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("RelayBar")
        .default_width(520)
        .default_height(560)
        .build();

    render(&window, &state);
    window.present();

    let (tray_sender, tray_commands) = mpsc::channel();
    let initial_running_count = state.borrow().running_count();
    let tray_handle = match (RelayTray {
        commands: tray_sender,
        running_count: initial_running_count,
    })
    .spawn()
    {
        Ok(handle) => Some(handle),
        Err(error) => {
            eprintln!("RelayBar tray unavailable: {error}");
            None
        }
    };
    if tray_handle.is_some() {
        window.connect_close_request(|window| {
            window.hide();
            glib::Propagation::Stop
        });
    }

    let weak_window = window.downgrade();
    let state_for_tick = state.clone();
    let tray_for_tick = tray_handle.clone();
    let app_for_tick = app.clone();
    let mut tray_running_count = initial_running_count;
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let Some(window) = weak_window.upgrade() else {
            return glib::ControlFlow::Break;
        };
        while let Ok(command) = tray_commands.try_recv() {
            match command {
                TrayCommand::ShowOrHide => {
                    if window.is_visible() {
                        window.hide();
                    } else {
                        window.present();
                    }
                }
                TrayCommand::Quit => app_for_tick.quit(),
            }
        }

        let tick = state_for_tick.borrow_mut().tick();
        let action_changed = run_actions(&state_for_tick, tick.actions);
        let running_count = state_for_tick.borrow().running_count();
        if running_count != tray_running_count {
            if let Some(handle) = &tray_for_tick {
                handle.update(|tray| tray.running_count = running_count);
            }
            tray_running_count = running_count;
        }
        if tick.changed || action_changed {
            render(&window, &state_for_tick);
        }
        glib::ControlFlow::Continue
    });

    app.connect_shutdown(move |_| {
        state.borrow_mut().shutdown();
        if let Some(handle) = &tray_handle {
            handle.shutdown().wait();
        }
    });
}

fn install_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_data(CSS);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn render(window: &gtk::ApplicationWindow, state: &SharedStore) {
    let (tunnels, running_count, notice) = {
        let store = state.borrow();
        let tunnels = store
            .tunnels
            .iter()
            .cloned()
            .map(|tunnel| {
                let phase = store.phase(tunnel.id);
                (tunnel, phase)
            })
            .collect::<Vec<_>>();
        (tunnels, store.running_count(), store.notice.clone())
    };

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    header.add_css_class("header");

    let heading = gtk::Box::new(gtk::Orientation::Vertical, 2);
    heading.set_hexpand(true);
    let title = left_label("RelayBar");
    title.add_css_class("title");
    heading.append(&title);
    let activity = if running_count == 0 {
        if tunnels.is_empty() {
            "Simple SSH tunnels".into()
        } else {
            "All tunnels stopped".into()
        }
    } else if running_count == 1 {
        "1 tunnel active".into()
    } else {
        format!("{running_count} tunnels active")
    };
    let activity = left_label(&activity);
    activity.add_css_class("muted");
    heading.append(&activity);
    header.append(&heading);

    let add = gtk::Button::with_label("Add tunnel");
    let parent = window.clone();
    let editor_state = state.clone();
    add.connect_clicked(move |_| show_editor(&parent, &editor_state, None));
    header.append(&add);
    root.append(&header);

    if let Some(notice) = notice {
        let notice = left_label(&notice);
        notice.add_css_class("error");
        notice.set_margin_start(16);
        notice.set_margin_end(16);
        notice.set_margin_top(8);
        root.append(&notice);
    }

    if tunnels.is_empty() {
        let empty = gtk::Box::new(gtk::Orientation::Vertical, 12);
        empty.set_valign(gtk::Align::Center);
        empty.set_halign(gtk::Align::Center);
        empty.set_vexpand(true);
        let heading = gtk::Label::new(Some("Your shortcuts to anywhere"));
        heading.add_css_class("title");
        empty.append(&heading);
        let hint = gtk::Label::new(Some("Paste an SSH command or add a tunnel by hand."));
        hint.add_css_class("muted");
        empty.append(&hint);
        let add = gtk::Button::with_label("Add your first tunnel");
        add.add_css_class("suggested-action");
        let parent = window.clone();
        let editor_state = state.clone();
        add.connect_clicked(move |_| show_editor(&parent, &editor_state, None));
        empty.append(&add);
        root.append(&empty);
    } else {
        let list = gtk::ListBox::new();
        list.set_selection_mode(gtk::SelectionMode::None);
        list.add_css_class("boxed-list");
        for (tunnel, phase) in tunnels {
            list.append(&tunnel_row(window, state, tunnel, phase));
        }
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();
        root.append(&scroller);
    }

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    footer.add_css_class("footer");
    let ssh_note = left_label("Uses system SSH · no shell");
    ssh_note.add_css_class("muted");
    ssh_note.set_hexpand(true);
    footer.append(&ssh_note);
    let quit = gtk::Button::with_label("Quit");
    if let Some(app) = window.application() {
        quit.connect_clicked(move |_| app.quit());
    }
    footer.append(&quit);
    root.append(&footer);

    window.set_child(Some(&root));
}

fn tunnel_row(
    window: &gtk::ApplicationWindow,
    state: &SharedStore,
    tunnel: Tunnel,
    phase: TunnelPhase,
) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.add_css_class("card");

    let status = gtk::Label::new(Some("●"));
    status.add_css_class(match phase {
        TunnelPhase::Running => "status-running",
        TunnelPhase::Starting | TunnelPhase::Retrying { .. } => "status-working",
        TunnelPhase::Failed(_) => "status-failed",
        TunnelPhase::Stopped => "status-stopped",
    });
    row.append(&status);

    let details = gtk::Box::new(gtk::Orientation::Vertical, 3);
    details.set_hexpand(true);
    let name = left_label(&tunnel.display_name());
    name.set_ellipsize(pango::EllipsizeMode::End);
    details.append(&name);
    let endpoint = left_label(&format!(
        "{}  →  {}",
        tunnel.local_endpoint(),
        tunnel.destination_endpoint()
    ));
    endpoint.add_css_class("endpoint");
    endpoint.set_ellipsize(pango::EllipsizeMode::End);
    details.append(&endpoint);
    let phase_text = left_label(&phase_text(&tunnel, &phase));
    phase_text.add_css_class(if matches!(phase, TunnelPhase::Failed(_)) {
        "error"
    } else {
        "muted"
    });
    phase_text.set_ellipsize(pango::EllipsizeMode::End);
    details.append(&phase_text);
    row.append(&details);

    let open = gtk::Button::with_label("Open");
    open.set_tooltip_text(Some("Start if needed, then open the local URL"));
    let open_state = state.clone();
    let open_window = window.clone();
    let id = tunnel.id;
    open.connect_clicked(move |_| {
        let actions = open_state.borrow_mut().open_in_browser(id);
        run_actions(&open_state, actions);
        render(&open_window, &open_state);
    });
    row.append(&open);

    let edit = gtk::Button::with_label("Edit");
    let edit_state = state.clone();
    let edit_window = window.clone();
    let editable = tunnel.clone();
    edit.connect_clicked(move |_| show_editor(&edit_window, &edit_state, Some(editable.clone())));
    row.append(&edit);

    let delete = gtk::Button::with_label("Delete");
    delete.add_css_class("destructive-action");
    let delete_state = state.clone();
    let delete_window = window.clone();
    delete.connect_clicked(move |_| {
        delete_state.borrow_mut().delete(id);
        render(&delete_window, &delete_state);
    });
    row.append(&delete);

    let active = phase.is_active();
    let toggle = gtk::Button::with_label(if active { "Stop" } else { "Start" });
    if !active {
        toggle.add_css_class("suggested-action");
    }
    let toggle_state = state.clone();
    let toggle_window = window.clone();
    toggle.connect_clicked(move |_| {
        toggle_state.borrow_mut().toggle(id);
        render(&toggle_window, &toggle_state);
    });
    row.append(&toggle);
    row
}

fn phase_text(tunnel: &Tunnel, phase: &TunnelPhase) -> String {
    match phase {
        TunnelPhase::Stopped => format!("via {}", tunnel.ssh_host),
        TunnelPhase::Starting => "Connecting…".into(),
        TunnelPhase::Running => format!("Connected via {}", tunnel.ssh_host),
        TunnelPhase::Retrying {
            attempt,
            max_attempts,
            delay_seconds,
            message,
        } => format!("Retry {attempt}/{max_attempts} in {delay_seconds}s · {message}"),
        TunnelPhase::Failed(message) => message.clone(),
    }
}

fn show_editor(parent: &gtk::ApplicationWindow, state: &SharedStore, existing: Option<Tunnel>) {
    let editor = gtk::Window::builder()
        .title(if existing.is_some() {
            "Edit Tunnel"
        } else {
            "New Tunnel"
        })
        .default_width(500)
        .default_height(560)
        .modal(true)
        .transient_for(parent)
        .build();

    let root = gtk::Box::new(gtk::Orientation::Vertical, 14);
    root.add_css_class("editor");
    let title = left_label(if existing.is_some() {
        "Edit Tunnel"
    } else {
        "New Tunnel"
    });
    title.add_css_class("title");
    root.append(&title);

    let import_error = left_label("");
    import_error.add_css_class("error");
    import_error.set_visible(false);

    let name = entry("Production database");
    let ssh_host = entry("user@bastion.example.com");
    let local_port = entry("5432");
    let destination_host = entry("localhost");
    let destination_port = entry("5432");
    let preserved = Rc::new(RefCell::new((None::<String>, Vec::<String>::new())));
    let options_hint = left_label("");
    options_hint.add_css_class("warning");
    options_hint.set_visible(false);

    if existing.is_none() {
        let section = left_label("QUICK ADD");
        section.add_css_class("section");
        root.append(&section);
        let quick_add = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let command = entry("ssh -N -L 8080:localhost:3000 user@host");
        command.set_hexpand(true);
        quick_add.append(&command);
        let import = gtk::Button::with_label("Import");
        quick_add.append(&import);
        root.append(&quick_add);
        root.append(&import_error);

        let command_for_import = command.clone();
        let name_for_import = name.clone();
        let ssh_for_import = ssh_host.clone();
        let local_for_import = local_port.clone();
        let destination_for_import = destination_host.clone();
        let destination_port_for_import = destination_port.clone();
        let error_for_import = import_error.clone();
        let preserved_for_import = preserved.clone();
        let hint_for_import = options_hint.clone();
        import.connect_clicked(
            move |_| match parser::parse(command_for_import.text().as_str()) {
                Ok(imported) => {
                    local_for_import.set_text(&imported.local_port.to_string());
                    destination_for_import.set_text(&imported.destination_host);
                    destination_port_for_import.set_text(&imported.destination_port.to_string());
                    ssh_for_import.set_text(&imported.ssh_host);
                    if name_for_import.text().trim().is_empty() {
                        name_for_import.set_text(&format!(
                            "{}:{}",
                            imported.destination_host, imported.destination_port
                        ));
                    }
                    update_options_hint(
                        &hint_for_import,
                        imported.bind_address.as_deref(),
                        &imported.additional_arguments,
                    );
                    *preserved_for_import.borrow_mut() =
                        (imported.bind_address, imported.additional_arguments);
                    error_for_import.set_visible(false);
                    name_for_import.grab_focus();
                }
                Err(error) => {
                    error_for_import.set_text(&error.to_string());
                    error_for_import.set_visible(true);
                }
            },
        );
    }

    let section = left_label("DETAILS");
    section.add_css_class("section");
    root.append(&section);
    let grid = gtk::Grid::builder()
        .row_spacing(10)
        .column_spacing(12)
        .build();
    add_field(&grid, 0, "Name (optional)", &name);
    add_field(&grid, 1, "SSH host", &ssh_host);
    add_field(&grid, 2, "Local port", &local_port);
    add_field(&grid, 3, "Destination host", &destination_host);
    add_field(&grid, 4, "Destination port", &destination_port);
    root.append(&grid);
    root.append(&options_hint);

    if let Some(tunnel) = &existing {
        name.set_text(&tunnel.name);
        ssh_host.set_text(&tunnel.ssh_host);
        local_port.set_text(&tunnel.local_port.to_string());
        destination_host.set_text(&tunnel.destination_host);
        destination_port.set_text(&tunnel.destination_port.to_string());
        *preserved.borrow_mut() = (
            tunnel.bind_address.clone(),
            tunnel.additional_arguments.clone(),
        );
        update_options_hint(
            &options_hint,
            tunnel.bind_address.as_deref(),
            &tunnel.additional_arguments,
        );
    } else {
        destination_host.set_text("localhost");
    }

    let validation_error = left_label("");
    validation_error.add_css_class("error");
    validation_error.set_visible(false);
    root.append(&validation_error);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let editor_for_cancel = editor.clone();
    cancel.connect_clicked(move |_| editor_for_cancel.close());
    actions.append(&cancel);
    let save = gtk::Button::with_label(if existing.is_some() {
        "Save Changes"
    } else {
        "Add Tunnel"
    });
    save.add_css_class("suggested-action");
    actions.append(&save);
    root.append(&actions);

    let saved_id = existing.as_ref().map(|tunnel| tunnel.id);
    let parent_for_save = parent.clone();
    let editor_for_save = editor.clone();
    let state_for_save = state.clone();
    save.connect_clicked(move |_| {
        let local = local_port
            .text()
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0);
        let destination = destination_port
            .text()
            .parse::<u16>()
            .ok()
            .filter(|port| *port > 0);
        let ssh = ssh_host.text().trim().to_owned();
        let destination_host_value = destination_host.text().trim().to_owned();
        let (bind_address, additional_arguments) = preserved.borrow().clone();
        if local.is_none()
            || destination.is_none()
            || !is_valid_host_target(&ssh)
            || !is_valid_destination_host(&destination_host_value)
            || !are_additional_arguments_safe(&additional_arguments)
        {
            validation_error
                .set_text("Enter valid ports and hosts; imported SSH options must be safe.");
            validation_error.set_visible(true);
            return;
        }

        let mut tunnel = Tunnel::new(
            name.text().trim().to_owned(),
            local.unwrap(),
            destination_host_value,
            destination.unwrap(),
            ssh,
        );
        if let Some(id) = saved_id {
            tunnel.id = id;
        }
        tunnel.bind_address = bind_address;
        tunnel.additional_arguments = additional_arguments;
        if saved_id.is_some() {
            state_for_save.borrow_mut().update(tunnel);
        } else {
            state_for_save.borrow_mut().add(tunnel);
        }
        editor_for_save.close();
        render(&parent_for_save, &state_for_save);
    });

    editor.set_child(Some(&root));
    editor.present();
}

fn update_options_hint(label: &gtk::Label, bind_address: Option<&str>, arguments: &[String]) {
    if let Some(bind_address) = bind_address {
        let probe = Tunnel {
            id: Uuid::nil(),
            name: String::new(),
            local_port: 1,
            destination_host: "localhost".into(),
            destination_port: 1,
            ssh_host: "host".into(),
            bind_address: Some(bind_address.into()),
            additional_arguments: Vec::new(),
        };
        label.set_text(if probe.exposes_beyond_loopback() {
            "Warning: this tunnel listens beyond localhost; imported SSH options are preserved."
        } else {
            "Imported bind address and SSH options are preserved."
        });
        label.set_visible(true);
    } else if !arguments.is_empty() {
        label.set_text("Imported SSH options are preserved.");
        label.set_visible(true);
    } else {
        label.set_visible(false);
    }
}

fn run_actions(state: &SharedStore, actions: Vec<Action>) -> bool {
    let mut changed = false;
    for action in actions {
        match action {
            Action::OpenUrl(url) => {
                if let Err(error) =
                    gio::AppInfo::launch_default_for_uri(&url, None::<&gio::AppLaunchContext>)
                {
                    state
                        .borrow_mut()
                        .set_notice(format!("Could not open {url}: {error}"));
                    changed = true;
                }
            }
        }
    }
    changed
}

fn add_field(grid: &gtk::Grid, row: i32, title: &str, entry: &gtk::Entry) {
    let label = left_label(title);
    grid.attach(&label, 0, row, 1, 1);
    entry.set_hexpand(true);
    grid.attach(entry, 1, row, 1, 1);
}

fn entry(placeholder: &str) -> gtk::Entry {
    gtk::Entry::builder().placeholder_text(placeholder).build()
}

fn left_label(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).xalign(0.0).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_reports_activity_and_sends_commands() {
        let (sender, receiver) = mpsc::channel();
        let mut tray = RelayTray {
            commands: sender,
            running_count: 2,
        };

        assert_eq!(ksni::Tray::title(&tray), "RelayBar · 2 active");
        ksni::Tray::activate(&mut tray, 0, 0);
        assert_eq!(receiver.recv().unwrap(), TrayCommand::ShowOrHide);
    }
}
