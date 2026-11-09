# Listen Together (native desktop + official mobile)

The native client uses the official BitChord REST/WebSocket party protocol, not
the unrelated `ytm-rs-jam` relay used by the initial Rust rewrite. The old relay
under `server/` is not the backend for this client and cannot host mobile rooms.

## Using it

1. Sign in to YouTube Music and open **Listen Together** in the sidebar.
2. Check **Party server**. The default is
   `https://api.bitchord.kushagrasingh.in`. Both devices must use the same server.
3. Start a party on either desktop or official mobile. On desktop, enter its code
   or paste an official invite, choose **Look up party**, review the members, then
   choose **Join party**. A lookup does not allocate a membership slot.
4. Play, pause, seek, next/previous and queue edits go through the server. With
   host-only control enabled, other members can listen but cannot change music.
5. Leave when finished. A host's departure transfers ownership rather than closing
   the party on the remaining devices.

The native screen keeps the v1.9 flow with the current app's shared margins,
page title, glass cards, text fields and pill controls: Start/Join choice cards, a nickname and
party-size selector, server Test/Save controls, join confirmation, and a room
dashboard with a QR invite, Now Playing, Up Next, listening members, host controls
and activity. Wide windows show two columns; narrow windows stack the cards.
The host can change the capacity (2–10, never below the current member count) and
remove a member after confirmation. The QR encodes the same link as **Copy invite**.
v1.9's local-only guest pause/detach mode is not implemented: host-only mode still
restricts shared playback, and the native screen does not advertise local pause.

The server address, display-name preference and installation ID are saved locally.
`BITCHORD_PARTY_SERVER` supplies the initial address when no saved preference exists.
Self-hosting requires the **official** backend from `kushagrasinghx/BitChord/backend`,
not this repository's legacy `server/`. Use HTTPS outside local development. Custom
servers receive the display name, avatar URL and an installation-scoped user/device
ID; they never receive Google cookies. The user ID is not a verified Google account ID.
Membership tokens stay in memory and are sent in the Authorization header, never in
URLs or logs. Closing and reopening the app requires joining again; abrupt exits
rely on the server's disconnected-member expiry.

External invites show the lobby for confirmation rather than silently joining or
changing the configured server. Server parameters in pasted links are not applied;
check the server address explicitly, especially for self-hosted parties.

## Timing and current scope

- All devices, including the host, follow the server's versioned playback state.
  A separate versioned queue avoids repeatedly transmitting the full list.
- Clock synchronization uses minimum-round-trip samples and a monotonic local
  clock. Future play/seek anchors are respected; persistent drift is corrected
  after buffering, with a cooldown to avoid seek loops.
- Audio is resolved and streamed independently on each device. Different cuts,
  unavailable tracks, decoder latency and very high network latency can still
  prevent perfect alignment. Exact mobile audio alignment requires real-device tests.
- Automix, crossfade, SponsorBlock and local autoplay are temporarily suppressed
  while in a party, without changing their saved preferences. The host advances
  on track completion. Desktop does not currently supply shared autoplay suggestions.
- Selecting songs on an opened playlist shares a bounded queue (current track plus
  up to 25 upcoming tracks, further limited by frame size). Collection-level play
  buttons and party shuffle are not implemented.
- A local playback failure does not skip the room's song for everyone else.
- Network reconnects reuse the membership token and fetch a fresh welcome snapshot;
  controls made during disconnection are not replayed later.

## Automated checks

With the platform's normal Rust/native build prerequisites:

```sh
cargo check --locked
cargo test --locked jam
cargo test --locked deeplink
```

The lightweight harness imports the production connection and protocol modules
without compiling the desktop's GUI/audio dependencies:

```sh
cargo test --manifest-path tools/party-client-tests/Cargo.toml --locked
```

For the integration test, build a copy of the official Go backend separately:

```sh
# From kushagrasinghx/BitChord/backend:
go build -o /absolute/path/to/bitchord-jam .

# From this repository (PowerShell: $env:BITCHORD_TEST_BACKEND='C:\path\bitchord-jam.exe'):
export BITCHORD_TEST_BACKEND=/absolute/path/to/bitchord-jam
cargo test --manifest-path tools/party-client-tests/Cargo.toml --locked -- --include-ignored
```

The test launches the supplied backend locally on a temporary port and checks
health probes, previews, party capacity, creation, joining, sender echoes, controls
in both directions, host-only enforcement, member removal and host transfer.
It does not contact the public server, run Android, or test audio.
The harness substitutes only the runtime/wakeup and direct-connect proxy adapters.

## Manual PC/mobile acceptance checklist

Do not claim end-to-end mobile compatibility is verified until these are tested
with the released official mobile app and a built desktop executable:

- [ ] PC creates -> mobile joins; mobile creates -> PC joins.
- [ ] A code containing `0`/`1` works; pasted official invite works.
- [ ] Selecting a song, pause/resume, seek and next/previous work from both devices.
- [ ] Repeated tracks preserve the selected queue index.
- [ ] Queue add, play-next and removal propagate without feedback loops.
- [ ] Host-only mode is enforced and updates after host reassignment.
- [ ] Party lookup shows the host/members before joining and reports a full room.
- [ ] QR scanning opens the same invite as Copy invite.
- [ ] Capacity changes and member removal propagate to other participants.
- [ ] Track completion advances once, not once per participant.
- [ ] A brief network outage reconnects without restoring stale desktop playback.
- [ ] A member leaving frees its slot; the host leaving preserves the party.
- [ ] A locally unavailable song does not skip everyone else's playback.
- [ ] Automix/crossfade/SponsorBlock do not run in the party and return after leaving.
- [ ] Delayed streams catch up without repeated audible seeking.
