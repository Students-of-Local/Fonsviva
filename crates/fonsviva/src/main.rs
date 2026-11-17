//! The `fonsviva` binary: the daemon (`serve`) and the CLI.

mod auth;
mod cli;
mod client;
mod discovery;
mod server;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "fonsviva",
    version,
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
    /// Run the daemon in the foreground.
    Serve,
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
    };
    if let Err(e) = r {
        eprintln!("fonsviva: {e:#}");
        std::process::exit(1);
    }
}
