use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use nightgrid_hub::config::{Config, DEFAULT_CONFIG, DEFAULT_DATA_DIR};
use nightgrid_hub::{collect, fleet, install, server, store};
use nightgrid_node::install::RealSys;

const HELP: &str = "nightgrid-hub: optional Nightgrid hub, one per network

USAGE:
    nightgrid-hub                   (on a terminal: the menu)
    nightgrid-hub <command> [options]

COMMANDS:
    run                     Run the hub (systemd starts this)
    collect                 Run the collector (its unit starts this)
    install                 Install or upgrade as a systemd service (root; needs
                            Nightgrid Node on this host)
        --user NAME         Let this login user talk to the hub (repeatable;
                            default: the user who ran sudo)
        --no-start          Enable at boot but don't start now
        --json              Print a machine-readable report
    uninstall               Stop and remove the hub (root; the node stays)
        --purge             Also remove the store, config and the service account
        --json              Print a machine-readable report
    allow collect           Start the collector: pulls the members over their
                            node-to-node link, read-only, and prints the hub's id
                            and fingerprint for pairing (root)
    deny collect            Stop the collector (root)
    allow | deny            Menu: the hub's access and whether it's on
    status                  The members and their state
    doctor                  Check this host's hub: config, units, socket,
                            collector and store (read-only; exits 1 on a
                            failure). With --config PATH, checks that file
        --json              Print a machine-readable report
    query METHOD [JSON]     Send one request to the hub and print the answer
    bridge                  Connect stdin/stdout to the hub's socket
                            (what the app runs over SSH)
    version                 Print the version

OPTIONS:
    --config PATH           Config file (default /etc/nightgrid-hub/config.toml)
    --socket PATH           Socket to connect to (default: from the config)
    --data-dir PATH         Store directory for `run` (default: systemd's
                            StateDirectory, else /var/lib/nightgrid-hub)
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
            .unwrap_or_else(|_| PathBuf::from(nightgrid_node_proto::hub::DEFAULT_SOCKET))
    })
}

fn connect(a: &Args) -> Result<nightgrid_node::client::Client, String> {
    let path = socket(a);
    nightgrid_node::client::Client::connect(&path, "nightgrid-hub cli").map_err(|e| format!("{}: {e}", path.display()))
}

fn print_report(r: &install::Report, json: bool) {
    if json {
        println!("{}", serde_json::to_string(r).unwrap_or_default());
        return;
    }
    for w in &r.warnings {
        eprintln!("warning: {w}");
    }
    println!("{} done (nightgrid-hub {})", r.action, r.version);
    if let Some(id) = &r.hub_id {
        println!("hub id: {id}");
    }
    if let Some(fp) = &r.fingerprint {
        println!("fingerprint: {fp}");
    }
    if r.listen_all.len() > 1 {
        println!("listening: {}", r.listen_all.join(", "));
    } else if let Some(l) = &r.listening {
        println!("listening: {l}");
    }
}

fn data_dir(a: &Args) -> PathBuf {
    a.data_dir.clone().unwrap_or_else(|| {
        std::env::var_os("STATE_DIRECTORY")
            .map(|s| PathBuf::from(s.to_string_lossy().split(':').next().unwrap_or_default().to_owned()))
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DATA_DIR))
    })
}

fn run(a: &Args) -> Result<(), String> {
    let mut config = Config::load(&a.config)?;
    if let Some(s) = &a.socket {
        config.server.socket = s.clone();
    }
    let dir = data_dir(a);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let st = store::Store::open(&dir.join("hub.db"))?;
    let hub = Arc::new(fleet::Hub::new(config, st));
    nightgrid_node::info!("nightgrid-hub {} (hub {})", env!("CARGO_PKG_VERSION"), hub.identity.node_id);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let listener = nightgrid_node::server::bind(&hub.config.server.socket)?;
        tokio::spawn(server::serve(listener, hub.clone()));
        nightgrid_node::daemon::sd_notify("READY=1");
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        let mut last_prune = 0i64;
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map_err(|e| e.to_string())?;
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = sigterm.recv() => break,
                _ = tokio::signal::ctrl_c() => break,
            }
            let h = hub.clone();
            let now = nightgrid_hub::now_secs();
            let prune = now - last_prune >= 3600;
            if prune {
                last_prune = now;
            }
            let r = tokio::task::spawn_blocking(move || -> Result<(), String> {
                fleet::check_offline(&h, now)?;
                if prune {
                    let max = u64::from(h.config.store.max_gb) * 1_000_000_000;
                    let (n, capped) =
                        h.store().prune(now, i64::from(h.config.store.minute_days), max).map_err(|e| e.to_string())?;
                    if n > 0 {
                        nightgrid_node::info!("pruned {n} old rows");
                    }
                    if capped {
                        let since = h.store().minutes_since().ok().flatten().unwrap_or(now);
                        let ev = serde_json::json!({"seq": 0, "at": now, "kind": "other", "level": "warn",
                            "title": format!("hub store at its {} GB cap", h.config.store.max_gb),
                            "detail": format!("keeping {} days of minutes", (now - since) / 86400)});
                        h.store().hub_event(now, ev).map_err(|e| e.to_string())?;
                    }
                }
                Ok(())
            })
            .await;
            match r {
                Ok(Err(e)) => nightgrid_node::warn!("hub upkeep: {e}"),
                Err(e) => nightgrid_node::warn!("hub upkeep: {e}"),
                Ok(Ok(())) => {}
            }
        }
        nightgrid_node::daemon::sd_notify("STOPPING=1");
        let _ = std::fs::remove_file(&hub.config.server.socket);
        Ok(())
    })
}

