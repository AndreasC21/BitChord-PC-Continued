//! UI and audio reconciliation for the official, server-authoritative party protocol.
//! Every participant (including the host) follows server state. Inbound audio changes
//! go straight to AudioCmd, never through the outbound user-action path.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use egui::Ui;
use serde_json::{Value, json};

use super::{Action, App, Page};
use crate::audio::AudioCmd;
use crate::jam::protocol::{Member, PartyQueue, PartyTrack, Playback};
use crate::jam::{Event, Frame, Identity, Jam};
use crate::model::{Link, Target, Thumb, Track};

#[path = "jam/view.rs"]
mod view;

const MAX_UPCOMING: usize = 25;
const MAX_DRIFT_MS: u64 = 750;

#[derive(Default)]
pub struct Session {
    conn: Option<Jam>,
    created: bool,
    welcomed: bool,
    reconnecting: bool,
    code: String,
    you: String,
    host_only: bool,
    members: Vec<Member>,
    max_members: u8,
    view: view::State,
    playback: Option<Playback>,
    queue: PartyQueue,
    queue_requested: Option<u64>,
    initial: Option<(Vec<Track>, usize, u64, bool)>,
    loaded: Option<String>,
    reported: Option<String>,
    load_started: Option<Instant>,
    load_ms: u64,
    last_sync: Option<Instant>,
    drift_strikes: u8,
    finished_seq: Option<u64>,
    code_input: String,
    pub name: String,
    pub server: String,
    pub device_id: String,
    error: Option<String>,
    /// Current-track attribution, used by the existing queue UI.
    pub by: HashMap<String, String>,
    script_done: usize,
}

impl Session {
    pub fn active(&self) -> bool {
        self.conn.is_some()
    }

    pub fn is_host(&self) -> bool {
        self.members.iter().any(|m| m.member_id == self.you && m.is_host)
    }

    fn can_control(&self) -> bool {
        !self.host_only || self.is_host()
    }

    pub fn member_count(&self) -> usize {
        self.members.len()
    }
}

fn to_party(track: &Track) -> PartyTrack {
    PartyTrack {
        video_id: track.id.clone(),
        title: track.title.clone(),
        artist: track.artist_line(),
        thumbnail_url: crate::model::pick(&track.thumbs, 300).map(str::to_owned),
        duration_ms: track.duration.map(|d| d as u64 * 1000),
        from_autoplay: false,
    }
}

fn from_party(track: &PartyTrack) -> Track {
    Track {
        id: track.video_id.clone(),
        title: track.title.clone(),
        artists: vec![Link { name: track.artist.clone(), id: None }],
        album: None,
        duration: track.duration_ms.map(|d| (d / 1000).min(u32::MAX as u64) as u32),
        thumbs: track.thumbnail_url.iter().map(|url| Thumb { url: url.clone(), width: 300, height: 300 }).collect(),
        track_nr: None,
        plays: None,
        music_video: false,
    }
}

/// Preserve preferences, not membership credentials, when a party ends.
fn fresh(app: &mut App) {
    let old = &mut app.jam;
    let max_members = old.view.capacity;
    app.jam = Session {
        name: std::mem::take(&mut old.name),
        server: std::mem::take(&mut old.server),
        device_id: std::mem::take(&mut old.device_id),
        code_input: std::mem::take(&mut old.code_input),
        max_members,
        view: std::mem::take(&mut old.view).without_room(),
        load_ms: 500,
        ..Default::default()
    };
}

pub fn start(app: &mut App) {
    enter(app, None);
}

pub fn join(app: &mut App, value: &str) {
    // Public official links are accepted in the input field too. For self-hosted
    // parties, set the server explicitly; pasted links never silently change it.
    let Some(code) = invite_code(&app.jam.server, value) else {
        app.jam.error = Some("Enter a six-character party code or an official invite link.".into());
        return;
    };
    if app.jam.active() && app.jam.code == code {
        return;
    }
    enter(app, Some(code));
}

