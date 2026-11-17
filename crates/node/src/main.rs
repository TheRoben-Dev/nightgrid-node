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
    upgrade FILE --sha256 HEX [--allow-downgrade]
                            Install a new binary if its checksum matches, check
                            the service stays up, else put the old one back (root)
    uninstall               Stop and remove the service (root)
        --purge             Also remove history, config and the service account
        --json              Print a machine-readable report
    allow docker            Let the service read containers through the engine
                            socket's group (root; root-equivalent access)
    allow journal           Let the service read the journal for SSH logins (root)
    allow netwatch          Start the listen-only helper for ARP and DHCP (root; CAP_NET_RAW)
    allow helper            Install the root helper for unit and power actions (root)
    allow health            Install the read-only helper for SMART, pools, updates
                            and UPS status (root)
    allow power             Let the service read the CPU energy counters (RAPL)
                            for power draw (root)
    allow checks            Install the check runner for the app's service checks,
                            with network access to [checks] networks only (root)
    allow peers             Start the node-to-node link: mutual TLS with the nodes the
                            app paired, on the tailnet address only (root)
    allow statuspage        Start a read-only status page on the tailnet address and print
                            its secret link (root; again to see the link)
    deny docker|journal|netwatch|helper|health|power|checks|peers|statuspage
                            Take that access away again (root)
    netwatch                Run the network watch helper (its unit starts this)
    helper                  Serve one root helper request (its socket unit starts this)
    health-helper           Serve one health read (its socket unit starts this)
    checker                 Run the service checks (its unit starts this)
    peer                    Run the node-to-node link (its unit starts this)
    status-page             Serve the status page (its unit starts this)
    status                  Show what the running node reports
    query METHOD [JSON]     Send one request and print the answer
    watch STREAM... [--count N]
                            Print live frames (events, metrics, containers, usage)
                            as JSON lines; stop after N if given
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
    count: Option<u64>,
    sha256: Option<String>,
    allow_downgrade: bool,
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
        count: None,
        sha256: None,
        allow_downgrade: false,
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
            "--count" => a.count = Some(val("--count")?.parse().map_err(|_| "--count needs a number")?),
            "--sha256" => a.sha256 = Some(val("--sha256")?.to_ascii_lowercase()),
            "--allow-downgrade" => a.allow_downgrade = true,
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
        if let Some(u) = &r.url {
            println!("status page: {u}");
        }
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
        "allow" | "deny" => {
            let what = a.rest.first().ok_or(
                "allow/deny what? (docker, journal, netwatch, helper, health, power, checks, peers, statuspage)",
            )?;
            let grant = install::Grant::parse(what).ok_or_else(|| {
                format!("unknown access {what:?} (docker, journal, netwatch, helper, health, power, checks, peers, statuspage)")
            })?;
            let layout = install::Layout::system();
            let r = if a.cmd == "allow" {
                let config = Config::load(&a.config)?;
                install::allow(&layout, &mut install::RealSys, grant, &config.docker.socket, a.no_start)?
            } else {
                install::deny(&layout, &mut install::RealSys, grant, a.no_start)?
            };
            print_report(&r, a.json);
            Ok(())
        }
        "helper" => nightgrid_node::helper::serve(&a.config),
        "health-helper" => nightgrid_node::health::watch::serve(&a.config),
        "checker" => nightgrid_node::checks::checker(&a.config),
        "peer" => nightgrid_node::peers::link(&a.config),
        "status-page" => nightgrid_node::statuspage::serve(&a.config),
        "netwatch" => {
            let mut config = Config::load(&a.config)?;
            if let Some(s) = &a.socket {
                config.server.socket = s.clone();
            }
            nightgrid_node::sniff::run(&config)
        }
        "upgrade" => {
            let file = a.rest.first().ok_or("upgrade needs the new binary: upgrade FILE --sha256 HEX")?;
            let opts = nightgrid_node::upgrade::UpgradeOptions {
                sha256: a.sha256.clone().ok_or("upgrade needs --sha256 (from the release's SHA256SUMS)")?,
                allow_downgrade: a.allow_downgrade,
            };
            let sock = socket(&a);
            let mut probe = nightgrid_node::upgrade::SocketProbe { socket: sock.clone() };
            let layout = install::Layout::system();
            install::preflight(&layout)?;
            let r = nightgrid_node::upgrade::upgrade(
                &layout,
                &mut install::RealSys,
                &mut probe,
                std::path::Path::new(file),
                &opts,
            )?;
            nightgrid_node::upgrade::note(&sock, &r);
            if a.json {
                println!("{}", serde_json::to_string(&r).unwrap_or_default());
            } else {
                for s in &r.steps {
                    println!("{s}");
                }
            }
            match (&r.error, r.ok) {
                (_, true) => Ok(()),
                (Some(e), false) => Err(e.clone()),
                (None, false) => Err("upgrade failed".into()),
            }
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
        "watch" => {
            let streams = a.rest.clone();
            let count = a.count;
            if streams.is_empty() {
                return Err("watch what? (events, metrics, containers)".into());
            }
            let path = socket(&a);
            let mut c = client::Client::connect(&path, "nightgrid-node watch")
                .map_err(|e| format!("{}: {e}", path.display()))?;
            c.call("subscribe", serde_json::json!({ "streams": streams })).map_err(|e| e.to_string())?;
            let mut seen = 0;
            while count.map_or(true, |n| seen < n) {
                let f = c.next_frame().map_err(|e| e.to_string())?;
                println!("{}", serde_json::to_string(&f).unwrap_or_default());
                seen += 1;
            }
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
