pub mod client;
pub mod protocol;
pub mod server;

pub use client::{ClientError, EventSubscription, IpcClient, IPC_TIMEOUT};
pub use protocol::*;
pub use server::{
    default_socket_path, DesktopAction, DesktopCommand, DisplayModeCommand, FlipCommand, IpcServer,
    ScreenshotCommand, WindowAction, WindowCommand, EVENT_CHANNEL_CAPACITY, SCREENSHOT_TIMEOUT,
    WINDOW_COMMAND_TIMEOUT,
};

#[cfg(test)]
mod tests {
    use super::*;
    use rix_config::ConfigManager;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::BufReader;
    #[test]
    fn foreground_outputs_are_optional_for_older_compositors_and_preserved_in_snapshots() {
        let legacy = serde_json::json!({
            "id": 7, "title": "Game", "app_id": "game", "outputs": ["DP-1"],
            "is_active": false, "is_minimized": false,
            "is_maximized": false, "is_fullscreen": true
        });
        let mut window: WindowInfo = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(window.foreground_outputs, None);
        assert_eq!(serde_json::to_value(&window).unwrap(), legacy);
        for outputs in [vec!["DP-1".to_owned()], vec![]] {
            window.foreground_outputs = Some(outputs);
            let snapshot = IpcResponse::Windows(vec![window.clone()]);
            let serialized = serde_json::to_string(&snapshot).unwrap();
            assert_eq!(
                serde_json::from_str::<IpcResponse>(&serialized).unwrap(),
                snapshot
            );
        }
    }

    #[tokio::test]
    async fn png_transfer_uses_compact_bounded_payload_and_preserves_the_connection() {
        let directory = tempfile::tempdir().unwrap();
        let sock = directory.path().join("png.sock");
        let config = Arc::new(ConfigManager::init_in(directory.path().join("config")).unwrap());
        let server = Arc::new(IpcServer::new_with_path(config, sock.clone()));
        let (sender, receiver) = std::sync::mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
        server.set_desktop_sender(sender);
        let worker = std::thread::spawn(move || {
            let command = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
            let DesktopAction::Clipboard(IpcRequest::ClipboardSetPng { data }) = command.action
            else {
                panic!("Wrong clipboard command")
            };
            assert_eq!(data, vec![7; 1_100_000]);
            command.responder.send(Ok(IpcResponse::Ok)).unwrap();
        });
        let active = server.clone();
        let task = tokio::spawn(async move { active.run().await });
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut client = IpcClient::connect_to(sock).await.unwrap();
        assert_eq!(
            client
                .send(IpcRequest::ClipboardSetPng {
                    data: vec![7; 1_100_000]
                })
                .await
                .unwrap(),
            IpcResponse::Ok
        );
        assert_eq!(
            client.send(IpcRequest::Ping).await.unwrap(),
            IpcResponse::Pong
        );
        worker.join().unwrap();
        task.abort();
        let _ = task.await;
        assert!(serde_json::to_string(&IpcRequest::ClipboardSetPng {
            data: vec![0; CLIPBOARD_PNG_MAX_BYTES + 1]
        })
        .is_err());
    }

    async fn spawn_test_server() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("Failed to create tempdir for IPC test");
        let sock = dir.path().join("rix-ipc-test.sock");
        let config = Arc::new(
            ConfigManager::init_in(dir.path().join("config"))
                .expect("Failed to init config manager"),
        );
        let server = Arc::new(IpcServer::new_with_path(config, sock.clone()));

        let srv = server.clone();
        tokio::spawn(async move {
            let _ = srv.run().await;
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        (dir, sock)
    }

    #[tokio::test]
    async fn oversized_requests_are_disconnected_and_socket_is_private() {
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (_dir, socket) = spawn_test_server().await;
        assert_eq!(
            std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();

        let _ = stream.write_all(&vec![b' '; IPC_MAX_LINE_BYTES + 1]).await;
        let mut byte = [0];
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte))
            .await
            .unwrap();
        assert!(matches!(read, Ok(0) | Err(_)));
        let mut client = IpcClient::connect_to(socket).await.unwrap();
        assert_eq!(
            client.send(IpcRequest::Ping).await.unwrap(),
            IpcResponse::Pong
        );
    }

