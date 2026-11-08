//! Which backend a process uses: `memory.backend`, else memorantado when its server answers, else Hindsight.

use crate::fake::FakeMemorantado;
use memorantado::resolve::{Choice, Endpoint, answers, memorantado_url};
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

#[test]
fn memorantado_is_chosen_by_setting_else_when_it_answers() {
    let none = |_: &str| None;
    let up = |_: &str| true;
    let down = |_: &str| false;
    let (_d, unset) = manager("{}");
    let endpoint = Endpoint::choose(&none, &unset, false, &up).unwrap();
    assert_eq!(endpoint.url, DEFAULT_MEMORANTADO_URL);
    assert_eq!(endpoint.project, "ultron");
    assert_eq!(Endpoint::choose(&none, &unset, false, &down), None);
    assert_eq!(Endpoint::choose(&none, &unset, true, &up), None, "memory turned off");
    let (_d, hindsight) = manager(r#"{"memory": {"backend": "hindsight"}}"#);
    assert_eq!(Endpoint::choose(&none, &hindsight, false, &up), None);
    let (_d, picked) = manager(r#"{"memory": {"backend": "memorantado"}, "memorantadoUrl": "http://m:9"}"#);
    let endpoint = Endpoint::choose(&none, &picked, true, &down).unwrap();
    assert_eq!(endpoint.url, "http://m:9");
    let env = |name: &str| match name {
        "ULTRON_MEMORANTADO_URL" => Some("off".to_string()),
        _ => None,
    };
    assert_eq!(Endpoint::choose(&env, &picked, false, &up), None);
    let project = |name: &str| (name == "ULTRON_MEMORANTADO_PROJECT").then(|| "work".to_string());
    assert_eq!(Endpoint::choose(&project, &picked, false, &up).unwrap().project, "work");
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
