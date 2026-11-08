//! `knowledge.*` for a session ([`graph`]): the knowledge store under the agent directory, the session
//! directory's git repository, the global `graph.neo4j.*` settings read at each call, the memorantado server memory
//! lives in as the session chose it (memorantado::resolve, so a detected memorantado is mirrored too), and Neo4j's
//! password from the session's environment (`NEO4J_PASSWORD`) or the profile's auth.json (`graph:neo4j`, bound to
//! the server it was saved for: config::auth::service_key).

use crate::compose::Config;
use std::sync::Arc;

/// The knowledge module of a session whose memory lives in `memorantado` (`None`: Hindsight or no memory).
/// `judge` is the session's judge: only a verdict whose receipt it issued is kept as the judge's.
pub fn module(
    config: &Arc<Config>,
    memorantado: Option<memorantado::Endpoint>,
    judge: Arc<judge::JudgeModule>,
) -> graph::KnowledgeModule {
    let settings = config.settings_reader();
    let auth = config::paths::AgentPaths::new(&config.agent_dir).auth();
    graph::KnowledgeModule::new(
        knowledge::default_path(&config.agent_dir),
        config.cwd.clone(),
        Arc::new(move || graph::Mirrors {
            neo4j: config::keys::neo4j_settings(settings().global_settings()),
            memorantado: memorantado.clone(),
        }),
        Arc::new(|| {
            let transport: Arc<dyn blocking_http::Transport> = Arc::new(blocking_http::ReqwestTransport::new()?);
            Ok(transport)
        }),
        config::auth::service_key_source(config::auth::NEO4J_PASSWORD, config.var.clone(), auth),
        Arc::new(move |receipt: &str| judge.issued(receipt)),
    )
}
