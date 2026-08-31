//! pie-proxy CLI — a standalone hostname-filtering egress proxy.

use clap::Parser;
use pie_proxy::{Policy, Target};
use std::net::SocketAddr;
use std::process::ExitCode;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[command(
    name = "pie-proxy",
    version,
    about = "Hostname-filtering egress proxy for pie sandboxes and piebox microVMs",
    after_help = "\
Point a process at it with the standard environment variables:

    HTTP_PROXY=http://127.0.0.1:8118 HTTPS_PROXY=http://127.0.0.1:8118 curl https://github.com

Rules: `github.com` matches that host exactly, `*.github.com` matches its
subdomains but not the parent, `*` matches everything. Deny always wins over
allow. With no rules at all the proxy is transparent; add any --allow rule and
everything unlisted is denied."
)]
struct Cli {
    /// Address to listen on.
    #[arg(
        long,
        short,
        env = "PIE_PROXY_LISTEN",
        default_value = "127.0.0.1:8118"
    )]
    listen: SocketAddr,

    /// Host pattern to allow. Repeatable. Any --allow enables allowlist mode.
    #[arg(long, short = 'a', env = "PIE_PROXY_ALLOW", value_delimiter = ',')]
    allow: Vec<String>,

    /// Host pattern to deny. Repeatable. Deny wins over allow.
    #[arg(long, short = 'd', env = "PIE_PROXY_DENY", value_delimiter = ',')]
    deny: Vec<String>,

    /// Reachable ports. Defaults to 80,443.
    #[arg(long, short = 'p', env = "PIE_PROXY_PORTS", value_delimiter = ',')]
    port: Vec<u16>,

    /// Report the verdict for one target and exit, without serving.
    #[arg(long, value_name = "HOST[:PORT]")]
    check: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let policy = match Policy::new(&cli.allow, &cli.deny) {
        Ok(policy) => policy.with_ports(cli.port),
        Err(err) => {
            eprintln!("pie-proxy: {err}");
            return ExitCode::FAILURE;
        }
    };

    // `--check` makes the policy testable without running a server, which is
    // also how a caller can confirm a rule set before trusting it.
    if let Some(authority) = cli.check {
        return match Target::parse_authority(&authority, Some(443)) {
            Ok(target) => {
                let decision = policy.evaluate(&target);
                match decision {
                    pie_proxy::Decision::Allow => {
                        println!("allow  {target}");
                        ExitCode::SUCCESS
                    }
                    pie_proxy::Decision::Deny(reason) => {
                        println!("deny   {target}  ({reason})");
                        ExitCode::FAILURE
                    }
                }
            }
            Err(err) => {
                eprintln!("pie-proxy: {err}");
                ExitCode::FAILURE
            }
        };
    }

    if let Err(err) = pie_proxy::serve(cli.listen, policy).await {
        eprintln!("pie-proxy: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
