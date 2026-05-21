use clap::{Parser, Subcommand};
use jewels::{redact, scan};
use std::io::{self, Read};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "jewels")]
#[command(about = "Scan and redact secrets from text or files", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Enable debug logging
    #[arg(short, long, global = true)]
    debug: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Scan input for secrets and list them
    Scan {
        /// Text to scan (if omitted, reads from stdin)
        input: Option<String>,
    },
    /// Redact secrets from input
    Redact {
        /// Text to redact (if omitted, reads from stdin)
        input: Option<String>,
    },
}

fn init_tracing(debug: bool) {
    let filter = if debug {
        EnvFilter::new("debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
    };

    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_target(false)
        .with_level(true)
        .without_time()
        .with_env_filter(filter)
        .compact()
        .init();
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.debug);

    match cli.command {
        Commands::Scan { input } => {
            let text = get_input(input)?;
            let matches = scan(&text);
            if matches.is_empty() {
                info!("No secrets found.");
            } else {
                for m in matches {
                    // Data output still goes to stdout for piping
                    println!("{}: {}", m.kind, m.value);
                }
            }
        }
        Commands::Redact { input } => {
            let text = get_input(input)?;
            let redacted = redact(&text);
            if redacted != text {
                warn!("Secrets detected and redacted.");
            } else {
                info!("No secrets detected.");
            }
            // Data output still goes to stdout for piping
            println!("{redacted}");
        }
    }

    Ok(())
}

fn get_input(input: Option<String>) -> io::Result<String> {
    if let Some(text) = input {
        Ok(text)
    } else {
        let mut buffer = String::new();
        io::stdin().read_to_string(&mut buffer)?;
        Ok(buffer)
    }
}
