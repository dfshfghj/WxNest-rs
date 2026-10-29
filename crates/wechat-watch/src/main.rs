use std::path::PathBuf;

use clap::Parser;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    once: bool,
}

fn main() {
    let args = Args::parse();
    if let Err(error) = wechat_watch::run(&args.config, args.once) {
        eprintln!("wechat-watch: {error}");
        std::process::exit(1);
    }
}
