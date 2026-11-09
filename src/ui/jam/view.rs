//! Listen Together: v1.9's choice/preview/dashboard flow using the native app's
//! shared page margins, cards, fields and buttons. Networking and audio
//! reconciliation stay in the parent module.

use std::collections::VecDeque;
use std::future::Future;
use std::time::{SystemTime, UNIX_EPOCH};

use crossbeam_channel::{Receiver, TryRecvError, bounded};
use egui::{Align, Align2, Color32, Layout, Rect, RichText, Sense, Ui, vec2};
use qrcode::{Color, QrCode};
use serde_json::json;

use super::{App, control, invite_code, join, leave, start};
use crate::jam::{
    self,
    protocol::{Activity, Preview},
};
use crate::theme::{self, icon};
use crate::ui::{
    dialogs,
    pages::{page_title, settings_card, toggle},
    widgets::{self, icon_at},
};

const COLUMN_GAP: f32 = 20.0;

enum Health {
    Unchecked,
    Checking,
    Online(u64),
    Offline(String),
}

pub(super) struct State {
    pub capacity: u8,
    server_draft: Option<String>,
    health: Health,
    health_request: Option<Receiver<Result<u64, String>>>,
    lookup_request: Option<Receiver<Result<Preview, String>>>,
    preview: Option<Preview>,
    preview_server: String,
    activity: VecDeque<Activity>,
    qr: Option<(String, QrCode)>,
    remove_member: Option<(String, String)>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            capacity: 5,
            server_draft: None,
            health: Health::Unchecked,
            health_request: None,
            lookup_request: None,
            preview: None,
            preview_server: String::new(),
            activity: VecDeque::new(),
            qr: None,
            remove_member: None,
        }
    }
}

impl State {
    pub fn without_room(mut self) -> Self {
        self.preview = None;
        self.lookup_request = None;
        self.activity.clear();
        self.qr = None;
        self.remove_member = None;
        self
    }

    pub fn push_activity(&mut self, entry: Activity) {
        self.activity.push_front(entry);
        self.activity.truncate(8);
    }
}

fn request<T: Send + 'static>(app: &App, future: impl Future<Output = Result<T, String>> + Send + 'static) -> Receiver<Result<T, String>> {
    let (tx, rx) = bounded(1);
    let wake = app.backend.clone();
    app.backend.spawn(async move {
        let _ = tx.send(future.await);
        wake.wake();
    });
    rx
}

fn poll<T>(pending: &mut Option<Receiver<Result<T, String>>>) -> Option<Result<T, String>> {
    let result = match pending.as_ref()?.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => return None,
        Err(TryRecvError::Disconnected) => Err("The party request was interrupted. Try again.".into()),
    };
    *pending = None;
    Some(result)
}

fn test_server(app: &mut App) {
    if app.jam.view.health_request.is_some() {
        return;
    }
    let server = app.jam.server.clone();
    app.jam.view.health = Health::Checking;
    app.jam.view.health_request = Some(request(app, async move { jam::probe(&server).await }));
}

fn look_up(app: &mut App) {
    let Some(code) = invite_code(&app.jam.server, &app.jam.code_input) else {
        app.jam.error = Some("Enter a six-character party code or invite link.".into());
        return;
    };
    let server = app.jam.server.clone();
    app.jam.error = None;
    app.jam.view.preview_server = server.clone();
    app.jam.view.lookup_request = Some(request(app, async move { jam::preview(&server, &code).await }));
}

