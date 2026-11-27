//! The `ultron` binary: composition root over the `cli` library and the daemon's internal processes
//! (`ultron __internal <role>`: the coordinator, a server generation, a session worker).

use std::path::PathBuf;

/// Session workers run for days on many threads, where glibc's per-thread arenas hold on to freed memory (an idle
/// worker reached 1.4 GB with a dozen 64 MB arenas); mimalloc returns freed pages to the system.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn exit_err(message: impl std::fmt::Display) -> ! {
    eprintln!("{message}");
    std::process::exit(1);
}

fn maybe_run_inert_mcp(argv: &[String]) {
    let [internal, role, args @ ..] = argv else {
        return;
    };
    if internal != "__internal" || role != "inert-mcp" {
        return;
    }
    if let Err(message) = cli::inert_mcp::run(args) {
        exit_err(message);
    }
    std::process::exit(0);
}

fn maybe_run_daemon_role(argv: &[String], env: &cli::Env, source: &cli::assets::Source) {
    let role = match daemon::process::role(argv) {
        Err(message) => exit_err(message),
        Ok(Some(found)) => found,
        Ok(None) => return,
    };
    let result = match role.0 {
        daemon::process::Role::Coordinator => {
            daemon::process::trace_startup("coordinator.entry");
            daemon::coordinator::run(role.1).map_err(|e| e.to_string())
        }
        daemon::process::Role::Server => {
            daemon::server::run_server_process(role.1, Some(cli::radius_relay::relay_config(env)))
        }
        daemon::process::Role::SessionWorker => {
            daemon::process::trace_startup("worker.entry");
            let package = source.ready().unwrap_or_else(|message| exit_err(message));
            daemon::worker::run(role.1, package)
        }
    };
    if let Err(message) = result {
        exit_err(message);
    }
    std::process::exit(0);
}

fn main() {
    if let Err(error) = signals::install() {
        eprintln!("Signal handling unavailable: {error}");
    }
    let argv: Vec<String> = std::env::args().skip(1).collect();
    maybe_run_inert_mcp(&argv);
    let home = std::env::var_os("HOME").map_or_default(PathBuf::from);
    let source = cli::assets::source(&|k| std::env::var(k).ok(), &home).unwrap_or_else(|message| exit_err(message));
    let env = cli::Env {
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        var: std::sync::Arc::new(|k| std::env::var(k).ok()),
        package: source.dir(),
        home,
    };
    maybe_run_daemon_role(&argv, &env, &source);
    let invoked = std::env::args().next().unwrap_or_default();
    let code = if cli::durable_client::is_home_command(&invoked) {
        cli::durable_client::run_home(&argv, &env, &mut std::io::stdout(), &mut std::io::stderr())
    } else {
        cli::run(&argv, &env, &mut std::io::stdout(), &mut std::io::stderr())
    };
    std::process::exit(code);
}