fn invite_code(server: &str, value: &str) -> Option<String> {
    let custom = crate::jam::normalize_server(server).ok().and_then(|server| {
        let url = url::Url::parse(value.trim()).ok()?;
        let base = format!("{server}/invite/");
        let candidate = url.as_str().strip_prefix(&base)?.split(['?', '#']).next()?;
        (candidate.len() == 6 && candidate.chars().all(|c| c.is_ascii_alphanumeric())).then(|| crate::jam::normalize_code(candidate)).flatten()
    });
    crate::deeplink::parse(value).or(custom).or_else(|| crate::jam::normalize_code(value))
}

fn enter(app: &mut App, code: Option<String>) {
    if !app.logged_in {
        app.jam.error = Some("Sign in to YouTube Music before joining a party.".into());
        return;
    }
    let server = match crate::jam::normalize_server(&app.jam.server) {
        Ok(server) => server,
        Err(error) => {
            app.jam.error = Some(error.into());
            return;
        }
    };
    leave(app);
    app.jam.server = server.clone();
    app.jam.created = code.is_none();
    // Device IDs are persistent but not credentials. No Google session data leaves
    // the account layer. The official server accepts a client-asserted user ID;
    // this client uses an installation-scoped ID (not a verified Google identity).
    if app.jam.device_id.is_empty() {
        use std::hash::{BuildHasher, Hasher};
        let mut hash = std::collections::hash_map::RandomState::new().build_hasher();
        hash.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        app.jam.device_id = format!("desktop-{:016x}", hash.finish());
    }
    let name = [&app.jam.name, app.account_name.as_deref().unwrap_or("")]
        .into_iter()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or("BitChord desktop")
        .chars()
        .scan(0, |bytes, ch| {
            *bytes += ch.len_utf8();
            (*bytes <= 80).then_some(ch)
        })
        .collect::<String>();
    let identity = Identity { user_id: app.jam.device_id.clone(), device_id: app.jam.device_id.clone(), display_name: name, avatar_url: app.account_avatar.clone() };
    if app.jam.created
        && let Some(index) = app.queue.index
    {
        let tracks = app.queue.tracks.iter().skip(index).take(MAX_UPCOMING + 1).cloned().collect();
        app.jam.initial = Some((tracks, 0, app.audio.status.position().as_millis() as u64, app.playing()));
    }
    app.jam.conn = Some(Jam::start_with_capacity(&app.backend, server, code, identity, app.jam.view.capacity));
    // Disable local-only timing changes without overwriting the saved preferences.
    app.handoff = false;
    app.pending_mix = None;
    app.mix_from = None;
    app.queue.fill = None;
    app.play_pending = None;
    app.set_crossfade();
    // Cancel old stream resolutions too. Otherwise a late local Play response can
    // start the wrong song after joining a party that is paused.
    let position = app.resume_at.unwrap_or_else(|| app.audio.status.position());
    app.generation += 1;
    app.audio.status.wanted_gen.store(app.generation, Ordering::Relaxed);
    app.audio.send(AudioCmd::Stop);
    app.buffering = false;
    app.playing_as = None;
    if app.queue.index.is_some() {
        app.resume_at = Some(position);
    }
}

/// Leaving as host transfers ownership; it does not end the party for mobile users.
pub fn leave(app: &mut App) {
    if let Some(conn) = app.jam.conn.take() {
        conn.leave();
    }
    fresh(app);
    app.set_crossfade();
}

pub fn dev_hooks(app: &mut App) {
    if super::dev_var("YTM_JAM_HOST").is_some() {
        app.go(Page::Jam);
        start(app);
    } else if let Some(code) = super::dev_var("YTM_JAM_JOIN") {
        app.go(Page::Jam);
        join(app, code);
    }
}

pub fn dev_script(app: &mut App, ctx: &egui::Context, now: f64) {
    let Some(script) = super::dev_var("YTM_JAM_SCRIPT") else { return };
    for (i, step) in script.split(',').enumerate() {
        let Some((time, action)) = step.split_once(':') else { continue };
        let Ok(time) = time.parse::<f64>() else { continue };
        if i < app.jam.script_done {
            continue;
        }
        if now < time {
            ctx.request_repaint_after(Duration::from_secs_f64(time - now));
            break;
        }
        app.jam.script_done = i + 1;
        match action {
            "skip" => app.next(),
            "prev" => app.prev(),
            "toggle" => app.toggle(),
            "control=on" => control(app, "setHostOnlyControl", json!({"enabled": false})),
            "control=off" => control(app, "setHostOnlyControl", json!({"enabled": true})),
            action if action.starts_with("seek=") => {
                if let Ok(seconds) = action[5..].parse() {
                    app.apply(Action::Seek(seconds));
                }
            }
            action if action.starts_with("add=") => {
                app.apply(Action::Enqueue(from_party(&PartyTrack { video_id: action[4..].into(), title: "Test song".into(), ..Default::default() })));
            }
            _ => {}
        }
    }
}