    #[tokio::test]
    async fn protocol_version_is_optional_and_mismatches_are_reported_clearly() {

        let legacy: IpcRequestEnvelope = serde_json::from_str(r#"{"type":"ping"}"#).unwrap();
        assert_eq!(legacy.protocol, LEGACY_PROTOCOL_VERSION);
        assert_eq!(legacy.request, IpcRequest::Ping);
        assert!(legacy.is_compatible());

        let current = IpcRequestEnvelope::new(IpcRequest::Ping);
        assert_eq!(current.protocol, CURRENT_PROTOCOL_VERSION);
        let wire = serde_json::to_string(&current).unwrap();
        assert!(wire.contains("\"protocol\":"));

        assert_eq!(
            serde_json::from_str::<IpcRequest>(&wire).unwrap(),
            IpcRequest::Ping
        );
        let mut future = current.clone();
        future.protocol = CURRENT_PROTOCOL_VERSION + 1;
        assert!(!future.is_compatible());

        let (_dir, socket) = spawn_test_server().await;
        let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        stream
            .write_all(
                format!(
                    "{{\"protocol\":{},\"type\":\"ping\"}}\n",
                    CURRENT_PROTOCOL_VERSION + 1
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<IpcResponse>(&line).unwrap(),
            IpcResponse::UnsupportedProtocol {
                supported: CURRENT_PROTOCOL_VERSION,
                received: CURRENT_PROTOCOL_VERSION + 1,
            }
        );

        reader
            .get_mut()
            .write_all(b"{\"type\":\"ping\"}\n")
            .await
            .unwrap();
        line.clear();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<IpcResponse>(&line).unwrap(),
            IpcResponse::Pong
        );
    }

    #[tokio::test]
    async fn oversized_plain_requests_are_rejected_without_killing_the_connection() {
        let (_dir, socket) = spawn_test_server().await;
        let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let mut raw = String::from("{\"type\":\"ping\"");
        raw.push_str(&" ".repeat(IPC_MAX_REQUEST_BYTES));
        raw.push_str("}\n");
        stream.write_all(raw.as_bytes()).await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let error = match serde_json::from_str::<IpcResponse>(&line).unwrap() {
            IpcResponse::Error(message) => message,
            other => panic!("expected an error response, got {other:?}"),
        };
        assert!(
            error.contains("1024 KiB"),
            "error must name the limit: {error}"
        );
        reader
            .get_mut()
            .write_all(b"{\"type\":\"ping\"}\n")
            .await
            .unwrap();
        line.clear();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<IpcResponse>(&line).unwrap(),
            IpcResponse::Pong
        );
    }