pub(super) fn page(app: &mut App, ui: &mut Ui) {
    if let Some(result) = poll(&mut app.jam.view.health_request) {
        app.jam.view.health = match result {
            Ok(ms) => Health::Online(ms),
            Err(error) => Health::Offline(error),
        };
    }
    if let Some(result) = poll(&mut app.jam.view.lookup_request) {
        match result {
            Ok(preview) => app.jam.view.preview = Some(preview),
            Err(error) => app.jam.error = Some(error),
        }
    }
    if matches!(app.jam.view.health, Health::Unchecked) && !app.jam.active() {
        test_server(app);
    }
    // The outer page frame already supplies the same margins as Library/Downloads.
    // Do not add a second centered column or another horizontal inset here.
    page_title(ui, "Listen Together");
    ui.add_space(4.0);
    secondary(ui, if app.jam.active() { "Everyone hears the same song at the same moment" } else { "Start a room or join friends with a six-character code" });
    ui.add_space(12.0);
    if app.jam.active() {
        room(app, ui);
    } else if app.jam.view.preview.is_some() {
        confirmation(app, ui);
    } else {
        landing(app, ui);
    }
}

fn card(ui: &mut Ui, body: impl FnOnce(&mut Ui)) {
    settings_card(ui, |ui| {
        // Use the shared card's fill, border, horizontal padding and themed radius.
        // A form needs the same vertical breathing room as a settings row's text.
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.add_space(10.0);
        body(ui);
        ui.add_space(10.0);
    });
}

fn secondary(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::regular(13.0)).color(theme::text_dim()));
}
fn small(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::regular(12.5)).color(theme::text_dim()));
}
fn heading(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(theme::bold(17.0)).color(theme::text()));
}

fn one_line(ui: &mut Ui, text: &str, size: f32, strong: bool) {
    ui.add(
        egui::Label::new(RichText::new(text).font(if strong { theme::semibold(size) } else { theme::regular(size) }).color(if strong { theme::text() } else { theme::text_dim() }))
            .truncate(),
    );
}

fn primary(ui: &mut Ui, label: &str, enabled: bool, busy: bool) -> bool {
    let text = if busy { "Please wait…" } else { label };
    let enabled = enabled && !busy && ui.is_enabled();
    ui.add_enabled_ui(enabled, |ui| {
        if !enabled {
            ui.multiply_opacity(0.45);
        }
        widgets::pill(ui, text, None, true).clicked()
    })
    .inner
}

fn text_button(ui: &mut Ui, label: &str, enabled: bool, color: Color32) -> bool {
    ui.add_enabled_ui(enabled, |ui| {
        if !enabled {
            ui.multiply_opacity(0.45);
        }
        widgets::pill_tinted(ui, label, None, false, Some(color)).clicked()
    })
    .inner
}

fn field(ui: &mut Ui, label: &str, value: &mut String, hint: &str, limit: usize) -> egui::Response {
    dialogs::text_field_limited(ui, label, value, false, hint, limit)
}

fn stepper(ui: &mut Ui, label: &str, name: &str, enabled: bool) -> bool {
    ui.add_enabled_ui(enabled, |ui| {
        if !enabled {
            ui.multiply_opacity(0.45);
        }
        let response = widgets::pill(ui, label, None, false).on_hover_text(name);
        response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, name));
        response.clicked()
    })
    .inner
}

fn avatar(ui: &mut Ui, url: Option<&str>, name: &str, size: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    ui.painter().circle_filled(rect.center(), size / 2.0, theme::glass(0.08));
    if let Some(initial) = name.chars().next() {
        ui.painter().text(rect.center(), Align2::CENTER_CENTER, initial.to_uppercase().collect::<String>(), theme::semibold(size * 0.38), theme::text_dim());
    } else {
        icon_at(ui, rect.center(), icon::USER, size * 0.5, theme::text_dim());
    }
    if let Some(url) = url.filter(|url| url.starts_with("https://") || url.starts_with("http://")) {
        egui::Image::new(url).corner_radius((size / 2.0) as u8).paint_at(ui, rect);
    }
}

fn notice(ui: &mut Ui, text: &str) {
    card(ui, |ui| {
        ui.label(RichText::new(text).font(theme::regular(13.0)).color(theme::RED));
    });
}

