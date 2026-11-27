//! The `fonsviva` binary: the daemon (`serve`) and the CLI.

mod auth;
mod bench;
mod cli;
mod client;
mod discovery;
mod node;
mod org;
mod server;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "fonsviva",
    version = env!("FONSVIVA_VERSION"),
    about = "Run local models with every setting in plain view."
)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Detect hardware, pick a model that fits, download it and load it.
    Setup {
        /// Accept the plan without asking.
        #[arg(long, short)]
        yes: bool,
    },
    /// Ask the loaded model a question and show how fast it answered.
    Ask {
        prompt: String,
        /// Model to ask (default: the loaded one).
        #[arg(long, short)]
        model: Option<String>,
    },
    /// Daemon, loaded models and their speed.
    Status,
    /// What Fonsviva sees on this machine.
    Hw {
        #[arg(long)]
        json: bool,
    },
    /// Installed and recommended models.
    Models {
        #[command(subcommand)]
        cmd: ModelsCmd,
    },
    /// Load a model with explicit settings.
    Run {
        model: String,
        /// Context size in tokens (default: 32768 when it fits, else the largest that fits).
        #[arg(long)]
        ctx: Option<u64>,
        #[arg(long)]
        gpu_layers: Option<u32>,
        /// llama.cpp or ollama
        #[arg(long)]
        engine: Option<String>,
        /// Serve embeddings instead of chat.
        #[arg(long)]
        embeddings: bool,
    },
    /// Unload a model (all of them without a name), or stop the daemon.
    Stop {
        model: Option<String>,
        #[arg(long)]
        daemon: bool,
    },
    /// Check the pieces Fonsviva depends on.
    Doctor,
    /// Measure the links to other machines: route, and round trip bound and unbound.
    Topology {
        /// A peer as name=addr[,addr...], for example parish1=169.254.222.99,100.104.23.40
        #[arg(long = "peer", required = true)]
        peers: Vec<String>,
        /// Pings per measurement.
        #[arg(long, default_value_t = 20)]
        count: u32,
        #[arg(long)]
        json: bool,
        /// Also write the result as JSON to this file.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// Pair with another machine and carry engine traffic to it over one link.
    Node {
        #[command(subcommand)]
        cmd: NodeCmd,
    },
    /// Measure a model on one machine or a split and record the result.
    Bench {
        #[command(subcommand)]
        cmd: BenchCmd,
    },
    /// Run the daemon in the foreground.
    Serve,
    /// The org server: members, roles and sign-in links.
    Org {
        #[command(subcommand)]
        cmd: OrgCmd,
    },
}

#[derive(Subcommand)]
enum OrgCmd {
    /// Make a new org store and print a one-time admin link.
    Init {
        /// The org server's folder (made 0700).
        #[arg(long)]
        data: std::path::PathBuf,
        /// The address people reach the org at, for example https://fonsviva.example.org
        #[arg(long)]
        url: String,
    },
    /// Print a new one-time admin link and revoke unused older ones.
    AdminLink {
        #[arg(long)]
        data: std::path::PathBuf,
    },
    /// Run the org server.
    Serve {
        #[arg(long)]
        data: std::path::PathBuf,
        #[arg(long, default_value = "127.0.0.1:7350")]
        listen: std::net::SocketAddr,
        /// Listen on a non-loopback address.
        #[arg(long)]
        public: bool,
        /// Cloudflare fronts this loopback listener: count joins by CF-Connecting-IP.
        #[arg(long)]
        trust_cf: bool,
    },
}

#[derive(Subcommand)]
enum BenchCmd {
    /// Run llama-bench on a placement and write a topology result the ratchet checks.
    Topology {
        /// The fleet as measured now: {"fleet", "machines", "links"}.
        #[arg(long)]
        fleet: std::path::PathBuf,
        /// The GGUF model file.
        #[arg(long)]
        model: std::path::PathBuf,
        /// The machines the model runs on; this machine first for a split.
        #[arg(long = "on", required = true)]
        on: Vec<String>,
        /// The link a split runs over: cable, tailnet or lan.
        #[arg(long, value_parser = parse_link)]
        link: Option<fonsviva_core::topology::LinkKind>,
        /// For a split, the local end of `fonsviva node forward` to the other machine's rpc server.
        #[arg(long)]
        rpc: Option<String>,
        /// llama-bench devices, for example none for the CPU alone.
        #[arg(long)]
        device: Option<String>,
        /// The llama-bench binary; its build is the recorded engine version.
        #[arg(long)]
        llama_bench: std::path::PathBuf,
        /// Why this result is slower than an earlier one, when it is.
        #[arg(long)]
        note: Option<String>,
        /// Where results go.
        #[arg(long, default_value_os_t = bench::default_out())]
        out: std::path::PathBuf,
    },
}

fn parse_link(s: &str) -> Result<fonsviva_core::topology::LinkKind, String> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| format!("{s} is not cable, tailnet or lan"))
}