    #[tokio::test]
    async fn png_requests_are_bounded_by_type_not_by_key_order() {
        use base64::Engine;
        let directory = tempfile::tempdir().unwrap();
        let sock = directory.path().join("png-order.sock");
        let config = Arc::new(ConfigManager::init_in(directory.path().join("config")).unwrap());
        let server = Arc::new(IpcServer::new_with_path(config, sock.clone()));
        let (sender, receiver) = std::sync::mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
        server.set_desktop_sender(sender);
        let worker = std::thread::spawn(move || {
            let command = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
            let DesktopAction::Clipboard(IpcRequest::ClipboardSetPng { data }) = command.action
            else {
                panic!("Wrong clipboard command")
            };
            assert_eq!(data.len(), 1_100_000);
            command.responder.send(Ok(IpcResponse::Ok)).unwrap();
        });
        let active = server.clone();
        let task = tokio::spawn(async move { active.run().await });
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(vec![9_u8; 1_100_000]);

        let raw =
            format!("{{\"payload\":{{\"data\":\"{encoded}\"}},\"type\":\"clipboard_set_png\"}}\n");
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
        stream.write_all(raw.as_bytes()).await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<IpcResponse>(&line).unwrap(),
            IpcResponse::Ok
        );
        worker.join().unwrap();
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn a_broken_server_cannot_make_the_client_hang_or_grow_without_bound() {
        use tokio::io::AsyncWriteExt;
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("stalled.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            let mut big = vec![b'x'; IPC_MAX_LINE_BYTES + 1];
            big.push(b'\n');
            let _ = stream.write_all(&big).await;
            std::future::pending::<()>().await;
        });
        let mut client = IpcClient::connect_to(sock).await.unwrap();
        let error = client.send(IpcRequest::Ping).await.unwrap_err();
        assert!(matches!(error, ClientError::LineTooLong(bytes) if bytes == IPC_MAX_LINE_BYTES));
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn idle_event_stream_supports_an_explicit_timeout() {
        let (_dir, sock) = spawn_test_server().await;
        let mut sub = IpcClient::subscribe_to(sock.clone()).await.unwrap();
        let error = sub
            .next_event_timeout(Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(error, ClientError::Timeout(_)));

        let mut ctl = IpcClient::connect_to(sock).await.unwrap();
        ctl.send(IpcRequest::SetEditMode(true)).await.unwrap();
        let event = tokio::time::timeout(
            Duration::from_secs(5),
            sub.next_event_timeout(Duration::from_secs(5)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(event, IpcEvent::EditModeChanged(true));
    }

    #[tokio::test]
    async fn event_timeout_keeps_partially_received_lines() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("split.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let first = serde_json::to_string(&IpcEvent::EditModeChanged(true)).unwrap();
        let second = serde_json::to_string(&IpcEvent::LauncherToggleRequested).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut request = Vec::new();
            reader.read_until(b'\n', &mut request).await.unwrap();
            write_half
                .write_all(b"{\"status\":\"ok\"}\n")
                .await
                .unwrap();

            let split = first.len() / 2;
            write_half
                .write_all(&first.as_bytes()[..split])
                .await
                .unwrap();
            write_half.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(500)).await;
            write_half
                .write_all(&first.as_bytes()[split..])
                .await
                .unwrap();
            write_half.write_all(b"\n").await.unwrap();
            write_half.write_all(second.as_bytes()).await.unwrap();
            write_half.write_all(b"\n").await.unwrap();
            write_half.flush().await.unwrap();
            std::future::pending::<()>().await;
        });

        let mut sub = IpcClient::subscribe_to(sock).await.unwrap();

        let mut received = Vec::new();
        match sub.next_event_timeout(Duration::from_millis(100)).await {
            Ok(event) => received.push(event),
            Err(ClientError::Timeout(_)) => {

                received.push(
                    sub.next_event_timeout(Duration::from_secs(5))
                        .await
                        .unwrap(),
                );
            }
            Err(error) => panic!("unexpected subscription error: {error:?}"),
        }
        assert_eq!(received, vec![IpcEvent::EditModeChanged(true)]);