pub fn open_link(app: &mut App, code: String) {
    // Never auto-join from an external link: show the code and server for confirmation.
    app.go(Page::Jam);
    app.jam.code_input = code;
}

pub fn tick(app: &mut App) {
    let Some(conn) = &app.jam.conn else { return };
    let mut events = Vec::new();
    while let Some(event) = conn.try_recv() {
        events.push(event);
    }
    for event in events {
        on_event(app, event);
    }
    if app.jam.welcomed && !app.jam.reconnecting {
        sync(app);
    }
}

fn on_event(app: &mut App, event: Event) {
    match event {
        Event::Joined(membership) => {
            app.jam.code = membership.code;
            app.jam.you = membership.you.member_id;
            app.jam.members = membership.party.members;
            app.jam.max_members = membership.party.max_members;
        }
        Event::Frame(Frame::Welcome { you, party, .. }) => {
            let first = !app.jam.welcomed;
            app.jam.welcomed = true;
            app.jam.reconnecting = false;
            app.jam.you = you.member_id;
            app.jam.host_only = party.host_only_control;
            app.jam.members = party.members;
            app.jam.max_members = party.max_members;
            app.jam.queue = party.queue;
            app.jam.queue_requested = None;
            app.jam.playback = Some(party.playback);
            // Seed a newly created room exactly once; a reconnect must never replace
            // another device's changes with a desktop queue snapshot.
            if first && let Some((tracks, index, position, playing)) = app.jam.initial.take() {
                publish_queue(app, &tracks, index, position, playing);
                app.jam.playback = None;
            }
            if first {
                app.toast(if app.jam.created { "Party created: share the code with mobile or PC" } else { "Joined the party" });
            }
        }
        Event::Frame(Frame::State { playback }) => {
            if app.jam.playback.as_ref().is_none_or(|old| playback.seq > old.seq) {
                app.jam.playback = Some(playback);
            }
        }
        Event::Frame(Frame::Queue { queue }) => {
            if queue.seq >= app.jam.queue.seq {
                app.jam.queue = queue;
                app.jam.queue_requested = None;
            }
        }
        Event::Frame(Frame::Members { members, max_members, host_only_control }) => {
            app.jam.members = members;
            app.jam.max_members = max_members;
            app.jam.host_only = host_only_control;
        }
        Event::Frame(Frame::Activity { entry }) => app.jam.view.push_activity(entry),
        Event::Frame(Frame::Error { error }) => {
            app.toast(crate::jam::protocol::error_message(&error, 0));
            // The rejected local action did not change playback; ask for truth again.
            if let Some(conn) = &app.jam.conn {
                conn.send(json!({"type": "sync"}));
            }
        }
        Event::Frame(_) => {}
        Event::Reconnecting => app.jam.reconnecting = true,
        Event::Closed(message) => {
            leave(app);
            app.jam.error = Some(message.clone());
            app.toast(message);
        }
    }
}

