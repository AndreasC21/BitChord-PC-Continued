//! Official BitChord Listen Together client (REST membership + authenticated WebSocket).
//! The server, not a desktop host, owns playback. Audio never passes through it.
//! See the upstream `backend/README.md` for the protocol shared with Android.

#[path = "jam/protocol.rs"]
pub mod protocol;

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

pub use protocol::{Clock, Frame, Identity, Membership, normalize_code, normalize_server};
pub const DEFAULT_SERVER: &str = "https://api.bitchord.kushagrasingh.in";

fn http_client() -> Result<reqwest::Client, String> {
    crate::proxy::apply(reqwest::Client::builder())
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| "Couldn't initialize the party connection.".to_owned())
}

/// Preview does not allocate a membership or send account details.
pub async fn preview(server: &str, code: &str) -> Result<protocol::Preview, String> {
    let code = normalize_code(code).ok_or_else(|| "Enter a six-character party code.".to_owned())?;
    let response = http_client()?.get(format!("{server}/api/parties/{code}/preview")).send().await.map_err(|_| "Couldn't reach the party server.".to_owned())?;
    if !response.status().is_success() {
        return Err(protocol::error_message("", response.status().as_u16()).to_owned());
    }
    let preview: protocol::Preview = response.json().await.map_err(|_| "The server returned an invalid party preview.".to_owned())?;
    if preview.code != code {
        return Err("The server returned a different party code.".into());
    }
    Ok(preview)
}

pub async fn probe(server: &str) -> Result<u64, String> {
    let started = Instant::now();
    let response = http_client()?.get(format!("{server}/healthz")).send().await.map_err(|_| "Party server is offline".to_owned())?;
    if !response.status().is_success() {
        return Err("Party server is offline".into());
    }
    let body: Value = response.json().await.map_err(|_| "Not a BitChord party server".to_owned())?;
    if body["ok"] != true {
        return Err("Not a BitChord party server".into());
    }
    Ok(started.elapsed().as_millis() as u64)
}

pub fn share_link(server: &str, code: &str) -> String {
    let origin = if server == DEFAULT_SERVER { "https://bitchord.kushagrasingh.in" } else { server };
    format!("{origin}/invite/{code}")
}

pub enum Event {
    Joined(Membership),
    Frame(Frame),
    Reconnecting,
    Closed(String),
}

enum Out {
    Text(String),
    Leave,
}

pub struct Jam {
    out: mpsc::UnboundedSender<Out>,
    events: Receiver<Event>,
    pub clock: Arc<Clock>,
}

impl Jam {
    /// `code == None` creates a new party; codes are allocated by the official server.
    pub fn start(backend: &crate::backend::Backend, server: String, code: Option<String>, identity: Identity) -> Self {
        Self::start_with_capacity(backend, server, code, identity, 5)
    }

    pub fn start_with_capacity(backend: &crate::backend::Backend, server: String, code: Option<String>, identity: Identity, max_members: u8) -> Self {
        let (out, rx) = mpsc::unbounded_channel();
        let (events, ev_rx) = unbounded();
        let clock = Arc::new(Clock::default());
        let wake = backend.clone();
        let conn = Conn { server, events, clock: clock.clone(), wake: Arc::new(move || wake.wake()) };
        backend.spawn(conn.run(code, identity, max_members.clamp(2, 10), rx));
        Self { out, events: ev_rx, clock }
    }

    pub fn try_recv(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }

    pub fn send(&self, frame: Value) {
        let _ = self.out.send(Out::Text(frame.to_string()));
    }

    pub fn control(&self, action: &str, mut body: Value) {
        body["type"] = "control".into();
        body["action"] = action.into();
        self.send(body);
    }

    pub fn leave(&self) {
        let _ = self.out.send(Out::Leave);
    }
}

impl Drop for Jam {
    fn drop(&mut self) {
        self.leave();
    }
}

struct Conn {
    server: String,
    events: Sender<Event>,
    clock: Arc<Clock>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

enum End {
    Left,
    Refused(String),
    Dropped,
}

impl Conn {
    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
        (self.wake)();
    }

