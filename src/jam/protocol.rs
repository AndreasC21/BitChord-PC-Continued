//! Wire types and timing shared by the native client and its protocol tests.

use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const CODE_LEN: usize = 6;

pub fn normalize_code(value: &str) -> Option<String> {
    let code: String = value
        .chars()
        .filter(|c| c.is_alphanumeric())
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect();
    (code.len() == CODE_LEN && code.chars().all(|c| "0123456789ABCDEFGHJKMNPQRSTUVWXYZ".contains(c))).then_some(code)
}

pub fn normalize_server(value: &str) -> Result<String, &'static str> {
    let value = value.trim();
    if value.is_empty() {
        return Err("Enter a party server address.");
    }
    if value.chars().any(char::is_whitespace) {
        return Err("The server address cannot contain spaces.");
    }
    let candidate = if value.contains("://") { value.to_owned() } else { format!("https://{value}") };
    let u = url::Url::parse(&candidate).map_err(|_| "Enter a valid HTTP(S) server address.")?;
    if !matches!(u.scheme(), "https" | "http") || u.host_str().is_none() || !u.username().is_empty() || u.password().is_some() || u.query().is_some() || u.fragment().is_some() {
        return Err("Use an HTTP(S) address without credentials, query, or fragment.");
    }
    Ok(u.to_string().trim_end_matches('/').to_owned())
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub user_id: String,
    pub device_id: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PartyTrack {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub thumbnail_url: Option<String>,
    pub duration_ms: Option<u64>,
    pub from_autoplay: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Member {
    pub member_id: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub is_host: bool,
    pub connected: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Playback {
    pub seq: u64,
    pub track: Option<PartyTrack>,
    pub queue_seq: u64,
    pub queue_index: i64,
    pub is_playing: bool,
    pub position_ms: u64,
    pub anchor_ms: i64,
    pub effective_position_ms: u64,
    pub started_by_name: Option<String>,
}

impl Playback {
    pub fn position_at(&self, server_ms: i64) -> u64 {
        let elapsed = if self.is_playing { server_ms.saturating_sub(self.anchor_ms).max(0) as u64 } else { 0 };
        let position = self.position_ms.saturating_add(elapsed);
        self.track.as_ref().and_then(|t| t.duration_ms).filter(|d| *d > 0).map_or(position, |d| position.min(d))
    }

    pub fn lead_ms(&self, server_ms: i64) -> u64 {
        if self.is_playing { self.anchor_ms.saturating_sub(server_ms).max(0) as u64 } else { 0 }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PartyQueue {
    pub seq: u64,
    pub items: Vec<PartyTrack>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Snapshot {
    pub server_ms: i64,
    #[serde(default = "default_capacity")]
    pub max_members: u8,
    pub host_only_control: bool,
    pub members: Vec<Member>,
    pub playback: Playback,
    pub queue: PartyQueue,
}

fn default_capacity() -> u8 {
    5
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub code: String,
    #[serde(default)]
    pub host_name: String,
    pub member_count: usize,
    pub max_members: u8,
    pub is_full: bool,
    #[serde(default)]
    pub members: Vec<PreviewMember>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PreviewMember {
    pub display_name: String,
    pub avatar_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    pub action: String,
    pub by: String,
    pub at_ms: i64,
    #[serde(default)]
    pub detail: String,
}

// Do not derive Debug: the party token is a credential.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Membership {
    pub code: String,
    pub token: String,
    pub you: Member,
    pub party: Snapshot,
    #[serde(default)]
    pub server_ms: i64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Frame {
    Welcome {
        you: Member,
        party: Snapshot,
        #[serde(rename = "serverMs")]
        server_ms: i64,
    },
    State {
        playback: Playback,
    },
    Queue {
        queue: PartyQueue,
    },
    Members {
        members: Vec<Member>,
        #[serde(default = "default_capacity", rename = "maxMembers")]
        max_members: u8,
        #[serde(default, rename = "hostOnlyControl")]
        host_only_control: bool,
    },
    Pong {
        #[serde(rename = "clientMs")]
        client_ms: i64,
        #[serde(rename = "serverMs")]
        server_ms: i64,
    },
    Error {
        error: String,
    },
    Bye {
        #[serde(default)]
        reason: String,
    },
    Activity {
        #[serde(flatten)]
        entry: Activity,
    },
    #[serde(other)]
    Unknown,
}

/// A wall-clock origin advanced monotonically; a Windows clock adjustment must not
/// move the party playhead. Samples are replaced periodically so long sessions
/// can learn a new offset instead of keeping an ancient minimum RTT forever.
pub struct Clock {
    origin: Instant,
    wall_ms: i64,
    sample: Mutex<ClockSample>,
}

struct ClockSample {
    offset: i64,
    best_rtt: i64,
    measured_at: Instant,
    ready: bool,
}

impl Default for Clock {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            origin: now,
            wall_ms: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as i64,
            sample: Mutex::new(ClockSample { offset: 0, best_rtt: i64::MAX, measured_at: now, ready: false }),
        }
    }
}

impl Clock {
    pub fn local_ms(&self) -> i64 {
        self.wall_ms + self.origin.elapsed().as_millis() as i64
    }

    pub fn server_ms(&self) -> i64 {
        self.local_ms() + self.sample.lock().unwrap().offset
    }

    pub fn ready(&self) -> bool {
        self.sample.lock().unwrap().ready
    }

    pub fn round_trip_ms(&self) -> Option<i64> {
        let sample = self.sample.lock().unwrap();
        sample.ready.then_some(sample.best_rtt)
    }

    pub fn seed(&self, server_ms: i64) {
        let mut sample = self.sample.lock().unwrap();
        if !sample.ready {
            sample.offset = server_ms - self.local_ms();
        }
    }

    pub fn sample(&self, sent_ms: i64, server_ms: i64) {
        self.sample_at(sent_ms, self.local_ms(), server_ms);
    }

    fn sample_at(&self, sent_ms: i64, received_ms: i64, server_ms: i64) {
        let rtt = received_ms - sent_ms;
        if !(0..=30_000).contains(&rtt) {
            return;
        }
        let mut sample = self.sample.lock().unwrap();
        if rtt <= sample.best_rtt || sample.measured_at.elapsed().as_secs() > 60 {
            sample.offset = server_ms - (sent_ms + rtt / 2);
            sample.best_rtt = rtt;
            sample.measured_at = Instant::now();
            sample.ready = true;
        }
    }
}

pub fn error_message(code: &str, status: u16) -> &'static str {
    match code {
        "host_only" => "Only the host can control this party.",
        "queue_full" => "The party queue is full.",
        "rate_limited" => "Too many requests. Wait a moment and try again.",
        "end_of_queue" => "Already at the end of the party queue.",
        "start_of_queue" => "Already at the beginning of the party queue.",
        "not_found" => "No party with that code on this server.",
        "party_full" | "full" => "That party is full.",
        "invalid_capacity" => "Party size must be between 2 and 10.",
        "party_too_small" => "Party size cannot be smaller than the current member count.",
        "unauthorized" | "invalid_token" => "Your party membership expired. Join again.",
        _ => match status {
            401 | 403 => "Your party membership expired or access was refused. Join again.",
            404 => "No party with that code on this server.",
            409 => "That party is full.",
            429 => "Too many requests. Wait a moment and try again.",
            _ => "The party server couldn't apply that request.",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_codes_include_zero_and_one() {
        assert_eq!(normalize_code(" ab-oIl2 ").as_deref(), Some("AB0112"));
        assert_eq!(normalize_code("ABC234").as_deref(), Some("ABC234"));
        for bad in ["", "ABCDE", "ABCDEFG", "ÄBC234"] {
            assert!(normalize_code(bad).is_none());
        }
    }

    #[test]
    fn server_addresses_do_not_accept_credentials_or_tokens() {
        assert_eq!(normalize_server(" PARTY.example.com/base/ ").unwrap(), "https://party.example.com/base");
        assert_eq!(normalize_server("http://127.0.0.1:8000/").unwrap(), "http://127.0.0.1:8000");
        for bad in ["", "ftp://example.com", "https://user:pass@example.com", "https://example.com/?token=x", "https://example.com/#x", "https://exa mple.com"] {
            assert!(normalize_server(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn scheduled_playback_holds_until_anchor_and_clamps_to_duration() {
        let mut playback = Playback {
            position_ms: 42_000,
            anchor_ms: 10_000,
            is_playing: true,
            track: Some(PartyTrack { duration_ms: Some(60_000), ..Default::default() }),
            ..Default::default()
        };
        assert_eq!(playback.position_at(9_650), 42_000);
        assert_eq!(playback.lead_ms(9_650), 350);
        assert_eq!(playback.position_at(10_300), 42_300);
        assert_eq!(playback.position_at(99_000), 60_000);
        playback.is_playing = false;
        assert_eq!(playback.position_at(99_000), 42_000);
    }

    #[test]
    fn lowest_rtt_clock_sample_wins() {
        let clock = Clock::default();
        clock.sample_at(1_000, 1_100, 1_550);
        assert_eq!(clock.sample.lock().unwrap().offset, 500);
        clock.sample_at(2_000, 2_400, 3_000);
        assert_eq!(clock.sample.lock().unwrap().offset, 500);
        clock.sample_at(3_000, 3_020, 3_610);
        assert_eq!(clock.sample.lock().unwrap().offset, 600);
        assert!(clock.ready());
    }

    #[test]
    fn parses_official_frames_and_ignores_extensions() {
        let frame: Frame = serde_json::from_str(r#"{"type":"state","playback":{"seq":7,"queueSeq":3,"queueIndex":1,"positionMs":42000,"anchorMs":10000,"isPlaying":true,"track":{"videoId":"song","durationMs":60000}}}"#).unwrap();
        let Frame::State { playback } = frame else { panic!("not a state") };
        assert_eq!(playback.seq, 7);
        assert_eq!(playback.queue_seq, 3);
        assert_eq!(playback.track.unwrap().video_id, "song");
        assert!(matches!(serde_json::from_str::<Frame>(r#"{"type":"futureExtension"}"#).unwrap(), Frame::Unknown));
        let frame: Frame = serde_json::from_str(r#"{"type":"members","members":[]}"#).unwrap();
        assert!(matches!(frame, Frame::Members { host_only_control: false, .. }));
    }

    #[test]
    fn membership_accepts_server_time_inside_snapshot_only() {
        // The official Go create/join response omits top-level serverMs, even
        // though its Kotlin wire model exposes that field with a default.
        let membership: Membership = serde_json::from_str(
            r#"{"code":"ABC012","token":"test-only","you":{"memberId":"a"},"party":{"serverMs":123,"playback":{"queueIndex":-1,"track":null},"queue":{"seq":0,"items":[]}}}"#,
        )
        .unwrap();
        assert_eq!(membership.server_ms, 0);
        assert_eq!(membership.party.server_ms, 123);
    }

    #[test]
    fn parses_v19_room_dashboard_fields() {
        let frame: Frame = serde_json::from_str(
            r#"{"type":"members","maxMembers":8,"hostOnlyControl":true,"members":[{"memberId":"a","displayName":"Ada","avatarUrl":"https://example.com/ada.jpg","isHost":true}]}"#,
        )
        .unwrap();
        let Frame::Members { members, max_members, host_only_control } = frame else { panic!("not a member frame") };
        assert_eq!(max_members, 8);
        assert!(host_only_control);
        assert_eq!(members[0].avatar_url.as_deref(), Some("https://example.com/ada.jpg"));
        let frame: Frame = serde_json::from_str(r#"{"type":"activity","action":"queueAdd","by":"Ada","atMs":123,"detail":"Added a song"}"#).unwrap();
        let Frame::Activity { entry } = frame else { panic!("not activity") };
        assert_eq!(entry.by, "Ada");
        assert_eq!(entry.detail, "Added a song");
    }
}
