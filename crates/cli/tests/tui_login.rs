//! The TUI's subscription login runner (`tui_login`, client-tui-auth.ts `runProviderLogin`): Esc while the device
//! code is shown stops the flow (`Login cancelled`): it ends, reports nothing and saves nothing, even though the
//! gateway would still answer later polls. Waits on the runner's own events (120 s hang guard).

use interactive::lane::{LaneEvent, LoginEvent, LoginPrompt, LoginStep};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, mpsc};
use std::time::Duration;

/// A Radius gateway whose token polls stay pending.
fn gateway() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                if header.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = header.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let (status, reply) = if path == "/v1/oauth/device" {
                (
                    200,
                    json!({"device_code": "d", "user_code": "CODE-1", "verification_uri": "http://127.0.0.1/device",
                        "expires_in": 600, "interval": 1}),
                )
            } else {
                (400, json!({"error": "authorization_pending"}))
            };
            let text = reply.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    port
}

/// An agent dir under `tmp` whose `gw` provider signs in with the pending gateway.
fn gateway_env(tmp: &std::path::Path) -> (cli::Env, std::path::PathBuf) {
    let port = gateway();
    let agent = tmp.join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("models.json"),
        json!({"providers": {"gw": {"oauth": "radius", "baseUrl": format!("http://127.0.0.1:{port}/v1")}}}).to_string(),
    )
    .unwrap();
    let agent_dir = agent.display().to_string();
    let env = cli::Env {
        cwd: tmp.to_path_buf(),
        home: tmp.to_path_buf(),
        var: Arc::new(move |k| (k == config::paths::ENV_AGENT_DIR).then(|| agent_dir.clone())),
        package: tmp.to_path_buf(),
    };
    (env, agent)
}

#[test]
fn cli_tui_login_esc_stops_the_flow_and_saves_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (env, agent) = gateway_env(tmp.path());
    assert!(cli::tui_login::supported(&env, "gw"));
    let (tx, rx) = mpsc::channel::<LaneEvent>();
    let tx = std::sync::Mutex::new(tx);
    let send: Arc<dyn Fn(LaneEvent) + Send + Sync> = Arc::new(move |event| {
        let _ = tx.lock().unwrap().send(event);
    });
    let slot = Arc::new(cli::tui_login::LoginSlot::default());
    let (saved_tx, saved_rx) = mpsc::channel::<()>();
    let saved_tx = std::sync::Mutex::new(saved_tx);
    cli::tui_login::start(
        env,
        1,
        "gw".into(),
        "gw".into(),
        send,
        slot.clone(),
        Box::new(move || {
            let _ = saved_tx.lock().unwrap().send(());
        }),
    );
    let guard = Duration::from_secs(120);
    let mut events = Vec::new();
    loop {
        let event = rx.recv_timeout(guard).expect("the login's next step within 120 s");
        match &event {
            LaneEvent::Login(_, LoginStep::Prompt(LoginPrompt::Select { options, .. })) => {
                assert_eq!(options[1].0, "device-code");
                slot.reply(1, Some("device-code".into()));
            }
            // Esc in the dialog while the code is shown.
            LaneEvent::Login(_, LoginStep::Event(LoginEvent::DeviceCode { user_code, .. })) => {
                assert_eq!(user_code, "CODE-1");
                slot.reply(1, None);
            }
            LaneEvent::Login(_, LoginStep::Finished) => break,
            _ => {}
        }
        events.push(event);
    }
    // A cancel reports nothing and saves nothing.
    assert!(!events.iter().any(|e| matches!(e, LaneEvent::Status(_))), "{events:?}");
    assert!(saved_rx.try_recv().is_err());
    assert!(!agent.join("auth.json").exists());
}

/// Two logins at once (a second `/login` after the first dialog was replaced): each has its own signal and prompt,
/// so cancelling or ending the first never touches the second (before, the second overwrote the shared signal and
/// the first's end dropped the second's pending prompt).
#[test]
fn cli_tui_login_two_logins_are_kept_apart() {
    let tmp = tempfile::tempdir().unwrap();
    let (env, _agent) = gateway_env(tmp.path());
    let (tx, rx) = mpsc::channel::<LaneEvent>();
    let tx = std::sync::Mutex::new(tx);
    let send: Arc<dyn Fn(LaneEvent) + Send + Sync> = Arc::new(move |event| {
        let _ = tx.lock().unwrap().send(event);
    });
    let slot = Arc::new(cli::tui_login::LoginSlot::default());
    for id in [1, 2] {
        cli::tui_login::start(
            env.clone(),
            id,
            "gw".into(),
            "gw".into(),
            send.clone(),
            slot.clone(),
            Box::new(|| {}),
        );
    }
    let guard = Duration::from_secs(120);
    let mut codes = std::collections::HashSet::new();
    // Both reach their device code.
    while codes.len() < 2 {
        match rx.recv_timeout(guard).expect("the logins' next step within 120 s") {
            LaneEvent::Login(id, LoginStep::Prompt(LoginPrompt::Select { .. })) => {
                slot.reply(id, Some("device-code".into()))
            }
            LaneEvent::Login(id, LoginStep::Event(LoginEvent::DeviceCode { .. })) => {
                codes.insert(id);
            }
            LaneEvent::Login(id, LoginStep::Finished) => panic!("login {id} ended early"),
            _ => {}
        }
    }
    // Esc on the first: only the first ends.
    slot.reply(1, None);
    loop {
        match rx.recv_timeout(guard).expect("the first login ends within 120 s") {
            LaneEvent::Login(1, LoginStep::Finished) => break,
            LaneEvent::Login(2, LoginStep::Finished) => panic!("cancelling login 1 ended login 2"),
            _ => {}
        }
    }
    assert!(!slot.running(1));
    assert!(slot.running(2), "login 2 still runs after login 1 ended");
    // The second is still cancellable on its own.
    slot.reply(2, None);
    loop {
        if let LaneEvent::Login(2, LoginStep::Finished) = rx.recv_timeout(guard).expect("the second login ends") {
            break;
        }
    }
    assert!(!slot.running(2));
}
