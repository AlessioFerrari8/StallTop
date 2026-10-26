use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::Utc;
use clap::Parser;
use crossterm::event::{self, Event, KeyEventKind};
use stalltop::app::{Action, App};
use stalltop::fmt;
use stalltop::model::{Snapshot, Status};
use stalltop::procfs::LinuxProcs;
use stalltop::providers::{Provider, Roots, clip, collect, default_providers};
use stalltop::ui;

/// htop for your AI coding agents - and it tells you which one is stuck
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Refresh interval in ms
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(100..))]
    interval: u64,
    /// Do not send desktop notifications
    #[arg(long)]
    no_notify: bool,
    /// Do not ring the terminal bell on alerts
    #[arg(long)]
    no_bell: bool,
    /// Read agent data from this directory instead of $HOME
    #[arg(long)]
    home: Option<PathBuf>,
    /// Print one snapshot and exit
    #[arg(long)]
    once: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut providers = default_providers(&Roots::resolve(args.home.as_deref()));

    if args.once {
        let snap = collect(&mut providers, &LinuxProcs, Utc::now());
        println!("{}", plain(&snap));
        return Ok(());
    }

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut providers, &args);
    ratatui::restore();
    result
}

fn run(terminal: &mut ratatui::DefaultTerminal, providers: &mut [Box<dyn Provider>], args: &Args) -> Result<()> {
    let interval = Duration::from_millis(args.interval);
    let mut app = App::new(!args.no_notify, Utc::now());
    let mut alerted: HashSet<String> = HashSet::new();
    let mut next_poll = Instant::now();
    loop {
        if Instant::now() >= next_poll {
            let snap = collect(providers, &LinuxProcs, Utc::now());
            alert(&mut app, &mut alerted, &snap, args);
            app.set_snapshot(snap);
            next_poll = Instant::now() + interval;
        }
        terminal.draw(|f| ui::render(f, &app, Utc::now()))?;
        let wait = next_poll
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(250));
        if event::poll(wait)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && app.on_key(key) == Action::Quit
        {
            return Ok(());
        }
    }
}

/// Flashes, rings and notifies once for every session that starts needing attention
fn alert(app: &mut App, alerted: &mut HashSet<String>, snap: &Snapshot, args: &Args) {
    let stuck: Vec<_> = snap
        .sessions
        .iter()
        .filter(|s| matches!(s.status, Status::NeedsYou { .. } | Status::RateLimited { .. }))
        .collect();
    alerted.retain(|k| stuck.iter().any(|s| &s.key() == k));
    for s in stuck {
        if !alerted.insert(s.key()) {
            continue;
        }
        let msg = format!("{}: {}", s.project, fmt::status_detail(&s.status, snap.at));
        app.flash = Some((msg.clone(), snap.at));
        if !args.no_bell {
            print!("\x07");
        }
        if app.notify {
            // a missing notification daemon must not stop the monitor
            let _ = notify_rust::Notification::new().summary("stalltop").body(&msg).show();
        }
    }
}

/// A ps-like table
fn plain(snap: &Snapshot) -> String {
    let mut out = format!(
        "{:<20} {:<10} {:<14} {:>7} {:>8} {:>6}  DETAIL\n",
        "PROJECT", "STATUS", "MODEL", "TOKENS", "COST", "AGE"
    );
    for s in &snap.sessions {
        out += &format!(
            "{:<20} {:<10} {:<14} {:>7} {:>8} {:>6}  {}\n",
            clip(&s.project, 20),
            s.status.label(),
            fmt::model(s.model.as_deref()),
            fmt::tokens(s.tokens.total()),
            fmt::cost(s.cost_usd),
            fmt::age((snap.at - s.started_at).num_seconds()),
            fmt::status_detail(&s.status, snap.at),
        );
    }
    out += &format!(
        "today {} · {} tokens\n",
        fmt::cost(Some(snap.today.cost_usd)),
        fmt::tokens(snap.today.tokens.total())
    );
    out
}
