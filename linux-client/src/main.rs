mod engine;
mod gate;
mod media;
mod protocol;

use engine::{Command, UiEvent};
use gtk::prelude::*;
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::{mpsc, Arc},
    time::Duration,
};
use tokio::sync::mpsc::UnboundedSender;

fn entry(parent: &gtk::Box, title: &str, value: &str, placeholder: &str) -> gtk::Entry {
    let label = gtk::Label::new(Some(title));
    label.set_xalign(0.0);
    parent.append(&label);
    let entry = gtk::Entry::builder()
        .text(value)
        .placeholder_text(placeholder)
        .hexpand(true)
        .build();
    parent.append(&entry);
    entry
}

fn build(app: &gtk::Application, smoke: bool) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Mamla")
        .default_width(440)
        .default_height(560)
        .build();
    let layout = gtk::Box::new(gtk::Orientation::Vertical, 12);
    layout.set_margin_top(24);
    layout.set_margin_bottom(24);
    layout.set_margin_start(24);
    layout.set_margin_end(24);
    let heading = gtk::Label::new(Some("Mamla"));
    heading.add_css_class("title-1");
    heading.set_xalign(0.0);
    layout.append(&heading);
    let subtitle = gtk::Label::new(Some("Голосовая комната · PipeWire"));
    subtitle.set_xalign(0.0);
    subtitle.add_css_class("dim-label");
    layout.append(&subtitle);

    let server = entry(
        &layout,
        "Сервер",
        "http://localhost:8080",
        "http://192.168.0.235:8080",
    );
    let room = entry(
        &layout,
        "ID комнаты",
        "local-test",
        "Одинаковый у всех участников",
    );
    let name = entry(&layout, "Ваше имя", "Гость", "Как вас представить?");
    let access_key = entry(
        &layout,
        "Личный ключ доступа",
        "",
        "mk_… — если сервер требует приглашение",
    );
    access_key.set_visibility(false);

    let advanced = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let input = entry(
        &advanced,
        "Микрофон",
        "",
        "По умолчанию или PipeWire node.name / ID",
    );
    let output = entry(
        &advanced,
        "Динамики / наушники",
        "",
        "По умолчанию или PipeWire node.name / ID",
    );
    let cert = entry(
        &advanced,
        "Доверенный сертификат сервера (.pem)",
        "",
        "Необязательно: /home/user/mamla-cert.pem",
    );
    let join_insecure = gtk::Button::with_label("Войти без проверки TLS");
    join_insecure.add_css_class("destructive-action");
    advanced.append(&join_insecure);
    let warning = gtk::Label::new(Some(
        "Без проверки TLS возможны подмена сервера и кража ключа доступа. Только для теста в доверенной сети.",
    ));
    warning.set_wrap(true);
    warning.set_xalign(0.0);
    advanced.append(&warning);
    let expander = gtk::Expander::builder()
        .label("Устройства и сертификат")
        .child(&advanced)
        .build();
    layout.append(&expander);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let join = gtk::Button::with_label("Войти");
    join.add_css_class("suggested-action");
    join.set_hexpand(true);
    let leave = gtk::Button::with_label("Выйти");
    leave.set_sensitive(false);
    actions.append(&join);
    actions.append(&leave);
    layout.append(&actions);
    let mute = gtk::ToggleButton::with_label("Выключить микрофон");
    mute.set_sensitive(false);
    layout.append(&mute);
    let gate_settings = Arc::new(gate::Settings::default());
    let gate_enable = gtk::CheckButton::with_label("Noise gate — отсекать тихий фон");
    layout.append(&gate_enable);
    let gate_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    gate_row.append(&gtk::Label::new(Some("Порог, дБFS")));
    let gate_threshold = gtk::Scale::with_range(gtk::Orientation::Horizontal, -80.0, -10.0, 1.0);
    gate_threshold.set_hexpand(true);
    gate_threshold.set_digits(0);
    gate_threshold.set_draw_value(true);
    gate_threshold.set_value(-45.0);
    gate_threshold.set_sensitive(false);
    gate_row.append(&gate_threshold);
    layout.append(&gate_row);
    let gate_hint = gtk::Label::new(Some(
        "Выше порог — меньше фона, но тихая речь может обрезаться.",
    ));
    gate_hint.set_wrap(true);
    gate_hint.set_xalign(0.0);
    gate_hint.add_css_class("dim-label");
    layout.append(&gate_hint);
    {
        let settings = gate_settings.clone();
        let threshold = gate_threshold.clone();
        gate_enable.connect_toggled(move |button| {
            settings.enable(button.is_active());
            threshold.set_sensitive(button.is_active());
        });
    }
    {
        let settings = gate_settings.clone();
        gate_threshold
            .connect_value_changed(move |scale| settings.threshold(scale.value().round() as i32));
    }
    let status = gtk::Label::new(Some("Введите адрес сервера и ID комнаты."));
    status.set_wrap(true);
    status.set_xalign(0.0);
    status.set_selectable(true);
    layout.append(&status);

    let people = gtk::ListBox::new();
    people.set_selection_mode(gtk::SelectionMode::None);
    people.add_css_class("boxed-list");
    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .min_content_height(100)
        .child(&people)
        .build();
    layout.append(&scroll);
    let note = gtk::Label::new(Some("До 6 участников. Для POC используйте наушники."));
    note.add_css_class("dim-label");
    note.set_wrap(true);
    note.set_xalign(0.0);
    layout.append(&note);
    let content = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&layout)
        .build();
    window.set_child(Some(&content));

    let command: Rc<RefCell<Option<UnboundedSender<Command>>>> = Rc::new(RefCell::new(None));
    let connection_insecure = Rc::new(Cell::new(false));
    let (ui_tx, ui_rx) = mpsc::channel();
    let fields = vec![
        server.clone(),
        room.clone(),
        name.clone(),
        access_key.clone(),
        input.clone(),
        output.clone(),
        cert.clone(),
    ];
    for (button, insecure_tls) in [(&join, false), (&join_insecure, true)] {
        let gate_settings = gate_settings.clone();
        let command = command.clone();
        let connection_insecure = connection_insecure.clone();
        let fields = fields.clone();
        let leave = leave.clone();
        let join = join.clone();
        let join_insecure = join_insecure.clone();
        let status = status.clone();
        let server = server.clone();
        let room = room.clone();
        let name = name.clone();
        let access_key = access_key.clone();
        let input = input.clone();
        let output = output.clone();
        let cert = cert.clone();
        let ui_tx = ui_tx.clone();
        button.connect_clicked(move |_| {
            let mut options = protocol::Connect {
                server: server.text().into(),
                room: room.text().into(),
                name: name.text().into(),
                access_key: access_key.text().into(),
                input: input.text().trim().into(),
                output: output.text().trim().into(),
                certificate: if cert.text().trim().is_empty() {
                    None
                } else {
                    Some(PathBuf::from(cert.text().trim()))
                },
                insecure_tls,
            };
            if let Err(error) = options.validate() {
                status.set_text(&error.to_string());
                return;
            }
            for field in &fields {
                field.set_sensitive(false);
            }
            join.set_sensitive(false);
            join_insecure.set_sensitive(false);
            leave.set_sensitive(true);
            connection_insecure.set(insecure_tls);
            status.set_text(if insecure_tls {
                "Подключение без проверки сертификата сервера…"
            } else {
                "Подключение…"
            });
            *command.borrow_mut() =
                Some(engine::start(options, ui_tx.clone(), gate_settings.clone()));
        });
    }
    {
        let command = command.clone();
        let status = status.clone();
        leave.connect_clicked(move |button| {
            if let Some(tx) = command.borrow().as_ref() {
                let _ = tx.send(Command::Leave);
            }
            button.set_sensitive(false);
            status.set_text("Отключение…");
        });
    }
    {
        let command = command.clone();
        mute.connect_toggled(move |button| {
            if let Some(tx) = command.borrow().as_ref() {
                let _ = tx.send(Command::Mute(button.is_active()));
            }
            button.set_label(if button.is_active() {
                "Включить микрофон"
            } else {
                "Выключить микрофон"
            });
        });
    }
    {
        let command = command.clone();
        window.connect_close_request(move |_| {
            if let Some(tx) = command.borrow_mut().take() {
                let _ = tx.send(Command::Leave);
            }
            glib::Propagation::Proceed
        });
    }
    let weak_window = window.downgrade();
    glib::timeout_add_local(Duration::from_millis(60), move || {
        if weak_window.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        for event in ui_rx.try_iter() {
            match event {
                UiEvent::Connected => {
                    mute.set_sensitive(true);
                    status.set_text(if connection_insecure.get() {
                        "Вы в комнате. Внимание: сертификат сервера не проверяется."
                    } else {
                        "Вы в комнате. Голос идёт по WebRTC."
                    });
                }
                UiEvent::Participants(rows) => {
                    while let Some(child) = people.first_child() {
                        people.remove(&child);
                    }
                    for row in rows {
                        let line = gtk::Box::new(gtk::Orientation::Vertical, 4);
                        line.set_margin_top(10);
                        line.set_margin_bottom(10);
                        line.set_margin_start(12);
                        line.set_margin_end(12);
                        let label = gtk::Label::new(Some(&row.name));
                        label.set_xalign(0.0);
                        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                        let state = gtk::Label::new(Some(&row.status));
                        state.set_xalign(0.0);
                        state.add_css_class("dim-label");
                        line.append(&label);
                        line.append(&state);
                        people.append(&line);
                    }
                }
                UiEvent::Muted(value) => {
                    if mute.is_active() != value {
                        mute.set_active(value);
                    }
                }
                UiEvent::Stopped(error) => {
                    command.borrow_mut().take();
                    join.set_sensitive(true);
                    join_insecure.set_sensitive(true);
                    connection_insecure.set(false);
                    leave.set_sensitive(false);
                    mute.set_sensitive(false);
                    mute.set_active(false);
                    for field in &fields {
                        field.set_sensitive(true);
                    }
                    while let Some(child) = people.first_child() {
                        people.remove(&child);
                    }
                    status.set_text(error.as_deref().unwrap_or("Вы вышли из комнаты."));
                }
            }
        }
        glib::ControlFlow::Continue
    });
    window.present();
    if smoke {
        let app = app.clone();
        glib::timeout_add_local_once(Duration::from_millis(500), move || app.quit());
    }
}

fn main() -> glib::ExitCode {
    let smoke = std::env::args().any(|arg| arg == "--smoke-test");
    let app = gtk::Application::builder()
        .application_id("org.mamla.Voice")
        .build();
    app.connect_activate(move |app| build(app, smoke));
    app.run_with_args::<&str>(&[])
}
