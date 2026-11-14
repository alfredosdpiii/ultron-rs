//! Which backend a process uses: `memory.backend`, else memorantado once its server answers (kept), else Hindsight.

use crate::fake::FakeMemorantado;
use memorantado::resolve::{Backend, Choice, Endpoint, Hindsight, answers, memorantado_url, remembered};
use memorantado::{DEFAULT_MEMORANTADO_URL, ReqwestTransport};

fn manager(global: &str) -> (tempfile::TempDir, config::settings::SettingsManager) {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(agent.join("settings.json"), global).unwrap();
    let manager = config::settings::SettingsManager::create(dir.path(), &agent, false);
    (dir, manager)
}

#[test]
fn memorantado_url_comes_from_env_then_setting_then_default() {
    assert_eq!(memorantado_url(None, None).as_deref(), Some(DEFAULT_MEMORANTADO_URL));
    assert_eq!(
        memorantado_url(None, Some("http://m:1/")).as_deref(),
        Some("http://m:1")
    );
    assert_eq!(
        memorantado_url(Some("http://e:2"), Some("http://m:1")).as_deref(),
        Some("http://e:2")
    );
    assert_eq!(memorantado_url(Some("OFF"), Some("http://m:1")), None);
    assert_eq!(Choice::parse(" Memorantado "), Some(Choice::Memorantado));
    assert_eq!(Choice::parse("other"), None);
}

fn agent(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("agent")
}

fn up_endpoint(backend: Backend) -> Endpoint {
    match backend {
        Backend::Memorantado { endpoint, up: true } => endpoint,
        other => panic!("not memorantado (up): {other:?}"),
    }
}

#[test]
fn memorantado_is_chosen_by_setting_else_when_it_answers() {
    let none = |_: &str| None;
    let up = |_: &str| true;
    let down = |_: &str| false;
    let (d, unset) = manager("{}");
    assert_eq!(
        Endpoint::choose(&none, &unset, &agent(&d), Hindsight::Default, &down),
        Backend::Hindsight
    );
    assert_eq!(remembered(&agent(&d)), None, "nothing detected, nothing kept");
    assert_eq!(
        Endpoint::choose(&none, &unset, &agent(&d), Hindsight::Off, &up),
        Backend::Off,
        "memory turned off"
    );
    let endpoint = up_endpoint(Endpoint::choose(&none, &unset, &agent(&d), Hindsight::Default, &up));
    assert_eq!(endpoint.url, DEFAULT_MEMORANTADO_URL);
    assert_eq!(endpoint.project, "ultron");
    // A Hindsight server set explicitly is kept while memorantado was never chosen: nothing is probed or kept.
    let probed = |_: &str| -> bool { panic!("no detection over an explicit Hindsight") };
    let (d, fresh) = manager("{}");
    assert_eq!(
        Endpoint::choose(&none, &fresh, &agent(&d), Hindsight::Set, &probed),
        Backend::Hindsight
    );
    assert_eq!(remembered(&agent(&d)), None);
    let (d, hindsight) = manager(r#"{"memory": {"backend": "hindsight"}}"#);
    assert_eq!(
        Endpoint::choose(&none, &hindsight, &agent(&d), Hindsight::Default, &up),
        Backend::Hindsight
    );
    assert_eq!(remembered(&agent(&d)), None, "an explicit backend is not detected");
    let (d, picked) = manager(r#"{"memory": {"backend": "memorantado"}, "memorantadoUrl": "http://m:9"}"#);
    let endpoint = up_endpoint(Endpoint::choose(&none, &picked, &agent(&d), Hindsight::Off, &up));
    assert_eq!(endpoint.url, "http://m:9");
    let env = |name: &str| match name {
        "ULTRON_LSP" => Some("off".to_string()),
        "ULTRON_MEMORANTADO_URL" => Some("off".to_string()),
        _ => None,
    };
    assert_eq!(
        Endpoint::choose(&env, &picked, &agent(&d), Hindsight::Default, &up),
        Backend::Off,
        "memorantado chosen but turned off: no memory, not Hindsight"
    );
    let project = |name: &str| (name == "ULTRON_MEMORANTADO_PROJECT").then(|| "work".to_string());
    assert_eq!(
        up_endpoint(Endpoint::choose(&project, &picked, &agent(&d), Hindsight::Default, &up)).project,
        "work"
    );
}

/// The split-store fix: once memorantado is the backend (set, or detected and kept in the agent dir), a server that
/// does not answer pauses memory with a notice and Hindsight is never used instead.
#[test]
fn a_memorantado_outage_pauses_memory_instead_of_switching_to_hindsight() {
    let none = |_: &str| None;
    let up = |_: &str| true;
    let down = |_: &str| false;
    // Detected once: kept, and a later outage is memorantado, down.
    let (d, unset) = manager("{}");
    up_endpoint(Endpoint::choose(&none, &unset, &agent(&d), Hindsight::Default, &up));
    assert_eq!(remembered(&agent(&d)), Some(Choice::Memorantado));
    let paused = Endpoint::choose(&none, &unset, &agent(&d), Hindsight::Default, &down);
    let Backend::Memorantado { endpoint, up: false } = &paused else {
        panic!("a detected memorantado that is down must pause memory, not use Hindsight: {paused:?}");
    };
    assert_eq!(paused.memorantado(), Some(endpoint));
    let notice = endpoint.paused_notice();
    assert!(
        notice.contains("Memory is paused") && notice.contains(DEFAULT_MEMORANTADO_URL),
        "{notice}"
    );
    // Detected, it stays memorantado even where Hindsight is set explicitly.
    assert!(matches!(
        Endpoint::choose(&none, &unset, &agent(&d), Hindsight::Set, &down),
        Backend::Memorantado { up: false, .. }
    ));
    // Memory turned off still turns it off; an explicit `hindsight` still wins over the detection.
    assert_eq!(
        Endpoint::choose(&none, &unset, &agent(&d), Hindsight::Off, &down),
        Backend::Off
    );
    let mut hindsight = config::settings::SettingsManager::create(d.path(), &agent(&d), false);
    hindsight.set_global_nested("memory", "backend", Some(serde_json::json!("hindsight")));
    assert_eq!(
        Endpoint::choose(&none, &hindsight, &agent(&d), Hindsight::Default, &up),
        Backend::Hindsight
    );
    // Set explicitly: down is memorantado, down (the setting needs no detection).
    let (d, picked) = manager(r#"{"memory": {"backend": "memorantado"}}"#);
    assert!(matches!(
        Endpoint::choose(&none, &picked, &agent(&d), Hindsight::Default, &down),
        Backend::Memorantado { up: false, .. }
    ));
    assert_eq!(remembered(&agent(&d)), None);
}

#[test]
fn memorantado_health_check_over_loopback() {
    let fake = FakeMemorantado::new();
    let url = fake.serve();
    let transport = ReqwestTransport::new().unwrap();
    assert!(answers(&transport, &url));
    let closed = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    assert!(!answers(&transport, &format!("http://127.0.0.1:{port}")));
}
