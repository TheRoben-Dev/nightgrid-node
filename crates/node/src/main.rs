use std::path::PathBuf;
use std::process::ExitCode;

use nightgrid_node::config::{Config, DEFAULT_CONFIG};
use nightgrid_node::{client, daemon, install};

const HELP: &str = "nightgrid-node: optional agent for the Nightgrid homelab app

USAGE:
    nightgrid-node <command> [options]

COMMANDS:
    run                     Run the service (systemd starts this)
    install                 Install or upgrade as a systemd service (root)
        --user NAME         Let this login user talk to the node (repeatable;
                            default: the user who ran sudo)
        --no-start          Enable at boot but don't start now
        --json              Print a machine-readable report
    uninstall               Stop and remove the service (root)
        --purge             Also remove history, config and the service account
        --json              Print a machine-readable report
    status                  Show what the running node reports
    query METHOD [JSON]     Send one request and print the answer
    bridge                  Connect stdin/stdout to the node's socket
                            (what the app runs over SSH)
    version                 Print the version

OPTIONS:
    --config PATH           Config file (default /etc/nightgrid-node/config.toml)
    --socket PATH           Socket to connect to (default: from the config)
    --data-dir PATH         History directory for `run` (default: systemd's
                            StateDirectory, else /var/lib/nightgrid-node)
";

struct Args {
    cmd: String,
    rest: Vec<String>,
    config: PathBuf,
    socket: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    users: Vec<String>,
    json: bool,
    purge: bool,
    no_start: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        cmd: String::new(),
        rest: Vec::new(),
        config: PathBuf::from(DEFAULT_CONFIG),
        socket: None,
        data_dir: None,
        users: Vec::new(),
        json: false,
        purge: false,
        no_start: false,
    };
    while let Some(x) = it.next() {
        let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match x.as_str() {
            "--config" => a.config = PathBuf::from(val("--config")?),
            "--socket" => a.socket = Some(PathBuf::from(val("--socket")?)),
            "--data-dir" => a.data_dir = Some(PathBuf::from(val("--data-dir")?)),
            "--user" => a.users.push(val("--user")?),
            "--json" => a.json = true,
            "--purge" => a.purge = true,
            "--no-start" => a.no_start = true,
            "-h" | "--help" => a.cmd = "help".into(),
            "-V" | "--version" => a.cmd = "version".into(),
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            _ if a.cmd.is_empty() => a.cmd = x,
            _ => a.rest.push(x),
        }
    }
    Ok(a)
}

fn socket(a: &Args) -> PathBuf {
    a.socket.clone().unwrap_or_else(|| {
        Config::load(&a.config)
            .map(|c| c.server.socket)
            .unwrap_or_else(|_| PathBuf::from(nightgrid_node_proto::DEFAULT_SOCKET))
    })
}

fn print_report(r: &install::Report, json: bool) {
    if json {
        println!("{}", serde_json::to_string(r).unwrap_or_default());
    } else {
        for w in &r.warnings {
            eprintln!("warning: {w}");
        }
        println!("{} done (nightgrid-node {})", r.action, r.version);
    }
}

fn real_main() -> Result<(), String> {
    let a = parse_args()?;
    match a.cmd.as_str() {
        "run" => {
            let config = Config::load(&a.config)?;
            let mut paths = daemon::Paths::from_env();
            if let Some(d) = &a.data_dir {
                paths.data_dir = d.clone();
            }
            let mut config = config;
            if let Some(s) = &a.socket {
                config.server.socket = s.clone();
            }
            daemon::run(config, paths)
        }
        "install" => {
            let mut users = a.users.clone();
            if users.is_empty() {
                if let Ok(u) = std::env::var("SUDO_USER") {
                    if !u.is_empty() && u != "root" {
                        users.push(u);
                    }
                }
            }
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let opts = install::InstallOptions { allow_users: users, no_start: a.no_start };
            let r = install::install(&install::Layout::system(), &mut install::RealSys, &exe, &opts)?;
            print_report(&r, a.json);
            Ok(())
        }
        "uninstall" => {
            let r = install::uninstall(&install::Layout::system(), &mut install::RealSys, a.purge)?;
            print_report(&r, a.json);
            Ok(())
        }
        "bridge" => client::bridge(&socket(&a)).map_err(|e| format!("{}: {e}", socket(&a).display())),
        "status" => {
            let path = socket(&a);
            let mut c = client::Client::connect(&path, "nightgrid-node status")
                .map_err(|e| format!("{}: {e}", path.display()))?;
            let info = c.call("node.info", serde_json::Value::Null).map_err(|e| e.to_string())?;
            if a.json {
                println!("{info}");
            } else {
                println!("{}", serde_json::to_string_pretty(&info).unwrap_or_default());
            }
            Ok(())
        }
        "query" => {
            let method = a.rest.first().ok_or("query needs a method")?;
            let params = match a.rest.get(1) {
                Some(p) => serde_json::from_str(p).map_err(|e| format!("params: {e}"))?,
                None => serde_json::Value::Null,
            };
            let path = socket(&a);
            let mut c = client::Client::connect(&path, "nightgrid-node query")
                .map_err(|e| format!("{}: {e}", path.display()))?;
            let v = c.call(method, params).map_err(|e| e.to_string())?;
            println!("{v}");
            Ok(())
        }
        "version" => {
            println!("nightgrid-node {} (protocol {})", env!("CARGO_PKG_VERSION"), nightgrid_node_proto::PROTO_VERSION);
            Ok(())
        }
        "help" | "" => {
            print!("{HELP}");
            Ok(())
        }
        other => Err(format!("unknown command {other} (try --help)")),
    }
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nightgrid-node: {e}");
            ExitCode::FAILURE
        }
    }
}