    async fn membership(&self, http: &reqwest::Client, code: Option<&str>, identity: &Identity, max_members: u8) -> Result<Membership, String> {
        let path = match code {
            Some(code) => format!("/api/parties/{code}/join"),
            None => "/api/parties".to_owned(),
        };
        // Never forward a Google cookie or credential to a party server.
        let mut body = serde_json::to_value(identity).map_err(|_| "Invalid party identity.".to_owned())?;
        if code.is_none() {
            body["maxMembers"] = max_members.into();
        }
        let response =
            http.post(format!("{}{path}", self.server)).json(&body).send().await.map_err(|_| "Couldn't reach the party server. Check the address and try again.".to_owned())?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let error = response.json::<Value>().await.unwrap_or_default();
            return Err(protocol::error_message(error["error"].as_str().unwrap_or(""), status).to_owned());
        }
        response.json().await.map_err(|_| "The party server returned an invalid membership.".to_owned())
    }

    async fn run(self, code: Option<String>, identity: Identity, max_members: u8, mut out: mpsc::UnboundedReceiver<Out>) {
        let http = match crate::proxy::apply(reqwest::Client::builder()).redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(20)).build() {
            Ok(http) => http,
            Err(_) => {
                self.emit(Event::Closed("Couldn't initialize the party connection.".into()));
                return;
            }
        };
        let membership = match self.membership(&http, code.as_deref(), &identity, max_members).await {
            Ok(membership) if normalize_code(&membership.code).as_deref() == Some(membership.code.as_str()) && !membership.token.is_empty() => membership,
            Ok(_) => {
                self.emit(Event::Closed("The party server returned an invalid membership.".into()));
                return;
            }
            Err(message) => {
                self.emit(Event::Closed(message));
                return;
            }
        };
        let mut leaving = out.is_closed();
        // Cancellation during the REST call must release the slot too.
        while let Ok(message) = out.try_recv() {
            leaving |= matches!(message, Out::Leave);
        }
        if !leaving {
            self.clock.seed(if membership.server_ms > 0 { membership.server_ms } else { membership.party.server_ms });
            self.emit(Event::Joined(membership.clone()));
            let mut retries = 0;
            loop {
                let started = Instant::now();
                let end = self.session(&membership, &mut out).await;
                if started.elapsed() > Duration::from_secs(120) {
                    retries = 0;
                }
                match end {
                    End::Left => break,
                    End::Refused(message) => {
                        self.emit(Event::Closed(message));
                        break;
                    }
                    End::Dropped => {
                        retries += 1;
                        if retries > 6 {
                            self.emit(Event::Closed("Lost the connection to the party. Join again to reconnect.".into()));
                            break;
                        }
                        // Do not replay controls queued while disconnected: a delayed seek or
                        // skip could overwrite changes another listener made in the meantime.
                        self.emit(Event::Reconnecting);
                        let delay = tokio::time::sleep(Duration::from_secs(retries.min(5)));
                        tokio::pin!(delay);
                        let leave = loop {
                            tokio::select! {
                                _ = &mut delay => break false,
                                message = out.recv() => {
                                    if matches!(message, Some(Out::Leave) | None) { break true; }
                                }
                            }
                        };
                        if leave {
                            break;
                        }
                    }
                }
            }
        }
        // A host leaves just like any other member; the server reassigns the role.
        // Tokens stay in memory, never in UI persistence, URLs, or logs.
        let _ = http.post(format!("{}/api/parties/{}/leave", self.server, membership.code)).bearer_auth(&membership.token).send().await;
    }

    async fn session(&self, membership: &Membership, out: &mut mpsc::UnboundedReceiver<Out>) -> End {
        while let Ok(message) = out.try_recv() {
            if matches!(message, Out::Leave) {
                return End::Left;
            }
        }
        let base = self.server.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
        let url = format!("{base}/ws/parties/{}", membership.code);
        let connect = async {
            let u = url::Url::parse(&url)?;
            let tcp = crate::proxy::connect(u.host_str().unwrap_or_default(), u.port_or_known_default().unwrap_or(443)).await?;
            let mut request = url.as_str().into_client_request()?;
            let mut authorization = format!("Bearer {}", membership.token).parse::<tokio_tungstenite::tungstenite::http::HeaderValue>()?;
            authorization.set_sensitive(true);
            request.headers_mut().insert("Authorization", authorization);
            anyhow::Ok(tokio_tungstenite::client_async_tls(request, tcp).await?)
        };
        let ws = match tokio::time::timeout(Duration::from_secs(10), connect).await {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(error)) => {
                if let Some(tokio_tungstenite::tungstenite::Error::Http(response)) = error.downcast_ref::<tokio_tungstenite::tungstenite::Error>() {
                    if response.status().is_client_error() {
                        return End::Refused(protocol::error_message("", response.status().as_u16()).into());
                    }
                }
                return End::Dropped;
            }
            Err(_) => return End::Dropped,
        };
        let (mut tx, mut rx) = ws.split();
        let mut ping = tokio::time::interval(Duration::from_millis(300));
        let mut ping_count = 0;
        let mut last_rx = Instant::now();
        let mut welcomed = false;
        loop {
            tokio::select! {
                message = out.recv() => match message {
                    Some(Out::Text(text)) if welcomed => {
                        if tx.send(Message::text(text)).await.is_err() { return End::Dropped; }
                    }
                    Some(Out::Text(_)) => {},
                    Some(Out::Leave) | None => {
                        let _ = tx.close().await;
                        return End::Left;
                    }
                },
                _ = ping.tick() => {
                    if last_rx.elapsed() > Duration::from_secs(45) { return End::Dropped; }
                    let frame = json!({"type": "ping", "clientMs": self.clock.local_ms()});
                    if tx.send(Message::text(frame.to_string())).await.is_err() { return End::Dropped; }
                    ping_count += 1;
                    if ping_count == 4 {
                        ping = tokio::time::interval_at(tokio::time::Instant::now() + Duration::from_secs(10), Duration::from_secs(10));
                    }
                },
                message = rx.next() => {
                    let text = match message {
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(Message::Ping(data))) => {
                            if tx.send(Message::Pong(data)).await.is_err() { return End::Dropped; }
                            continue;
                        }
                        Some(Ok(Message::Close(_))) | None => return End::Dropped,
                        Some(Ok(_)) => continue,
                        Some(Err(_)) => return End::Dropped,
                    };
                    last_rx = Instant::now();
                    let Ok(frame) = serde_json::from_str::<Frame>(&text) else {
                        // Don't log raw frames: welcomes contain membership information.
                        log::warn!("jam: ignored an invalid party frame");
                        continue;
                    };
                    match &frame {
                        Frame::Welcome { server_ms, .. } => {
                            welcomed = true;
                            self.clock.seed(*server_ms);
                        }
                        Frame::Pong { client_ms, server_ms } => {
                            self.clock.sample(*client_ms, *server_ms);
                            continue;
                        }
                        Frame::Bye { reason } => return End::Refused(if reason == "kicked" {
                            "The host removed you from the party.".into()
                        } else { "You left the party.".into() }),
                        Frame::Error { error } => {
                            if matches!(error.as_str(), "unauthorized" | "not_found" | "invalid_token") {
                                return End::Refused(protocol::error_message(error, 401).into());
                            }
                        }
                        _ => {}
                    }
                    self.emit(Event::Frame(frame));
                }
            }
        }
    }
}
