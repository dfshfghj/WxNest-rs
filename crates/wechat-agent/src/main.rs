use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    let command = args.peek().cloned().unwrap_or_default();
    match command.as_str() {
        "mcp" => {
            args.next();
            run_mcp();
        }
        "watch" => {
            args.next();
            run_watch(args.collect());
        }
        "db" => run_db(),
        "" => run_mcp(),
        "-h" | "--help" | "help" => print_usage(),
        other if other.starts_with('-') => run_watch(
            std::iter::once(other.to_string())
                .chain(args.skip(1))
                .collect(),
        ),
        _ => print_usage(),
    }
}

fn run_mcp() {
    if let Err(error) = wechat_mcp::run() {
        eprintln!("wechat-agent mcp: {error}");
        std::process::exit(1);
    }
}

fn run_watch(args: Vec<String>) {
    let mut config: Option<PathBuf> = None;
    let mut once = false;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => config = iter.next().map(PathBuf::from),
            "--once" => once = true,
            other => eprintln!("wechat-agent watch: 忽略未知参数 {other}"),
        }
    }
    let Some(config) = config else {
        eprintln!("用法: wechat-agent watch --config <path> [--once]");
        std::process::exit(2);
    };
    if let Err(error) = wechat_watch::run(&config, once) {
        eprintln!("wechat-agent watch: {error}");
        std::process::exit(1);
    }
}

fn run_db() {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("wechat-agent db: {error}");
            std::process::exit(1);
        }
    };
    if let Err(error) = runtime.block_on(wce_self_broker::run()) {
        eprintln!("wechat-agent db: {error}");
        std::process::exit(1);
    }
}

fn print_usage() {
    eprintln!("用法: wechat-agent <mcp|watch|db> ...");
    eprintln!("  mcp                        stdio MCP 服务（默认）");
    eprintln!("  watch --config <path> [--once]");
    eprintln!("  db --root <db_storage> [--port 0] [--token <t>] ...");
}
