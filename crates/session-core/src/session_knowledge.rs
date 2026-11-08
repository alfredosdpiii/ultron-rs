//! `knowledge.*` for a session ([`graph`]): the knowledge store under the agent directory, the session
//! directory's git repository, the global `graph.neo4j.*` settings read at each call, the memorantado server memory
//! lives in as the session chose it (memorantado::resolve, so a detected memorantado is mirrored too), and Neo4j's
//! password from the session's environment (`NEO4J_PASSWORD`) or the profile's auth.json (`neo4j`).

use crate::compose::Config;
use std::sync::Arc;

/// The knowledge module of a session whose memory lives in `memorantado` (`None`: Hindsight or no memory).
pub fn module(config: &Arc<Config>, memorantado: Option<memorantado::Endpoint>) -> graph::KnowledgeModule {
    let settings = config.settings_reader();
    let auth = config::paths::AgentPaths::new(&config.agent_dir).auth();
    let memory = memorantado.map(|endpoint| graph::MemorantadoSettings {
        url: format!("{}/api", endpoint.url),
        project: endpoint.project,
    });
    graph::KnowledgeModule::new(
        knowledge::default_path(&config.agent_dir),
        config.cwd.clone(),
        Arc::new(move || graph::Mirrors {
            neo4j: config::keys::neo4j_settings(settings().global_settings()),
            memorantado: memory.clone(),
        }),
        Arc::new(|| {
            let transport: Arc<dyn blocking_http::Transport> = Arc::new(blocking_http::ReqwestTransport::new()?);
            Ok(transport)
        }),
        judge::keys::key_source("neo4j", config.var.clone(), auth),
    )
}
