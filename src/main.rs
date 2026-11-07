use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use chrono::Local;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
struct Entry {
    ts: String,
    message: String,
}

#[derive(Parser)]
#[command(name = "agent-log", version, about = "Zaznamenává akce, které AI agent právě provádí")]
struct Cli {
    /// Cesta k souboru s logem
    #[arg(long, global = true, env = "AGENT_LOG_FILE", default_value = "agent.log")]
    file: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Zapíše novou zprávu do logu
    Add {
        /// Popis akce, kterou agent právě provádí
        message: String,
    },
    /// Vypíše záznamy z logu
    List {
        /// Kolik posledních záznamů ukázat (0 = všechny)
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// Vymaže celý log
    Clear,
}

fn add_entry(file: &PathBuf, message: &str) -> std::io::Result<()> {
    let entry = Entry {
        ts: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        message: message.to_string(),
    };
    let mut f = OpenOptions::new().create(true).append(true).open(file)?;
    let line = serde_json::to_string(&entry)?;
    writeln!(f, "{line}")?;
    println!("[{}] {}", entry.ts, entry.message);
    Ok(())
}

fn list_entries(file: &PathBuf, limit: usize) -> std::io::Result<()> {
    let f = match File::open(file) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("Log je prázdný.");
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    let entries: Vec<Entry> = BufReader::new(f)
        .lines()
        .filter_map(|l| l.ok())
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect();

    if entries.is_empty() {
        println!("Log je prázdný.");
        return Ok(());
    }

    let shown = if limit == 0 {
        &entries[..]
    } else {
        let start = entries.len().saturating_sub(limit);
        &entries[start..]
    };

    for e in shown {
        println!("[{}] {}", e.ts, e.message);
    }
    Ok(())
}

fn clear_log(file: &PathBuf) -> std::io::Result<()> {
    std::fs::write(file, "")?;
    println!("Log byl vymazán.");
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let result = match &cli.command {
        Command::Add { message } => add_entry(&cli.file, message),
        Command::List { limit } => list_entries(&cli.file, *limit),
        Command::Clear => clear_log(&cli.file),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Chyba: {e}");
            ExitCode::FAILURE
        }
    }
}
