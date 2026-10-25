use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use clap::Parser;
use stalltop::fmt;
use stalltop::model::Snapshot;
use stalltop::procfs::LinuxProcs;
use stalltop::providers::{Roots, clip, collect, default_providers};

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

    // notifiche, TODO

    // without --once the table is printed again every interval
    loop {
        let snap = collect(&mut providers, &LinuxProcs, Utc::now());
        println!("{}", plain(&snap));
        std::thread::sleep(Duration::from_millis(args.interval));
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
