use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::net::UdpSocket;

mod auth;
mod config;
mod connection;
mod ffmpeg;
#[allow(dead_code)]
mod groovy;
#[cfg(test)]
mod mock_server;
mod plex;
mod streamer;
mod test_pattern;

use groovy::MODELINES;

// ── CLI ──

#[derive(Parser)]
#[command(name = "groovy-cli", about = "Cast Plex media to MiSTer FPGA via Groovy protocol")]
struct Cli {
    #[arg(short, long, env = "GROOVY_MISTER")]
    mister: Option<String>,
    #[arg(short, long, env = "GROOVY_PLEX_SERVER")]
    server: Option<String>,
    #[arg(long, env = "GROOVY_PLEX_PORT")]
    port: Option<u16>,
    #[arg(short, long, env = "PLEX_TOKEN")]
    token: Option<String>,
    #[arg(long)]
    modeline: Option<String>,
    /// Scale video (0.5-1.0). 0.9 = 90% size with black border.
    #[arg(long)]
    scale: Option<f64>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Search { query: String },
    Episodes { name: String },
    Play {
        name: String,
        #[arg(short, long)] season: Option<u32>,
        #[arg(short, long)] episode: Option<u32>,
        /// Audio language (e.g. "japanese", "english", "jpn", "eng")
        #[arg(short, long)] audio: Option<String>,
        /// Subtitle language (e.g. "english", "eng", "none" to disable)
        #[arg(long)] subs: Option<String>,
        /// Seek to position in seconds
        #[arg(long)] seek: Option<f64>,
    },
    PlayKey {
        key: u64,
        #[arg(short, long)] audio: Option<String>,
        #[arg(long)] subs: Option<String>,
    },
    /// Play a local video file
    File {
        /// Path to video file
        path: String,
        /// Audio track index (0-based)
        #[arg(short, long)] audio: Option<u32>,
        /// Subtitle track index (0-based), or "none"
        #[arg(long)] subs: Option<String>,
        /// Seek to position in seconds
        #[arg(long)] seek: Option<f64>,
    },
    #[command(alias = "ondeck")]
    Continue,
    Libraries, Modelines, Config, Auth, Stop,
    /// Send a test pattern to MiSTer for modeline calibration
    TestPattern {
        /// How long to display in seconds
        #[arg(short, long, default_value = "60")]
        duration: u64,
    },
}

// ── Main ──

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = config::load();

    // Commands that don't need config resolution
    match &cli.command {
        Commands::Auth => {
            let token = auth::plex_oauth()?;
            let path = config::config_path();
            if let Some(p) = path.parent() { std::fs::create_dir_all(p)?; }
            let mut t = if path.exists() {
                std::fs::read_to_string(&path)?.parse::<toml::Table>().unwrap_or_default()
            } else { toml::Table::new() };
            t.insert("token".into(), toml::Value::String(token));
            std::fs::write(&path, toml::to_string_pretty(&t)?)?;
            println!("Token saved to {}", path.display());
            return Ok(());
        }
        Commands::Modelines => { for m in MODELINES { println!("{}", m.name); } return Ok(()); }
        Commands::Config => {
            let p = config::config_path();
            println!("Config: {}", p.display());
            if p.exists() { println!("{}", std::fs::read_to_string(&p)?); }
            else { println!("(not found)\n\nExample:\n  mister = \"192.168.0.115\"\n  server = \"192.168.0.29\"\n  port = 32400\n  token = \"your-token\"\n  modeline = \"640x480i NTSC\""); }
            return Ok(());
        }
        _ => {}
    }

    // Resolve config for commands that need Plex / MiSTer
    let rcfg = config::resolve(
        cli.mister.clone(), cli.server.clone(), cli.port,
        cli.token.clone(), cli.modeline.clone(), cli.scale, &cfg,
    )?;
    let plex = rcfg.plex();

    match &cli.command {
        Commands::Libraries => {
            for lib in &plex.libraries()? { println!("{}: {} (type={})", lib.key, lib.title, lib.lib_type); }
        }
        Commands::Search { query } => {
            let shows = plex.search(query)?;
            if shows.is_empty() { println!("No results for '{}'", query); }
            for s in &shows { println!("{} (key={}, library={})", s.title, s.rating_key, s.library); }
        }
        Commands::Episodes { name } => {
            let shows = plex.search(name)?;
            let show = shows.first().with_context(|| format!("No show found for '{}'", name))?;
            println!("{}:", show.title);
            for ep in &plex.get_episodes(show.rating_key)? {
                let w = if ep.view_count > 0 { "✓" } else { " " };
                println!("  [{}] S{}E{} - {} (key={})", w, ep.season, ep.episode, ep.title, ep.rating_key);
            }
        }
        Commands::Continue => {
            let items = plex.on_deck()?;
            if items.is_empty() { println!("Nothing on deck."); }
            else {
                println!("Continue watching:");
                for (i, item) in items.iter().enumerate() {
                    let off = if item.view_offset > 0 { format!(" ({}m in)", item.view_offset / 60_000) } else { String::new() };
                    if item.item_type == "episode" {
                        println!("  {}. {} — S{}E{}: {}{} (key={})", i+1, item.show_title, item.season, item.episode, item.title, off, item.rating_key);
                    } else {
                        println!("  {}. {}{} (key={})", i+1, item.title, off, item.rating_key);
                    }
                }
            }
        }
        Commands::Play { name, season, episode, audio, subs, seek } => {
            let shows = plex.search(name)?;
            let show = shows.first().with_context(|| format!("No show found for '{}'", name))?;
            let episodes = plex.get_episodes(show.rating_key)?;
            let ep = if let (Some(s), Some(e)) = (season, episode) {
                episodes.iter().find(|ep| ep.season == *s && ep.episode == *e)
                    .with_context(|| format!("S{}E{} not found", s, e))?
            } else {
                episodes.iter().find(|ep| ep.view_count == 0).or_else(|| episodes.first()).context("No episodes")?
            };
            println!("Playing: {} S{}E{} - {}", show.title, ep.season, ep.episode, ep.title);
            streamer::stream_to_mister(&plex, ep.rating_key, &rcfg.mister, &rcfg.modeline()?, rcfg.scale, audio.as_deref(), subs.as_deref(), *seek)?;
        }
        Commands::PlayKey { key, audio, subs } => {
            println!("Playing key={}", key);
            streamer::stream_to_mister(&plex, *key, &rcfg.mister, &rcfg.modeline()?, rcfg.scale, audio.as_deref(), subs.as_deref(), None)?;
        }
        Commands::File { path, audio, subs, seek } => {
            let modeline = rcfg.modeline()?;
            streamer::stream_file(path, &rcfg.mister, &modeline, rcfg.scale, *audio, subs.as_deref(), *seek)?;
        }
        Commands::Stop => {
            let sock = UdpSocket::bind("0.0.0.0:0")?;
            sock.send_to(&groovy::build_close(), format!("{}:{}", rcfg.mister, groovy::UDP_PORT))?;
            println!("Sent stop");
        }
        Commands::TestPattern { duration } => {
            let modeline = rcfg.modeline()?;
            test_pattern::send_test_pattern(&rcfg.mister, &modeline, rcfg.scale, *duration)?;
        }
        Commands::Modelines | Commands::Config | Commands::Auth => unreachable!(),
    }
    Ok(())
}
