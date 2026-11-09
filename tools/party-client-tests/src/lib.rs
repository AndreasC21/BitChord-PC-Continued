#![allow(dead_code, unused_imports)]

// Only the runtime/wakeup and proxy adapters are substituted. All membership,
// authentication, parsing, clocks, and connection lifecycle code is production code.
#[path = "../../../src/jam.rs"]
mod jam;

mod backend {
    #[derive(Clone)]
    pub struct Backend;
    impl Backend {
        pub fn wake(&self) {}
        pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
            tokio::spawn(task);
        }
    }
}

mod proxy {
    pub fn apply(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        builder.no_proxy()
    }
    pub async fn connect(host: &str, port: u16) -> std::io::Result<tokio::net::TcpStream> {
        tokio::net::TcpStream::connect((host, port)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use jam::protocol::{Frame, Identity, PartyTrack};
    use serde_json::json;
    use std::time::Duration;

    async fn next(client: &jam::Jam) -> jam::Event {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if let Some(event) = client.try_recv() {
                    return event;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("timed out waiting for a party event")
    }

    async fn welcome(client: &jam::Jam) {
        loop {
            match next(client).await {
                jam::Event::Frame(Frame::Welcome { .. }) => return,
                jam::Event::Closed(message) => panic!("party closed: {message}"),
                _ => {}
            }
        }
    }

    async fn state(client: &jam::Jam, playing: bool, video: &str) {
        loop {
            match next(client).await {
                jam::Event::Frame(Frame::State { playback }) if playback.is_playing == playing && playback.track.as_ref().is_some_and(|t| t.video_id == video) => return,
                jam::Event::Closed(message) => panic!("party closed: {message}"),
                _ => {}
            }
        }
    }

    struct Server(std::process::Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn identity(name: &str) -> Identity {
        Identity { user_id: name.into(), device_id: format!("test-{name}"), display_name: name.into(), avatar_url: None }
    }

    #[tokio::test]
    async fn cancelling_during_membership_releases_the_slot_without_opening_a_socket() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (arrived, request_seen) = tokio::sync::oneshot::channel();
        let (respond, allowed) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 8192];
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]);
            assert!(request.starts_with("POST /api/parties HTTP/1.1"));
            assert!(!request.to_ascii_lowercase().contains("cookie:"));
            let _ = arrived.send(());
            allowed.await.unwrap();
            let body = r#"{"code":"ABC012","token":"fixture-token","you":{"memberId":"desktop"},"party":{"serverMs":123}}"#;
            let response = format!("HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            socket.write_all(response.as_bytes()).await.unwrap();
            drop(socket);
            let (mut socket, _) = listener.accept().await.unwrap();
            let read = socket.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]);
            assert!(request.starts_with("POST /api/parties/ABC012/leave HTTP/1.1"));
            assert!(request.to_ascii_lowercase().contains("authorization: bearer fixture-token"));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
        });
        let client = jam::Jam::start(&backend::Backend, base, None, identity("cancelled"));
        tokio::time::timeout(Duration::from_secs(5), request_seen).await.unwrap().unwrap();
        client.leave();
        respond.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
        assert!(client.try_recv().is_none(), "a cancelled join must not reach the UI");
    }

    /// Run against a locally built copy of the OFFICIAL Go backend, never the public
    /// service: BITCHORD_TEST_BACKEND=/absolute/path/to/bitchord-jam[.exe].
    #[tokio::test]
    #[ignore = "requires BITCHORD_TEST_BACKEND pointing at the official Go backend binary"]
    async fn official_backend_bidirectional_controls_permissions_and_leave() {
        tokio::time::timeout(Duration::from_secs(60), exercise_backend()).await.expect("official backend integration test timed out");
    }

    async fn exercise_backend() {
        let executable = std::env::var("BITCHORD_TEST_BACKEND").expect("set BITCHORD_TEST_BACKEND");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let _server =
            Server(std::process::Command::new(executable).env("PORT", port.to_string()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap());
        let base = format!("http://127.0.0.1:{port}");
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if http.get(format!("{base}/healthz")).send().await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();

        jam::probe(&base).await.expect("the UI health check must recognize the official backend");
        let host = jam::Jam::start_with_capacity(&backend::Backend, base.clone(), None, identity("desktop"), 6);
        let (code, token) = loop {
            match next(&host).await {
                jam::Event::Joined(membership) => {
                    assert_eq!(membership.party.max_members, 6);
                    break (membership.code, membership.token);
                }
                jam::Event::Closed(message) => panic!("create failed: {message}"),
                _ => {}
            }
        };
        welcome(&host).await;
        let preview = jam::preview(&base, &code).await.expect("v1.9-style lookup must work before joining");
        assert_eq!(preview.member_count, 1);
        assert_eq!(preview.max_members, 6);
        assert_eq!(preview.host_name, "desktop");
        assert!(!preview.is_full);
        let peer = jam::Jam::start(&backend::Backend, base.clone(), Some(code.clone()), identity("protocol-peer"));
        welcome(&peer).await;
        host.control("setMaxMembers", json!({"maxMembers": 7}));
        loop {
            if let jam::Event::Frame(Frame::Members { max_members: 7, .. }) = next(&peer).await {
                break;
            }
        }
        let track = PartyTrack { video_id: "test-video".into(), title: "Test track".into(), duration_ms: Some(180_000), ..Default::default() };
        host.control("setQueue", json!({"queue": [track], "queueIndex": 0}));
        host.control("setTrack", json!({"track": track, "queueIndex": 0, "positionMs": 42_000, "isPlaying": true}));
        state(&peer, true, "test-video").await;
        state(&host, true, "test-video").await;
        // The other participant can pause the desktop host, not just follow it.
        peer.control("pause", json!({}));
        state(&host, false, "test-video").await;
        state(&peer, false, "test-video").await;
        host.control("play", json!({"positionMs": 50_000}));
        state(&peer, true, "test-video").await;

        // A second socket for our own token replaces the first one. The production
        // client must reconnect with the same token and recover the room snapshot.
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = format!("ws://127.0.0.1:{port}/ws/parties/{code}").into_client_request().unwrap();
        request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
        let (mut replacement, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let _ = replacement.next().await;
        loop {
            if let jam::Event::Reconnecting = next(&host).await {
                break;
            }
        }
        welcome(&host).await;
        host.control("seek", json!({"positionMs": 60_000}));
        loop {
            if let jam::Event::Frame(Frame::State { playback }) = next(&peer).await {
                if playback.position_ms == 60_000 {
                    break;
                }
            }
        }

        host.control("setHostOnlyControl", json!({"enabled": true}));
        loop {
            if let jam::Event::Frame(Frame::Members { host_only_control: true, .. }) = next(&peer).await {
                break;
            }
        }
        peer.control("pause", json!({}));
        loop {
            if let jam::Event::Frame(Frame::Error { error }) = next(&peer).await {
                assert_eq!(error, "host_only");
                break;
            }
        }
        let removed = jam::Jam::start(&backend::Backend, base.clone(), Some(code.clone()), identity("removed-peer"));
        let removed_id = loop {
            match next(&removed).await {
                jam::Event::Joined(membership) => break membership.you.member_id,
                jam::Event::Closed(message) => panic!("third participant failed to join: {message}"),
                _ => {}
            }
        };
        welcome(&removed).await;
        host.control("kick", json!({"memberId": removed_id}));
        loop {
            if let jam::Event::Closed(message) = next(&removed).await {
                assert!(message.contains("removed"));
                break;
            }
        }
        host.leave();
        loop {
            if let jam::Event::Frame(Frame::Members { members, .. }) = next(&peer).await {
                if members.len() == 1 && members[0].is_host {
                    break;
                }
            }
        }
        // Leaving the desktop host does not close a mobile participant's party.
        peer.control("pause", json!({}));
        state(&peer, false, "test-video").await;
        peer.leave();
    }
}