fn landing(app: &mut App, ui: &mut Ui) {
    let busy = app.jam.view.lookup_request.is_some();
    if ui.available_width() >= 620.0 {
        let column = (ui.available_width() - COLUMN_GAP) / 2.0;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = COLUMN_GAP;
            ui.allocate_ui_with_layout(vec2(column, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(column);
                start_card(app, ui, busy);
            });
            ui.allocate_ui_with_layout(vec2(column, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(column);
                join_card(app, ui, busy);
            });
        });
    } else {
        start_card(app, ui, busy);
        ui.add_space(COLUMN_GAP);
        join_card(app, ui, busy);
    }
    ui.add_space(COLUMN_GAP);
    if !app.logged_in {
        notice(ui, "Sign in to YouTube Music before creating or joining a party.");
        if text_button(ui, "Sign in to YouTube Music", true, app.accent) {
            app.start_sign_in(ui.ctx());
        }
        ui.add_space(10.0);
    }
    if let Some(error) = app.jam.error.clone() {
        notice(ui, &error);
        ui.add_space(COLUMN_GAP);
    }
    server_card(app, ui);
}

fn start_card(app: &mut App, ui: &mut Ui, busy: bool) {
    card(ui, |ui| {
        ui.set_min_height(240.0);
        ui.label(RichText::new("Start a party").font(theme::bold(21.0)).color(theme::text()));
        secondary(ui, "Choose how friends see you and how many can join.");
        ui.add_space(20.0);
        ui.horizontal(|ui| {
            avatar(ui, app.account_avatar.as_deref(), app.account_name.as_deref().unwrap_or(""), 44.0);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                let hint = app.account_name.as_deref().unwrap_or("Nickname");
                field(ui, "Nickname", &mut app.jam.name, hint, 80);
            });
        });
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            let text_width = (ui.available_width() - 164.0).max(70.0);
            ui.allocate_ui_with_layout(vec2(text_width, 36.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(text_width);
                ui.label(RichText::new("Party size").font(theme::semibold(13.5)));
                small(ui, "2–10 people");
            });
            if stepper(ui, "−", "Decrease party size", app.logged_in && !busy && app.jam.view.capacity > 2) {
                app.jam.view.capacity -= 1;
            }
            ui.add_sized(vec2(25.0, 32.0), egui::Label::new(RichText::new(app.jam.view.capacity.to_string()).font(theme::semibold(14.0))));
            if stepper(ui, "+", "Increase party size", app.logged_in && !busy && app.jam.view.capacity < 10) {
                app.jam.view.capacity += 1;
            }
        });
        ui.add_space(16.0);
        if primary(ui, "Start party", app.logged_in, busy) {
            start(app);
        }
    });
}

fn join_card(app: &mut App, ui: &mut Ui, busy: bool) {
    card(ui, |ui| {
        ui.set_min_height(240.0);
        ui.label(RichText::new("Join a party").font(theme::bold(21.0)).color(theme::text()));
        secondary(ui, "Enter the code first. You’ll see who is inside before joining.");
        ui.add_space(20.0);
        let response = field(ui, "Party code", &mut app.jam.code_input, "ABC123 or invite link", 512);
        if response.changed() {
            app.jam.error = None;
            if let Some(code) = invite_code(&app.jam.server, &app.jam.code_input) {
                app.jam.code_input = code;
            }
        }
        ui.add_space(18.0);
        let enabled = app.logged_in && invite_code(&app.jam.server, &app.jam.code_input).is_some();
        let enter = response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
        if primary(ui, "Look up party", enabled, busy) || (enabled && !busy && enter) {
            look_up(app);
        }
    });
}

