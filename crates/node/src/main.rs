use std::path::PathBuf;
use std::process::ExitCode;

use nightgrid_node::config::{Config, DEFAULT_CONFIG};
use nightgrid_node::{cli, client, daemon, install};

const HELP: &str = "nightgrid-node: optional agent for the Nightgrid homelab app

USAGE:
    nightgrid-node                  (on a terminal: the menu)
    nightgrid-node <command> [options]

FIX THINGS:
    fix                     Menu: what looks wrong, every fix root allows,
                            and automation. Asks before anything runs
    fix list [--json]       The same as a list
    fix NAME [TARGET] [-y]  Run one fix (asks first; -y doesn't):
                              restart UNIT|CONTAINER  reset UNIT  start CONTAINER
                              journal  logs  packages  prune  ssh [USER]
                              reboot  run RUNBOOK
    fix auto                Menu: what the node may fix by itself
    fix auto NAME on|preview|off
                            Switch a preset (root): services, containers,
                            stopped, journal, packages, images

COMMANDS:
    run                     Run the service (systemd starts this)
    install                 Install or upgrade as a service (root): systemd, or
                            OpenRC/sysvinit without systemd's sandbox
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
    allow | deny            Menu: every access and whether it's on
    serve                   Menu: what paired peers may ask this node
    serve add|remove ping|ups|sync
                            Change what paired peers may ask this node ([peers] serve),
                            then restart the service and the link (root)
    netwatch                Run the network watch helper (its unit starts this)
    helper                  Serve one root helper request (its socket unit starts this)
    health-helper           Serve one health read (its socket unit starts this)
    checker                 Run the service checks (its unit starts this)
    peer                    Run the node-to-node link (its unit starts this)
    status-page             Serve the status page (its unit starts this)
    status                  Show what the running node reports: the host, active
                            alerts and access (--json: the raw node.info)
    events [--count N]      The last N events (default 30), oldest first
    audit [--count N]       The last N entries of the action log (default 30)
    audit verify            Check the action log's whole SHA-256 chain; exits 1
                            if any entry was edited or removed
    completions bash|zsh|fish
                            Print a shell completion script
    man                     Print the manual page (nightgrid-node man | man -l -)
    doctor                  Check this host's install: config, service, socket,
                            memory and each allowed access (read-only; exits 1
                            on a failure)
        --json              Print a machine-readable report
    query METHOD [JSON]     Send one request and print the answer
    watch STREAM... [--count N]
                            Print live frames (events, metrics, containers, usage)
                            as JSON lines; stop after N if given
    bridge                  Connect stdin/stdout to the node's socket
                            (what the app runs over SSH). If it can't, prints
                            one JSON error line (code no_access or not_running)
    version                 Print the version

OPTIONS:
    --config PATH           Config file (default /etc/nightgrid-node/config.toml)
    --socket PATH           Socket to connect to (default: from the config)
    -y, --yes               Don't ask (fix)
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
    yes: bool,
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
        yes: false,
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
            "-y" | "--yes" => a.yes = true,
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

fn connect(a: &Args, who: &str) -> Result<client::Client, String> {
    let path = socket(a);
    client::Client::connect(&path, who).map_err(|e| format!("{}: {e}", path.display()))
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
    if matches!(
        a.cmd.as_str(),
        "status"
            | "doctor"
            | "events"
            | "audit"
            | "query"
            | "watch"
            | "completions"
            | "man"
            | "version"
            | "help"
            | ""
            | "fix"
    ) {
        // Rust ignores SIGPIPE; let `nightgrid-node events | head` end quietly
        // instead of panicking on the closed pipe.
        // SAFETY: setting a signal's disposition to its default has no preconditions.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    if a.cmd.is_empty() && nightgrid_node::tui::interactive() {
        // `nightgrid-node` alone on a terminal: the home menu.
        while let Some(cmd) = nightgrid_node::menu::home() {
            let mut b = Args { cmd: cmd.to_owned(), ..parse_args()? };
            if cmd == "access" {
                b.cmd = "allow".into();
            }
            if let Err(e) = dispatch(b) {
                if !e.is_empty() {
                    eprintln!("nightgrid-node: {e}");
                }
            }
            if !matches!(cmd, "fix" | "access" | "serve") {
                nightgrid_node::tui::pause();
            }
        }
        return Ok(());
    }
    dispatch(a)
}

fn fix_ctx(a: &Args) -> nightgrid_node::menu::Ctx {
    nightgrid_node::menu::Ctx { config_path: a.config.clone(), socket: socket(a), yes: a.yes, json: a.json }
}

/// Runs the command. With `--json`, a failed install, uninstall, allow,
/// deny or serve also prints `{"error":{"code":...,"message":...}}` on
/// stdout (codes `no_systemd`, `not_root`, `failed`), for the app.
fn dispatch(a: Args) -> Result<(), String> {
    let json_errors = a.json && matches!(a.cmd.as_str(), "install" | "uninstall" | "allow" | "deny" | "serve");
    let r = run_command(a);
    if let (true, Err(e)) = (json_errors, &r) {
        println!("{}", install::error_json(e));
    }
    r
}

fn run_command(a: Args) -> Result<(), String> {
    match a.cmd.as_str() {
        "fix" => nightgrid_node::menu::fix(&fix_ctx(&a), &a.rest),
        "fix-apply" => match a.rest.as_slice() {
            [what, user] if what == "ssh-permissions" => {
                let msg = nightgrid_node::fixes::apply_ssh_permissions(user)?;
                println!("{msg}");
                Ok(())
            }
            _ => Err("fix-apply is run by the root helper".into()),
        },
        "allow" | "deny" if a.rest.is_empty() => nightgrid_node::menu::access(&a.config),
        "serve" if a.rest.is_empty() => nightgrid_node::menu::serve(&a.config),
        "run" => {
            let config = Config::load(&a.config)?;
            let mut paths = daemon::Paths::from_env();
            paths.fixes = nightgrid_node::fixes::path_for(&a.config);
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
        "serve" => {
            let (verb, name) = match a.rest.as_slice() {
                [v, n] if v == "add" || v == "remove" => (v.as_str(), n.as_str()),
                _ => return Err("usage: serve add|remove ping|ups|sync".into()),
            };
            let r = install::serve(&install::Layout::system(), &mut install::RealSys, name, verb == "add")?;
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
            install::preflight(&layout).inspect_err(|e| {
                if a.json {
                    println!("{}", install::error_json(e));
                }
            })?;
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
            let mut c = connect(&a, "nightgrid-node status")?;
            let info = c.call("node.info", serde_json::Value::Null).map_err(|e| e.to_string())?;
            if a.json {
                println!("{info}");
                return Ok(());
            }
            let info: nightgrid_node_proto::NodeInfo = serde_json::from_value(info).map_err(|e| e.to_string())?;
            let sample =
                c.call("metrics.latest", serde_json::Value::Null).ok().and_then(|v| serde_json::from_value(v).ok());
            let alerts: Vec<nightgrid_node_proto::Alert> = c
                .call("alerts.active", serde_json::Value::Null)
                .ok()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            print!("{}", cli::status(&info, sample.as_ref(), &alerts, daemon::now_secs()));
            Ok(())
        }
        "events" => {
            let mut c = connect(&a, "nightgrid-node events")?;
            let n = a.count.unwrap_or(30) as usize;
            let mut tail = std::collections::VecDeque::new();
            let mut after = 0;
            loop {
                let v = c.call("events.sync", serde_json::json!({ "after": after })).map_err(|e| e.to_string())?;
                let page: nightgrid_node_proto::EventsResult = serde_json::from_value(v).map_err(|e| e.to_string())?;
                for e in page.events {
                    tail.push_back(e);
                    if tail.len() > n {
                        tail.pop_front();
                    }
                }
                if !page.more || page.next == after {
                    break;
                }
                after = page.next;
            }
            let list: Vec<_> = tail.into();
            if a.json {
                list.iter().for_each(|e| println!("{}", serde_json::to_string(e).unwrap_or_default()));
            } else {
                print!("{}", cli::events(&list));
            }
            Ok(())
        }
        "audit" => {
            let verify = match a.rest.first().map(String::as_str) {
                None => false,
                Some("verify") => true,
                Some(x) => return Err(format!("audit {x}? (audit, or audit verify)")),
            };
            let mut c = connect(&a, "nightgrid-node audit")?;
            let n = a.count.unwrap_or(30) as usize;
            let mut tail = std::collections::VecDeque::new();
            let mut check = cli::Verify::default();
            let mut after = 0;
            loop {
                let v = c.call("actions.log", serde_json::json!({ "after": after })).map_err(|e| e.to_string())?;
                let page: nightgrid_node_proto::ActionsLogResult =
                    serde_json::from_value(v).map_err(|e| e.to_string())?;
                check.add(&page);
                for r in &page.records {
                    tail.push_back(r.clone());
                    if tail.len() > n {
                        tail.pop_front();
                    }
                }
                if !page.more || page.next == after {
                    break;
                }
                after = page.next;
            }
            if verify {
                if a.json {
                    println!("{}", serde_json::to_string(&check).unwrap_or_default());
                } else {
                    print!("{}", check.text());
                }
                return if check.ok() { Ok(()) } else { Err("the action log's chain is broken".into()) };
            }
            let list: Vec<_> = tail.into();
            if a.json {
                list.iter().for_each(|r| println!("{}", serde_json::to_string(r).unwrap_or_default()));
            } else {
                print!("{}", cli::audit(&list));
                if !check.ok() {
                    eprintln!("warning: the chain is broken (run: nightgrid-node audit verify)");
                }
            }
            Ok(())
        }
        "completions" => {
            let sh = a.rest.first().map(String::as_str).unwrap_or("");
            print!("{}", cli::completions(sh).ok_or("completions for which shell? (bash, zsh or fish)")?);
            Ok(())
        }
        "man" => {
            print!("{}", cli::man(HELP, env!("CARGO_PKG_VERSION")));
            Ok(())
        }
        "doctor" => {
            let r = nightgrid_node::doctor::run(
                &install::Layout::system(),
                &a.config,
                a.socket.as_deref(),
                &mut nightgrid_node::doctor::RealProbe,
            );
            if a.json {
                println!("{}", serde_json::to_string(&r).unwrap_or_default());
            } else {
                print!("{}", nightgrid_node::doctor::render(&r));
            }
            if r.worst() == nightgrid_node::doctor::Level::Fail {
                return Err("doctor found a failure".into());
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
            while count.is_none_or(|n| seen < n) {
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
            if !e.is_empty() {
                eprintln!("nightgrid-node: {e}");
            }
            ExitCode::FAILURE
        }
    }
}
