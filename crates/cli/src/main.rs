//! The `ultron` binary: composition root over the `cli` library and the daemon's internal processes
//! (`ultron __internal <role>`: the coordinator, a server generation, a session worker).

use std::path::PathBuf;

/// Session workers run for days on many threads, where glibc's per-thread arenas hold on to freed memory (an idle
/// worker reached 1.4 GB with a dozen 64 MB arenas); mimalloc returns freed pages to the system.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    if let Err(error) = signals::install() {
        eprintln!("Signal handling unavailable: {error}");
    }
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let [internal, role, args @ ..] = &argv[..]
        && internal == "__internal"
        && role == "inert-mcp"
    {
        if let Err(message) = cli::inert_mcp::run(args) {
            eprintln!("{message}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let source = match cli::assets::source(&|k| std::env::var(k).ok(), &home) {
        Ok(source) => source,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    };
    let ready_package = || match source.ready() {
        Ok(dir) => dir,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
    };
    let env = cli::Env {
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        var: std::sync::Arc::new(|k| std::env::var(k).ok()),
        package: source.dir(),
        home,
    };
    match daemon::process::role(&argv) {
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(1);
        }
        Ok(Some((role, args))) => {
            let result = match role {
                daemon::process::Role::Coordinator => {
                    daemon::process::trace_startup("coordinator.entry");
                    daemon::coordinator::run(args).map_err(|e| e.to_string())
                }
                daemon::process::Role::Server => {
                    daemon::server::run_server_process(args, Some(cli::radius_relay::relay_config(&env)))
                }
                daemon::process::Role::SessionWorker => {
                    daemon::process::trace_startup("worker.entry");
                    daemon::worker::run(args, ready_package())
                }
            };
            if let Err(message) = result {
                eprintln!("{message}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
        Ok(None) => {}
    }
    let invoked = std::env::args().next().unwrap_or_default();
    if cli::durable_client::is_home_command(&invoked) {
        let code = cli::durable_client::run_home(&argv, &env, &mut std::io::stdout(), &mut std::io::stderr());
        std::process::exit(code);
    }
    let code = cli::run(&argv, &env, &mut std::io::stdout(), &mut std::io::stderr());
    std::process::exit(code);
}