fn server_card(app: &mut App, ui: &mut Ui) {
    if app.jam.view.server_draft.is_none() {
        app.jam.view.server_draft = Some(if app.jam.server == jam::DEFAULT_SERVER { String::new() } else { app.jam.server.clone() });
    }
    card(ui, |ui| {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(20.0, 28.0), Sense::hover());
            icon_at(ui, rect.center(), icon::SETTINGS, 20.0, theme::text_dim());
            ui.vertical(|ui| {
                ui.label(RichText::new("Party server").font(theme::semibold(14.0)));
                small(ui, "Leave blank to use the server included with this build.");
            });
        });
        ui.add_space(12.0);
        field(ui, "Server address", app.jam.view.server_draft.as_mut().unwrap(), "Custom server address", 512);
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            let (status, error) = match &app.jam.view.health {
                Health::Unchecked => ("Server not checked".to_owned(), false),
                Health::Checking => ("Checking the party server…".to_owned(), false),
                Health::Online(ms) => (format!("Party server online · {ms} ms"), false),
                Health::Offline(error) => (error.clone(), true),
            };
            let status_width = (ui.available_width() - 220.0).max(50.0);
            ui.allocate_ui_with_layout(vec2(status_width, 38.0), Layout::left_to_right(Align::Center), |ui| {
                ui.set_width(status_width);
                ui.add(egui::Label::new(RichText::new(status).font(theme::regular(12.0)).color(if error { theme::RED } else { theme::text_dim() })).truncate());
            });
            if text_button(ui, "Test", app.jam.view.health_request.is_none(), app.accent) {
                test_server(app);
            }
            let blank = app.jam.view.server_draft.as_deref().unwrap_or("").trim().is_empty();
            if text_button(ui, if blank { "Use built-in" } else { "Save" }, true, theme::text()) {
                let draft = app.jam.view.server_draft.as_deref().unwrap_or("");
                match jam::normalize_server(if blank { jam::DEFAULT_SERVER } else { draft }) {
                    Ok(server) => {
                        app.jam.server = server;
                        app.jam.error = None;
                        app.jam.view.lookup_request = None;
                        app.jam.view.health_request = None;
                        test_server(app);
                    }
                    Err(error) => app.jam.error = Some(error.into()),
                }
            }
        });
        small(ui, "Both devices must use the same server. Test checks the saved address; Save applies edits.");
    });
}

fn confirmation(app: &mut App, ui: &mut Ui) {
    let Some(preview) = app.jam.view.preview.clone() else { return };
    ui.add_space(30.0);
    ui.vertical_centered(|ui| {
        let (rect, _) = ui.allocate_exact_size(vec2(54.0, 54.0), Sense::hover());
        icon_at(ui, rect.center(), icon::USERS, 48.0, app.accent);
        ui.add_space(14.0);
        let title = if preview.host_name.is_empty() { "Join this party?".into() } else { format!("Join {}’s party?", preview.host_name) };
        ui.label(RichText::new(title).font(theme::bold(24.0)));
        secondary(ui, &format!("{} of {} people · {}", preview.member_count, preview.max_members, preview.code));
        ui.add_space(20.0);
        let count = preview.members.len().min(5);
        let width = count as f32 * 52.0;
        ui.allocate_ui_with_layout(vec2(width, 50.0), Layout::left_to_right(Align::Center), |ui| {
            for member in preview.members.iter().take(5) {
                avatar(ui, member.avatar_url.as_deref(), &member.display_name, 44.0);
            }
        });
        ui.add_space(24.0);
        let width = ui.available_width().min(360.0);
        ui.allocate_ui_with_layout(vec2(width, 0.0), Layout::top_down(Align::Min), |ui| {
            field(ui, "Nickname", &mut app.jam.name, app.account_name.as_deref().unwrap_or("Nickname"), 80);
            if let Some(error) = &app.jam.error {
                ui.label(RichText::new(error).color(theme::RED));
            }
            if preview.is_full {
                notice(ui, "This party is full.");
            }
            ui.add_space(20.0);
            if primary(ui, "Join party", app.logged_in && !preview.is_full, false) {
                // The preview belongs to this explicitly chosen endpoint. Looking up
                // a party did not allocate a slot or send the user's profile.
                if app.jam.server == app.jam.view.preview_server {
                    join(app, &preview.code);
                } else {
                    app.jam.error = Some("The server changed. Look up the party again.".into());
                }
            }
            if text_button(ui, "Back", true, theme::text_dim()) {
                app.jam.view.preview = None;
                app.jam.error = None;
            }
        });
    });
}