#[derive(Subcommand)]
enum NodeCmd {
    /// Without --peer, wait on --link for one attempt and show a code; with
    /// --peer and --code, join that node.
    Pair {
        /// The interface the link uses, for example bridge0 or thunderbolt0.
        #[arg(long)]
        link: String,
        #[arg(long, default_value_t = node::PAIR_PORT)]
        port: u16,
        /// The node to join, as addr:port.
        #[arg(long)]
        peer: Option<std::net::SocketAddrV4>,
        /// The code the node showed.
        #[arg(long)]
        code: Option<String>,
        /// This machine's name (default: its host name).
        #[arg(long = "as")]
        me: Option<String>,
        /// Replace an existing pairing with a machine of the same name.
        #[arg(long)]
        replace: bool,
    },
    /// Accept tunnels from a paired machine and carry them to a local engine.
    Serve {
        #[arg(long)]
        link: String,
        #[arg(long)]
        port: u16,
        /// The paired machine allowed in.
        #[arg(long)]
        peer: String,
        /// The local engine, on 127.0.0.1.
        #[arg(long)]
        to: std::net::SocketAddrV4,
    },
    /// Listen on 127.0.0.1 and carry each connection to a paired node.
    Forward {
        #[arg(long)]
        link: String,
        /// The paired node.
        #[arg(long)]
        peer: String,
        /// The node's channel, as addr:port.
        #[arg(long)]
        at: std::net::SocketAddrV4,
        /// The local end, on 127.0.0.1.
        #[arg(long)]
        local: std::net::SocketAddrV4,
    },
    /// A loopback echo server to measure the channel against.
    Echo {
        #[arg(long)]
        port: u16,
    },
    /// Measure round trips through an address that echoes.
    Rtt {
        #[arg(long)]
        to: std::net::SocketAddrV4,
        #[arg(long, default_value_t = 200)]
        count: u32,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ModelsCmd {
    /// Every GGUF model on this machine (Fonsviva, Ollama, LM Studio, Hugging Face cache).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Catalog models that fit this machine.
    Recommend {
        #[arg(long)]
        json: bool,
    },
    /// Download a catalog model.
    Pull { id: String },
    /// Remove a model.
    Rm {
        id: String,
        /// Confirm removing a model that belongs to Ollama.
        #[arg(long, short)]
        yes: bool,
    },
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let r = match args.cmd {
        Cmd::Serve => server::serve().await,
        Cmd::Org { cmd } => match cmd {
            OrgCmd::Init { data, url } => org::init(&data, &url),
            OrgCmd::AdminLink { data } => org::admin_link(&data),
            OrgCmd::Serve {
                data,
                listen,
                public,
                trust_cf,
            } => org::serve(&data, listen, public, trust_cf).await,
        },
        Cmd::Setup { yes } => cli::setup(yes).await,
        Cmd::Ask { prompt, model } => cli::ask(&prompt, model).await,
        Cmd::Status => cli::status().await,
        Cmd::Hw { json } => cli::hw(json),
        Cmd::Models { cmd } => match cmd {
            ModelsCmd::List { json } => cli::models_list(json),
            ModelsCmd::Recommend { json } => cli::recommend(json),
            ModelsCmd::Pull { id } => cli::pull(&id).await,
            ModelsCmd::Rm { id, yes } => cli::rm(&id, yes).await,
        },
        Cmd::Run {
            model,
            ctx,
            gpu_layers,
            engine,
            embeddings,
        } => {
            cli::run(server::RunReq {
                model,
                engine,
                context: ctx,
                gpu_layers,
                embeddings,
            })
            .await
        }
        Cmd::Stop { model, daemon } => cli::stop(model, daemon).await,
        Cmd::Doctor => cli::doctor().await,
        Cmd::Topology {
            peers,
            count,
            json,
            out,
        } => cli::topology(&peers, count, json, out.as_deref()).await,
        Cmd::Bench {
            cmd:
                BenchCmd::Topology {
                    fleet,
                    model,
                    on,
                    link,
                    rpc,
                    device,
                    llama_bench,
                    note,
                    out,
                },
        } => {
            bench::topology(bench::Topology {
                fleet,
                model,
                on,
                link,
                rpc,
                device,
                llama_bench,
                note,
                out,
            })
            .await
        }
        Cmd::Node { cmd } => match cmd {
            NodeCmd::Pair {
                link,
                port,
                peer,
                code,
                me,
                replace,
            } => node::pair(&link, port, peer, code, me, replace).await,
            NodeCmd::Serve {
                link,
                port,
                peer,
                to,
            } => node::serve(&link, port, &peer, to).await,
            NodeCmd::Forward {
                link,
                peer,
                at,
                local,
            } => node::forward(&link, &peer, at, local).await,
            NodeCmd::Echo { port } => node::echo(port).await,
            NodeCmd::Rtt { to, count, json } => node::rtt(to, count, json).await,
        },
    };
    if let Err(e) = r {
        eprintln!("fonsviva: {e:#}");
        std::process::exit(if e.is::<org::Refused>() { 2 } else { 1 });
    }
}
