//! Model prices for `budget.estimate` and the stop-line of workflow nodes (`delegation::estimate`): a unit's own
//! model, else its kind's default in this session, priced from the built-in catalog or models.json. A judge has no
//! default model here (its backends are not catalog models), so an unnamed one stays unpriced.

use crate::RunEnv as Env;
use crate::compose::Config;
use ai::Model;
use delegation::estimate::{Priced, Pricing, Rates};
use jsrt::sync;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn rates(model: &Model) -> Rates {
    Rates {
        input: model.cost.input,
        output: model.cost.output,
    }
}

/// Prices units of this session: a subagent defaults to `rlm.childModel`, a frame to `ULTRON_RLM_FRAME_MODEL` or
/// `rlm.frameModel` (as frames resolve them), and each of them and a completion to the session's model as it is now.
pub fn pricing(config: &Config, root_model: Arc<Mutex<Model>>) -> Pricing {
    let env = Env {
        cwd: config.cwd.clone(),
        home: config.home.clone(),
        var: config.var.clone(),
        package: PathBuf::new(),
    };
    let settings = config.settings_reader();
    let frame_env = config.frame_env.clone();
    Arc::new(move |kind: &str, model: Option<&str>| {
        let configured = || {
            let rlm = config::keys::rlm_model_settings(settings().global_settings());
            match kind {
                "child" => rlm.child_model,
                "frame" => config::env::env_model(&|key| frame_env.get(key).cloned(), config::env::RLM_FRAME_MODEL_ENV)
                    .or(rlm.frame_model),
                _ => None,
            }
        };
        let spec = match model {
            Some(model) => model.to_string(),
            None if kind == "judge" => return Priced::default(),
            None => match configured() {
                Some(model) => model,
                None => {
                    let root = sync::lock(&root_model).clone();
                    return Priced {
                        model: Some(format!("{}/{}", root.provider, root.id)),
                        rates: Some(rates(&root)),
                    };
                }
            },
        };
        let found = spec
            .split_once('/')
            .and_then(|(provider, id)| crate::lane::catalog_model(provider, id, &env).ok());
        Priced {
            rates: found.as_ref().map(rates),
            model: Some(spec),
        }
    })
}