fn room(app: &mut App, ui: &mut Ui) {
    if ui.available_width() >= 660.0 {
        let width = ui.available_width() - COLUMN_GAP;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = COLUMN_GAP;
            ui.allocate_ui_with_layout(vec2(width * 0.575, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(width * 0.575);
                room_music(app, ui);
            });
            ui.allocate_ui_with_layout(vec2(width * 0.425, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(width * 0.425);
                room_people(app, ui);
            });
        });
    } else {
        room_music(app, ui);
        ui.add_space(COLUMN_GAP);
        room_people(app, ui);
    }
}

fn room_music(app: &mut App, ui: &mut Ui) {
    hero(app, ui);
    ui.add_space(16.0);
    now_playing(app, ui);
    ui.add_space(16.0);
    queue(app, ui);
}

fn hero(app: &mut App, ui: &mut Ui) {
    let invite = jam::share_link(&app.jam.server, &app.jam.code);
    if !app.jam.code.is_empty() && app.jam.view.qr.as_ref().is_none_or(|(url, _)| url != &invite) {
        app.jam.view.qr = QrCode::new(invite.as_bytes()).ok().map(|qr| (invite.clone(), qr));
    }
    card(ui, |ui| {
        ui.horizontal_top(|ui| {
            let text_width = (ui.available_width() - 124.0).max(120.0);
            ui.allocate_ui_with_layout(vec2(text_width, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(text_width);
                heading(ui, "Party code");
                let code = if app.jam.code.is_empty() { "…".to_owned() } else { app.jam.code.chars().map(|c| c.to_string()).collect::<Vec<_>>().join(" ") };
                ui.label(RichText::new(code).font(theme::bold(28.0)));
                let status = if app.jam.reconnecting {
                    "Reconnecting…".into()
                } else if !app.jam.welcomed {
                    "Connecting…".into()
                } else if let Some(ms) = app.jam.conn.as_ref().and_then(|conn| conn.clock.round_trip_ms()) {
                    format!("In sync · {ms} ms")
                } else {
                    "Syncing clocks…".into()
                };
                small(ui, &status);
                ui.add_space(6.0);
                small(ui, "Scan from a phone or copy the invite link");
            });
            if let Some((_, qr)) = &app.jam.view.qr {
                draw_qr(ui, qr);
            }
        });
        ui.horizontal_wrapped(|ui| {
            if text_button(ui, "Copy invite", !app.jam.code.is_empty(), theme::text()) {
                app.share_pending = Some(invite);
                app.toast("Invite copied");
            }
            if text_button(ui, "Copy code", !app.jam.code.is_empty(), theme::text()) {
                app.share_pending = Some(app.jam.code.clone());
                app.toast("Code copied");
            }
        });
    });
}

fn draw_qr(ui: &mut Ui, qr: &QrCode) {
    let modules = qr.width() + 8;
    let scale = (112.0 / modules as f32).floor().max(1.0);
    let side = modules as f32 * scale;
    let (rect, _) = ui.allocate_exact_size(vec2(side, side), Sense::hover());
    ui.painter().rect_filled(rect, 4, Color32::WHITE);
    for y in 0..qr.width() {
        for x in 0..qr.width() {
            if qr[(x, y)] == Color::Dark {
                let min = rect.min + vec2((x + 4) as f32 * scale, (y + 4) as f32 * scale);
                ui.painter().rect_filled(Rect::from_min_size(min, vec2(scale, scale)), 0, Color32::BLACK);
            }
        }
    }
}