fn sync(app: &mut App) {
    let Some(playback) = app.jam.playback.clone() else { return };
    let Some(conn) = &app.jam.conn else { return };
    if playback.queue_seq != app.jam.queue.seq {
        if app.jam.queue_requested != Some(playback.queue_seq) {
            conn.send(json!({"type": "syncQueue"}));
            app.jam.queue_requested = Some(playback.queue_seq);
        }
        return;
    }
    let now = conn.clock.server_ms();
    let clock_ready = conn.clock.ready();
    let target = Duration::from_millis(if clock_ready { playback.position_at(now) } else { playback.effective_position_ms });
    let Some(track) = &playback.track else {
        if app.jam.loaded.is_some() || app.queue.index.is_some() {
            // Invalidate any old stream resolution before clearing local playback.
            app.generation += 1;
            app.audio.status.wanted_gen.store(app.generation, Ordering::Relaxed);
            app.audio.send(AudioCmd::Stop);
            app.buffering = false;
            app.resume_at = None;
            app.queue.tracks.clear();
            app.queue.index = None;
            app.jam.loaded = None;
        }
        return;
    };
    // Prefer the authoritative index, not the first matching ID: queues may repeat songs.
    let index = usize::try_from(playback.queue_index).ok().filter(|i| app.jam.queue.items.get(*i).is_some_and(|t| t.video_id == track.video_id));
    let index = index.or_else(|| app.jam.queue.items.iter().position(|t| t.video_id == track.video_id));
    app.queue.tracks = app.jam.queue.items.iter().map(from_party).collect();
    let index = index.unwrap_or_else(|| {
        app.queue.tracks.push(from_party(track));
        app.queue.tracks.len() - 1
    });
    app.queue.index = Some(index);
    app.queue.fill = None;
    app.queue_source = None;
    app.jam.by.clear();
    if let Some(name) = &playback.started_by_name {
        app.jam.by.insert(track.video_id.clone(), name.clone());
    }
    if app.jam.finished_seq == Some(playback.seq) {
        return;
    }
    let lead = if clock_ready { playback.lead_ms(now) } else { 0 };
    // A seed from welcome is only a rough estimate. Do not start before the first
    // actual ping/pong sample, which could put this device ahead of a future anchor.
    let should_play = clock_ready && playback.is_playing && lead == 0;
    if app.jam.loaded.as_deref() != Some(track.video_id.as_str()) || app.resume_at.is_some() {
        if should_play {
            let report = app.jam.reported.as_deref() != Some(track.video_id.as_str());
            app.jam.reported = Some(track.video_id.clone());
            app.jam.loaded = Some(track.video_id.clone());
            app.jam.load_started = Some(Instant::now());
            app.play_current_at(target + Duration::from_millis(app.jam.load_ms), report);
        } else if app.resume_at != Some(target) || app.jam.loaded.as_deref() != Some(track.video_id.as_str()) {
            app.generation += 1;
            app.audio.status.wanted_gen.store(app.generation, Ordering::Relaxed);
            app.audio.send(AudioCmd::Stop);
            app.jam.loaded = Some(track.video_id.clone());
            app.jam.load_started = None;
            app.playing_as = None;
            app.resume_at = Some(target);
            app.buffering = false;
            app.audio.status.position_ms.store(target.as_millis() as u64, Ordering::Relaxed);
            app.audio.status.duration_ms.store(track.duration_ms.unwrap_or(0), Ordering::Relaxed);
            app.request_art();
            let shown = from_party(track);
            app.show_in_media(&shown);
            app.refresh_lyrics(shown);
        }
        return;
    }
    if app.buffering {
        return;
    }
    if let Some(started) = app.jam.load_started.take() {
        app.jam.load_ms = ((app.jam.load_ms + started.elapsed().as_millis().min(8_000) as u64) / 2).max(100);
        app.jam.last_sync = None;
    }
    if app.playing() != should_play {
        app.audio.send(if should_play { AudioCmd::Resume } else { AudioCmd::Pause });
    }
    if !clock_ready || app.jam.last_sync.is_some_and(|last| last.elapsed() < Duration::from_secs(2)) {
        return;
    }
    let drift = app.audio.status.position_smooth().as_millis().abs_diff(target.as_millis());
    app.jam.drift_strikes = if drift > MAX_DRIFT_MS as u128 { app.jam.drift_strikes.saturating_add(1) } else { 0 };
    if app.jam.drift_strikes >= 3 {
        app.audio.send(if should_play { AudioCmd::SeekLive { pos: target, at: Instant::now() } } else { AudioCmd::Seek(target) });
        app.jam.last_sync = Some(Instant::now());
        app.jam.drift_strikes = 0;
    }
}

pub fn control(app: &mut App, action: &str, body: Value) {
    if !app.jam.welcomed || app.jam.reconnecting {
        app.toast("Wait for the party connection before changing playback");
        return;
    }
    if !app.jam.can_control() && action != "setHostOnlyControl" {
        app.toast("Only the host can control this party");
        return;
    }
    if serde_json::to_vec(&body).map_or(true, |bytes| bytes.len() > 15_000) {
        app.toast("This track's metadata is too large for the party server");
        return;
    }
    if let Some(conn) = &app.jam.conn {
        conn.control(action, body);
    }
}