        let event = sub
            .next_event_timeout(Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(event, IpcEvent::LauncherToggleRequested);
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn excess_parallel_large_lines_are_rejected_cleanly() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let (_dir, socket) = spawn_test_server().await;

        let mut holders = Vec::new();
        for _ in 0..IPC_MAX_LARGE_READS {
            let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
            stream
                .write_all(&vec![b'h'; 2 * IPC_MAX_REQUEST_BYTES])
                .await
                .unwrap();
            holders.push(stream);
        }
        tokio::time::sleep(Duration::from_millis(200)).await;

        let mut waiting = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let _ = waiting
            .write_all(&vec![b'w'; IPC_MAX_REQUEST_BYTES + 1])
            .await;
        let mut reader = BufReader::new(waiting);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let response: IpcResponse = serde_json::from_str(&line).unwrap();
        assert!(
            matches!(&response, IpcResponse::Error(message) if message.contains("busy")),
            "expected a clear busy rejection, got: {response:?}"
        );

        let mut client = IpcClient::connect_to(socket.clone()).await.unwrap();
        assert_eq!(
            client.send(IpcRequest::Ping).await.unwrap(),
            IpcResponse::Pong
        );

        drop(holders);
        let mut recovered = false;
        for _ in 0..20 {
            let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
            let mut attempt = br#"{"type":"ping"}"#.to_vec();
            attempt.resize(IPC_MAX_REQUEST_BYTES + 100, b' ');
            attempt.push(b'\n');
            let _ = stream.write_all(&attempt).await;
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            if tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
                .await
                .is_ok_and(|result| result.is_ok())
            {
                let response: Option<IpcResponse> = serde_json::from_str(&line).ok();
                if let Some(IpcResponse::Error(message)) = response {
                    if message.contains("KiB limit") {
                        recovered = true;
                        break;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            recovered,
            "large transfers never resumed after slot release"
        );
    }

    #[test]
    fn unreadable_peer_credentials_skip_only_that_connection() {
        let error = std::io::Error::other("credentials unavailable");
        assert_eq!(crate::server::peer_uid(Err(error)), None);
    }

    #[tokio::test]
    async fn socket_is_removed_when_server_stops() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("shutdown.sock");
        let config = Arc::new(ConfigManager::init_in(dir.path().join("config")).unwrap());
        let server = Arc::new(IpcServer::new_with_path(config, sock.clone()));
        let task = tokio::spawn(async move { server.run().await });

        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(sock.exists(), "IPC server did not bind its socket");
        task.abort();
        let _ = task.await;
        assert!(!sock.exists(), "IPC socket remained after server shutdown");
    }

    #[tokio::test]
    async fn test_ipc_roundtrip() {
        let (_dir, sock) = spawn_test_server().await;
        let mut client = IpcClient::connect_to(sock.clone())
            .await
            .expect("Failed to connect to IPC server");

        let res = client
            .send(IpcRequest::Ping)
            .await
            .expect("Failed to send Ping");
        assert_eq!(res, IpcResponse::Pong);

        let res = client
            .send(IpcRequest::ToggleEditMode)
            .await
            .expect("Failed to toggle edit mode");
        assert_eq!(res, IpcResponse::EditMode(true));

        let res = client
            .send(IpcRequest::GetEditMode)
            .await
            .expect("Failed to get edit mode");
        assert_eq!(res, IpcResponse::EditMode(true));

        let res = client
            .send(IpcRequest::SetEditMode(false))
            .await
            .expect("Failed to set edit mode");
        assert_eq!(res, IpcResponse::EditMode(false));

        let res = client
            .send(IpcRequest::GetLayout)
            .await
            .expect("Failed to get layout");
        if let IpcResponse::Layout(layout) = res {
            assert!(!layout.panels.is_empty());
        } else {
            panic!("Expected Layout response");
        }
    }

    #[tokio::test]
    async fn layout_edits_merge_concurrent_clients_and_reject_conflicts() {
        let (_dir, socket) = spawn_test_server().await;
        let mut first = IpcClient::connect_to(socket.clone()).await.unwrap();
        let mut second = IpcClient::connect_to(socket.clone()).await.unwrap();
        let IpcResponse::Layout(base) = first.send(IpcRequest::GetLayout).await.unwrap() else {
            panic!("Expected layout")
        };
        let mut a = base.clone();
        a.panels[0].thickness = 60;
        let mut b = base.clone();
        b.keyboard.repeat_delay = 600;
        let (a, b) = tokio::join!(
            first.send(IpcRequest::EditLayout {
                base: base.clone(),
                updated: a
            }),
            second.send(IpcRequest::EditLayout {
                base: base.clone(),
                updated: b
            }),
        );
        assert!(matches!(a.unwrap(), IpcResponse::Layout(_)));
        assert!(matches!(b.unwrap(), IpcResponse::Layout(_)));
        let IpcResponse::Layout(merged) = first.send(IpcRequest::GetLayout).await.unwrap() else {
            panic!("Expected layout")
        };
        assert_eq!(merged.panels[0].thickness, 60);
        assert_eq!(merged.keyboard.repeat_delay, 600);
        let mut conflicting = base.clone();
        conflicting.panels[0].thickness = 72;
        assert!(
            matches!(first.send(IpcRequest::EditLayout { base, updated: conflicting }).await,
            Err(ClientError::Server(message)) if message.contains("another client") && message.contains("thickness"))
        );
        assert_eq!(
            first.send(IpcRequest::GetLayout).await.unwrap(),
            IpcResponse::Layout(merged)
        );
        assert_eq!(
            first.send(IpcRequest::Ping).await.unwrap(),
            IpcResponse::Pong
        );
    }

    #[tokio::test]
    async fn theme_and_desktop_edits_return_committed_snapshots() {
        let (_dir, socket) = spawn_test_server().await;
        let mut client = IpcClient::connect_to(socket).await.unwrap();
        let IpcResponse::Theme(base) = client.send(IpcRequest::GetTheme).await.unwrap() else {
            panic!("Expected theme")
        };
        let mut current = base.clone();
        current.cursor_size = 40;
        client.send(IpcRequest::SetTheme(current)).await.unwrap();
        let mut updated = base.clone();
        updated.opacity = 0.8;
        let IpcResponse::Theme(merged) = client
            .send(IpcRequest::EditTheme { base, updated })
            .await
            .unwrap()
        else {
            panic!("Expected theme")
        };
        assert_eq!(merged.cursor_size, 40);
        assert_eq!(merged.opacity, 0.8);
        let IpcResponse::Desktop(base) = client.send(IpcRequest::GetDesktop).await.unwrap() else {
            panic!("Expected desktop")
        };
        let mut current = base.clone();
        current.language = "en".into();
        client.send(IpcRequest::SetDesktop(current)).await.unwrap();
        let mut updated = base.clone();
        updated.wallpaper.blur = 2.;
        let IpcResponse::Desktop(merged) = client
            .send(IpcRequest::EditDesktop { base, updated })
            .await
            .unwrap()
        else {
            panic!("Expected desktop")
        };
        assert_eq!(merged.language, "en");
        assert_eq!(merged.wallpaper.blur, 2.);
        assert_eq!(
            client.send(IpcRequest::GetDesktop).await.unwrap(),
            IpcResponse::Desktop(merged)
        );
    }

    #[tokio::test]
    async fn test_window_ops_are_not_fake_ok() {
        let (_dir, sock) = spawn_test_server().await;
        let mut client = IpcClient::connect_to(sock).await.expect("connect");

        for req in [
            IpcRequest::FocusWindow(1),
            IpcRequest::CloseWindow(1),
            IpcRequest::MinimizeWindow(1),
            IpcRequest::MaximizeWindow(1),
            IpcRequest::ListWindows,
        ] {
            let err = client
                .send(req)
                .await
                .expect_err("window op must not return Ok");
            match err {
                ClientError::Server(msg) => {
                    assert!(
                        msg.contains("not implemented") || msg.contains("not available"),
                        "unexpected error message: {}",
                        msg
                    );
                }
                other => panic!("expected Server error, got {:?}", other),
            }
        }
    }

    #[tokio::test]
    async fn saturated_compositor_queue_does_not_block_ipc() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("busy.sock");
        let config = Arc::new(ConfigManager::init_in(dir.path().join("config")).unwrap());
        let server = Arc::new(IpcServer::new_with_path(config, sock.clone()));
        let (tx, rx) = std::sync::mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);

        let mut replies = Vec::new();
        for _ in 0..COMMAND_QUEUE_CAPACITY {
            let (responder, reply) = tokio::sync::oneshot::channel();
            tx.try_send(WindowCommand {
                action: WindowAction::List,
                responder,
            })
            .unwrap();
            replies.push(reply);
        }
        server.set_window_sender(tx);
        let srv = server.clone();
        let task = tokio::spawn(async move { srv.run().await });
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut client = IpcClient::connect_to(sock).await.unwrap();
        let error = tokio::time::timeout(
            Duration::from_millis(500),
            client.send(IpcRequest::ListWindows),
        )
        .await
        .expect("A full queue blocked the IPC worker")
        .unwrap_err();
        assert!(matches!(error, ClientError::Server(message) if message.contains("busy")));
        assert_eq!(
            client.send(IpcRequest::Ping).await.unwrap(),
            IpcResponse::Pong
        );

        drop(rx);
        assert!(matches!(client.send(IpcRequest::ListWindows).await,
            Err(ClientError::Server(message)) if message.contains("unavailable")));
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn test_window_commands_return_compositor_results() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("windows.sock");
        let config = Arc::new(ConfigManager::init_in(dir.path().join("config")).unwrap());
        let server = Arc::new(IpcServer::new_with_path(config, sock.clone()));
        let (tx, rx) = std::sync::mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
        server.set_window_sender(tx);
        let worker = std::thread::spawn(move || {
            for _ in 0..4 {
                let command: WindowCommand = rx.recv().unwrap();
                let result = match command.action {
                    WindowAction::List | WindowAction::ShowDesktop | WindowAction::Focus(7) => {
                        Ok(vec![WindowInfo {
                            outputs: Vec::new(),
                            foreground_outputs: Some(Vec::new()),
                            id: 7,
                            title: "Editor".into(),
                            app_id: "editor".into(),
                            is_active: true,
                            is_minimized: false,
                            is_maximized: false,
                            is_fullscreen: false,
                        }])
                    }
                    WindowAction::Close(99) => Err("Window 99 does not exist".into()),
                    other => panic!("unexpected command: {other:?}"),
                };
                let _ = command.responder.send(result);
            }
        });
        let srv = server.clone();
        tokio::spawn(async move {
            let _ = srv.run().await;
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut client = IpcClient::connect_to(sock).await.unwrap();
        match client.send(IpcRequest::ListWindows).await.unwrap() {
            IpcResponse::Windows(windows) => assert_eq!(windows[0].id, 7),
            other => panic!("unexpected response: {other:?}"),
        }
        assert_eq!(
            client.send(IpcRequest::FocusWindow(7)).await.unwrap(),
            IpcResponse::Ok
        );
        assert_eq!(
            client.send(IpcRequest::ShowDesktop).await.unwrap(),
            IpcResponse::Ok
        );
        assert!(matches!(client.send(IpcRequest::CloseWindow(99)).await,
            Err(ClientError::Server(message)) if message.contains("does not exist")));
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn test_subscribe_receives_events() {
        let (_dir, sock) = spawn_test_server().await;
        let mut sub = IpcClient::subscribe_to(sock.clone())
            .await
            .expect("subscribe handshake failed");

        let mut ctl = IpcClient::connect_to(sock).await.expect("connect");
        let res = ctl
            .send(IpcRequest::SetEditMode(true))
            .await
            .expect("SetEditMode failed");
        assert_eq!(res, IpcResponse::EditMode(true));

        let event = tokio::time::timeout(Duration::from_secs(5), sub.next_event())
            .await
            .expect("timed out waiting for event")
            .expect("event stream closed");
        assert_eq!(event, IpcEvent::EditModeChanged(true));

        ctl.send(IpcRequest::ToggleLauncher)
            .await
            .expect("ToggleLauncher failed");
        let event = tokio::time::timeout(Duration::from_secs(5), sub.next_event())
            .await
            .expect("timed out waiting for launcher event")
            .expect("event stream closed");
        assert_eq!(event, IpcEvent::LauncherToggleRequested);
    }
}