fn now_playing(app: &App, ui: &mut Ui) {
    card(ui, |ui| {
        heading(ui, "Now playing");
        ui.add_space(10.0);
        if let Some(track) = app.jam.playback.as_ref().and_then(|p| p.track.as_ref()) {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(62.0, 62.0), Sense::hover());
                ui.painter().rect_filled(rect, theme::thumb_radius(), theme::glass(0.07));
                icon_at(ui, rect.center(), icon::MUSIC, 24.0, theme::text_dim());
                if let Some(url) = &track.thumbnail_url {
                    egui::Image::new(url).corner_radius(theme::thumb_radius() as u8).paint_at(ui, rect);
                }
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    one_line(ui, &track.title, 14.0, true);
                    one_line(ui, &track.artist, 13.0, false);
                    if let Some(name) = app.jam.playback.as_ref().and_then(|p| p.started_by_name.as_ref()) {
                        small(ui, &format!("Started by {name}"));
                    }
                });
            });
        } else {
            secondary(ui, "Nothing playing yet");
            if app.jam.is_host() {
                small(ui, "Play a song and it will appear for everyone.");
            }
        }
    });
}

fn queue(app: &App, ui: &mut Ui) {
    card(ui, |ui| {
        heading(ui, "Up next");
        ui.add_space(8.0);
        let start = app.jam.playback.as_ref().map_or(0, |p| (p.queue_index + 1).max(0) as usize);
        let upcoming = app.jam.queue.items.iter().skip(start).take(5).collect::<Vec<_>>();
        if upcoming.is_empty() {
            secondary(ui, "The shared queue is empty");
        }
        for (index, track) in upcoming.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.add_sized(vec2(24.0, 38.0), egui::Label::new(RichText::new((index + 1).to_string()).color(theme::text_dim())));
                ui.vertical(|ui| {
                    one_line(ui, &track.title, 13.5, true);
                    one_line(ui, &track.artist, 12.0, false);
                });
            });
            if index + 1 < upcoming.len() {
                ui.add_space(8.0);
            }
        }
    });
}

fn room_people(app: &mut App, ui: &mut Ui) {
    members(app, ui);
    if app.jam.is_host() {
        ui.add_space(16.0);
        host_controls(app, ui);
    }
    if !app.jam.view.activity.is_empty() {
        ui.add_space(16.0);
        activity(app, ui);
    }
    if let Some(error) = app.jam.error.clone() {
        ui.add_space(16.0);
        notice(ui, &error);
    }
    ui.add_space(10.0);
    if text_button(ui, "Leave party", true, theme::RED) {
        leave(app);
    }
}

fn members(app: &mut App, ui: &mut Ui) {
    card(ui, |ui| {
        ui.horizontal(|ui| {
            heading(ui, "Listening");
            small(ui, &format!("{}/{}", app.jam.member_count(), app.jam.max_members));
        });
        ui.add_space(8.0);
        let mut remove = None;
        for member in &app.jam.members {
            ui.horizontal(|ui| {
                avatar(ui, member.avatar_url.as_deref(), &member.display_name, 34.0);
                let can_remove = app.jam.is_host() && !member.is_host;
                let width = (ui.available_width() - if can_remove { 56.0 } else { 0.0 }).max(50.0);
                ui.allocate_ui_with_layout(vec2(width, 38.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(width);
                    let name =
                        format!("{}{}", if member.display_name.is_empty() { "Someone" } else { &member.display_name }, if member.member_id == app.jam.you { " · You" } else { "" });
                    one_line(ui, &name, 13.5, true);
                    small(
                        ui,
                        if member.is_host {
                            "Host"
                        } else if member.connected {
                            "Connected"
                        } else {
                            "Away"
                        },
                    );
                });
                if can_remove && stepper(ui, "−", &format!("Remove {}", member.display_name), app.jam.welcomed && !app.jam.reconnecting) {
                    remove = Some((member.member_id.clone(), member.display_name.clone()));
                }
            });
            ui.add_space(8.0);
        }
        if let Some(remove) = remove {
            app.jam.view.remove_member = Some(remove);
        }
        if let Some((id, name)) = app.jam.view.remove_member.clone() {
            secondary(ui, &format!("Remove {name} from the party?"));
            ui.horizontal(|ui| {
                if text_button(ui, "Remove", true, theme::RED) {
                    control(app, "kick", json!({"memberId": id}));
                    app.jam.view.remove_member = None;
                }
                if text_button(ui, "Cancel", true, theme::text_dim()) {
                    app.jam.view.remove_member = None;
                }
            });
        }
    });
}