pub fn toggle_playback(app: &mut App) {
    let playing = app.jam.playback.as_ref().is_some_and(|p| p.is_playing);
    set_playing(app, !playing);
}

pub fn set_playing(app: &mut App, playing: bool) {
    control(app, if playing { "play" } else { "pause" }, json!({}));
}

/// Only the host advances on decoder completion. A guest's shorter alternate source
/// or an unplayable local stream must never skip the song on everybody else's device.
pub fn finished(app: &mut App) {
    app.jam.finished_seq = app.jam.playback.as_ref().map(|p| p.seq);
    if app.jam.is_host() && app.jam.can_control() {
        control(app, "next", json!({}));
    }
}

pub fn playback_failed(app: &mut App) {
    app.jam.finished_seq = app.jam.playback.as_ref().map(|p| p.seq);
    app.jam.loaded = None;
    app.jam.load_started = None;
}

fn publish_queue(app: &mut App, tracks: &[Track], index: usize, position: u64, playing: bool) {
    let Some(track) = tracks.get(index) else { return };
    // Bound both frames well below the official server's 16 KiB frame limit.
    let mut queue = Vec::new();
    for track in tracks.iter().skip(index).take(MAX_UPCOMING + 1) {
        let track = to_party(track);
        let size = serde_json::to_vec(&track).map_or(16_384, |v| v.len());
        let used: usize = queue.iter().map(|t| serde_json::to_vec(t).map_or(16_384, |v| v.len())).sum();
        if used + size > 12_000 {
            break;
        }
        queue.push(track);
    }
    if queue.is_empty() {
        app.toast("This track's metadata is too large to share");
        return;
    }
    control(app, "setQueue", json!({"queue": queue, "queueIndex": 0}));
    control(app, "setTrack", json!({"track": to_party(track), "queueIndex": 0, "positionMs": position, "isPlaying": playing}));
}

/// Intercept all participants' shared actions, not just guests'. The server echoes
/// each accepted command back to its sender as well as to the other devices.
pub fn party_action(app: &mut App, action: Action) -> Option<Action> {
    match action {
        Action::Play(tracks, index) => {
            if tracks.get(index).is_some_and(|t| app.current_id() == Some(t.id.as_str())) {
                toggle_playback(app);
            } else {
                publish_queue(app, &tracks, index, 0, true);
            }
        }
        Action::Open(Target::Song(track)) | Action::Radio(track) => control(app, "setTrack", json!({"track": to_party(&track), "positionMs": 0, "isPlaying": true})),
        Action::Enqueue(track) => control(app, "queueAdd", json!({"tracks": [to_party(&track)], "playNext": false})),
        Action::PlayNext(track) => control(app, "queueAdd", json!({"tracks": [to_party(&track)], "playNext": true})),
        Action::QueueJump(index) => {
            if let Some(track) = app.jam.queue.items.get(index).cloned() {
                control(app, "setTrack", json!({"track": track, "queueIndex": index, "positionMs": 0, "isPlaying": true}));
            }
        }
        Action::QueueRemove(index) => control(app, "queueRemove", json!({"index": index})),
        Action::Seek(seconds) => control(app, "seek", json!({"positionMs": (seconds.max(0.0) as f64 * 1000.0) as u64})),
        Action::TogglePlay => toggle_playback(app),
        Action::PlayCollection(..) => app.toast("In a party, open the playlist and select its songs to share"),
        action => return Some(action),
    }
    None
}

pub fn page(app: &mut App, ui: &mut Ui) {
    view::page(app, ui);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_conversion_preserves_video_and_duration_units() {
        let wire = PartyTrack {
            video_id: "video".into(),
            title: "Song".into(),
            artist: "Artist".into(),
            duration_ms: Some(123_000),
            thumbnail_url: Some("https://example.com/art.jpg".into()),
            ..Default::default()
        };
        let track = from_party(&wire);
        assert_eq!(track.duration, Some(123));
        assert_eq!(to_party(&track), wire);
    }
}