fn status(a: &Args) -> Result<(), String> {
    let mut c = connect(a)?;
    let v = c.call(nightgrid_node_proto::hub::method::OVERVIEW, serde_json::Value::Null).map_err(|e| e.to_string())?;
    if a.json {
        println!("{v}");
        return Ok(());
    }
    let o: nightgrid_node_proto::hub::Overview = serde_json::from_value(v).map_err(|e| e.to_string())?;
    println!("hub {} ({}), collector {}", c.welcome.node.hostname, o.hub_id, o.collector);
    if let Some(fp) = &o.fingerprint {
        let on = if o.listen_all.len() > 1 { Some(o.listen_all.join(", ")) } else { o.listening.clone() };
        println!("fingerprint {fp}{}", on.map(|l| format!(", listening on {l}")).unwrap_or_default());
    }
    println!("store {}", nightgrid_node::cli::bytes(o.store_bytes));
    if o.members.is_empty() {
        println!("no members yet (the app adds them with fleet.members.set)");
    }
    let now = nightgrid_hub::now_secs();
    for m in &o.members {
        let seen = m.last_contact.map_or("never".to_owned(), |t| format!("{} ago", nightgrid_node::cli::span(now - t)));
        let state = serde_json::to_value(m.state).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
        println!(
            "  {:<20} {:<12} seen {:<10} {} alert(s){}",
            m.name,
            state,
            seen,
            m.alerts,
            m.error.as_ref().map(|e| format!("  ({e})")).unwrap_or_default()
        );
    }
    Ok(())
}

fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

/// `nightgrid-hub allow` / `deny` with no name: the hub's access, on or off.
fn access_menu(a: &Args) -> Result<(), String> {
    use nightgrid_node::tui::{self, Item, Menu, Tone};
    const ABOUT: &str = "Pull the members over their node-to-node link, read-only, and print the hub's id and \
                         fingerprint for pairing.";
    loop {
        let l = install::Layout::system();
        let on = install::collect_allowed(&l);
        let mut m = Menu::new("hub", "access");
        m.subtitle = "what root lets the hub do".into();
        m.section("Access");
        m.item(
            Item::new("collect", "Collect from the members", ABOUT)
                .state(if on { Tone::Ok } else { Tone::Dim }, if on { "on" } else { "off" }),
        );
        if !tui::interactive() {
            println!("{}", m.plain_with_title());
            println!("\nChange it (root): nightgrid-hub allow collect | deny collect");
            return Ok(());
        }
        if tui::pick(&m).is_none() {
            return Ok(());
        }
        println!("\n{}\n  {ABOUT}", tui::banner("hub", "access · collect"));
        if euid() != 0 {
            println!(
                "{} Run: sudo nightgrid-hub {} collect",
                tui::pill(Tone::Dim, "root only"),
                if on { "deny" } else { "allow" }
            );
        } else if tui::confirm(if on { "Stop the collector?" } else { "Start the collector?" }, !on) {
            let r = if on {
                install::deny_collect(&l, &mut RealSys)
            } else {
                install::allow_collect(&l, &mut RealSys, a.no_start)
            };
            match r {
                Ok(rep) => print_report(&rep, false),
                Err(e) => println!("{} {e}", tui::pill(Tone::Danger, "error")),
            }
        }
        tui::pause();
    }
}

