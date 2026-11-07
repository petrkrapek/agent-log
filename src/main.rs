use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use agent_log::{add_entry, clear_log, format_entry, list_entries};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "agent-log",
    version,
    about = "Zaznamenává akce, které AI agent právě provádí"
)]
struct Cli {
    /// Cesta k souboru s logem. Musí zůstat v aktuálním adresáři, pokud není `--allow-outside`.
    #[arg(
        long,
        global = true,
        env = "AGENT_LOG_FILE",
        default_value = "agent.log"
    )]
    file: PathBuf,

    /// Povolí cestu mimo aktuální adresář. Symlinky zůstávají zakázané.
    #[arg(long, global = true)]
    allow_outside: bool,

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

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        // Čtenář výstupu skončil dřív (`agent-log list | head`). Na logu se nic nepokazilo.
        Err(agent_log::Error::Io(e)) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Chyba: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), agent_log::Error> {
    let cli = Cli::parse();
    let base = std::env::current_dir()?;
    let mut out = io::stdout().lock();

    match &cli.command {
        Command::Add { message } => {
            let entry = add_entry(&base, &cli.file, cli.allow_outside, message)?;
            writeln!(out, "{}", format_entry(&entry))?;
        }
        Command::List { limit } => {
            list_entries(&base, &cli.file, cli.allow_outside, *limit, &mut out)?;
        }
        Command::Clear => {
            clear_log(&base, &cli.file, cli.allow_outside)?;
            writeln!(out, "Log byl vymazán.")?;
        }
    }
    out.flush()?;
    Ok(())
}