fn host_controls(app: &mut App, ui: &mut Ui) {
    card(ui, |ui| {
        heading(ui, "Host controls");
        ui.add_space(12.0);
        let mut host_only = app.jam.host_only;
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 54.0).max(80.0);
            ui.allocate_ui_with_layout(vec2(width, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(width);
                ui.label(RichText::new("Only I can control playback").font(theme::semibold(13.0)));
                small(ui, "Restricts shared playback and queue edits.");
            });
            ui.add_enabled_ui(app.jam.welcomed && !app.jam.reconnecting, |ui| {
                if toggle(ui, &mut host_only, "Only I can control playback").changed() {
                    control(app, "setHostOnlyControl", json!({"enabled": host_only}));
                }
            });
        });
        ui.add_space(20.0);
        ui.label(RichText::new("Party size").font(theme::semibold(13.0)));
        small(ui, "Never removes people already inside.");
        ui.horizontal(|ui| {
            let live = app.jam.welcomed && !app.jam.reconnecting;
            let minimum = app.jam.member_count().max(2);
            if stepper(ui, "−", "Decrease party capacity", live && app.jam.max_members as usize > minimum) {
                control(app, "setMaxMembers", json!({"maxMembers": app.jam.max_members - 1}));
            }
            ui.add_sized(vec2(30.0, 32.0), egui::Label::new(app.jam.max_members.to_string()));
            if stepper(ui, "+", "Increase party capacity", live && app.jam.max_members < 10) {
                control(app, "setMaxMembers", json!({"maxMembers": app.jam.max_members + 1}));
            }
        });
    });
}

fn activity(app: &App, ui: &mut Ui) {
    card(ui, |ui| {
        heading(ui, "Activity");
        ui.add_space(8.0);
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64;
        for entry in &app.jam.view.activity {
            let seconds = now.saturating_sub(entry.at_ms).max(0) / 1000;
            let age = if seconds < 60 { "now".to_owned() } else { format!("{}m", seconds / 60) };
            let detail = if entry.detail.is_empty() { &entry.action } else { &entry.detail };
            let text = format!("{age}  {}: {detail}", entry.by);
            ui.add(egui::Label::new(RichText::new(&text).font(theme::regular(11.5)).color(theme::text_dim())).truncate()).on_hover_text(text);
            ui.add_space(3.0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaving_preserves_preferences_but_clears_room_ui() {
        let mut state = State { capacity: 8, server_draft: Some("https://party.example.com".into()), ..Default::default() };
        for at_ms in 0..12 {
            state.push_activity(Activity { action: "play".into(), by: "Ada".into(), at_ms, detail: String::new() });
        }
        assert_eq!(state.activity.len(), 8);
        assert_eq!(state.activity.front().unwrap().at_ms, 11);
        state.remove_member = Some(("member".into(), "Ada".into()));
        let state = state.without_room();
        assert_eq!(state.capacity, 8);
        assert_eq!(state.server_draft.as_deref(), Some("https://party.example.com"));
        assert!(state.activity.is_empty());
        assert!(state.remove_member.is_none());
        assert!(state.preview.is_none());
    }
}