/// `nightgrid-hub` alone on a terminal.
fn home_menu() -> Option<&'static str> {
    use nightgrid_node::tui::{self, Item, Menu};
    const ENTRIES: &[(&str, &str, &str, &str)] = &[
        ("Look", "status", "Members and their state", "Every member node and when it was last pulled."),
        ("Settings", "access", "Access", "Start or stop the collector (allow/deny collect)."),
        ("Help", "help", "All commands", "The full command list."),
    ];
    let mut m = Menu::new("hub", "");
    m.subtitle = format!("v{}", env!("CARGO_PKG_VERSION"));
    let mut group = "";
    for (g, key, label, about) in ENTRIES {
        if *g != group {
            m.section(g);
            group = g;
        }
        m.item(Item::new(*key, *label, *about));
    }
    tui::pick(&m).map(|i| ENTRIES[i].1)
}

fn real_main() -> Result<(), String> {
    nightgrid_node::log::set_name("nightgrid-hub");
    let a = parse_args()?;
    if a.cmd.is_empty() && nightgrid_node::tui::interactive() {
        while let Some(cmd) = home_menu() {
            let b = Args { cmd: if cmd == "access" { "allow".into() } else { cmd.into() }, ..parse_args()? };
            if let Err(e) = dispatch(b) {
                eprintln!("nightgrid-hub: {e}");
            }
            if cmd != "access" {
                nightgrid_node::tui::pause();
            }
        }
        return Ok(());
    }
    dispatch(a)
}

/// Runs the command. With `--json`, a failed install, uninstall, allow or
/// deny also prints `{"error":{"code":...,"message":...}}` on stdout.
fn dispatch(a: Args) -> Result<(), String> {
    let json_errors = a.json && matches!(a.cmd.as_str(), "install" | "uninstall" | "allow" | "deny");
    let r = run_command(a);
    if let (true, Err(e)) = (json_errors, &r) {
        println!("{}", nightgrid_node::install::error_json(e));
    }
    r
}

fn run_command(a: Args) -> Result<(), String> {
    if matches!(a.cmd.as_str(), "status" | "doctor" | "query" | "version" | "help" | "") {
        // SAFETY: setting a signal's disposition to its default has no preconditions.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    match a.cmd.as_str() {
        "run" => run(&a),
        "collect" => collect::run(&a.config),
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
            let r = install::install(&install::Layout::system(), &mut RealSys, &exe, &users, a.no_start)?;
            print_report(&r, a.json);
            Ok(())
        }
        "uninstall" => {
            let r = install::uninstall(&install::Layout::system(), &mut RealSys, a.purge)?;
            print_report(&r, a.json);
            Ok(())
        }
        "allow" | "deny" if a.rest.is_empty() => access_menu(&a),
        "allow" | "deny" => {
            if a.rest.first().map(String::as_str) != Some("collect") {
                return Err(format!("{} what? (collect)", a.cmd));
            }
            let l = install::Layout::system();
            let r = if a.cmd == "allow" {
                install::allow_collect(&l, &mut RealSys, a.no_start)?
            } else {
                install::deny_collect(&l, &mut RealSys)?
            };
            print_report(&r, a.json);
            Ok(())
        }
        "status" => status(&a),
        "doctor" => {
            let r = nightgrid_hub::doctor::run(
                &install::Layout::system(),
                &a.config,
                a.socket.as_deref(),
                &mut nightgrid_hub::doctor::RealProbe,
            );
            if a.json {
                println!("{}", serde_json::to_string(&r).unwrap_or_default());
            } else {
                print!("{}", nightgrid_hub::doctor::render(&r));
            }
            if r.worst() == nightgrid_node::doctor::Level::Fail {
                return Err("doctor found a failure".into());
            }
            Ok(())
        }
        "query" => {
            let m = a.rest.first().ok_or("query METHOD [JSON]")?;
            let p = match a.rest.get(1) {
                Some(s) => serde_json::from_str(s).map_err(|e| format!("params: {e}"))?,
                None => serde_json::Value::Null,
            };
            let mut c = connect(&a)?;
            let v = c.call(m, p).map_err(|e| e.to_string())?;
            println!("{v}");
            Ok(())
        }
        "bridge" => nightgrid_node::client::bridge(&socket(&a)).map_err(|e| e.to_string()),
        "version" => {
            println!("nightgrid-hub {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "help" | "" => {
            print!("{HELP}");
            Ok(())
        }
        c => Err(format!("unknown command {c:?} (see nightgrid-hub help)")),
    }
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nightgrid-hub: {e}");
            ExitCode::FAILURE
        }
    }
}
