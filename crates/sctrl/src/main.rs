//! `sctrl`: S-CTRL. With no command, the floating window. Otherwise one
//! command per job, `--json` for scripts and agents; when an S-CTRL is
//! already running (it holds the radio's port), the command runs inside it.
//! Exit codes: 0 ok, 2 usage, 3 not connected, 4 refused by safety,
//! 5 timeout, 6 the radio reported an error.

mod cli;

use clap::Parser;
use serde_json::json;

use cli::{Cli, Cmd, Env};
use sctrl_core::ipc;
use sctrl_core::link::LinkOptions;
use sctrl_core::service::Service;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("SCTRL_LOG").unwrap_or_else(|_| "warn".into()))
        .init();
    let argv: Vec<String> = std::env::args().collect();
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let code = match &cli.cmd {
        None | Some(Cmd::Gui) => {
            eprintln!("sctrl: the window comes in the next build; `sctrl daemon` serves commands meanwhile");
            2
        }
        Some(Cmd::Daemon) => rt.block_on(daemon(cli.serial.clone())),
        Some(_) => rt.block_on(async {
            if !cli.direct
                && let Some(code) = ipc::forward(argv[1..].to_vec(), env!("SCTRL_BUILD_ID")).await
            {
                return code;
            }
            one(&cli, &mut Env::stdout()).await
        }),
    };
    std::process::exit(code);
}

/// One command; prints its error the way the caller asked.
async fn one(cli: &Cli, env: &mut Env) -> i32 {
    match cli::run(cli, env).await {
        Ok(()) => 0,
        Err(e) => {
            if cli.json {
                println!("{}", json!({"ok": false, "error": e.kind(), "message": e.message()}));
            } else {
                eprintln!("sctrl: {}", e.message());
            }
            e.code()
        }
    }
}

/// Hold the radio and serve `sctrl` commands over the socket.
async fn daemon(serial: Option<String>) -> i32 {
    let svc = Service::start(serial, LinkOptions::default());
    let s2 = svc.clone();
    let handler = move |argv: Vec<String>, sink: ipc::Sink| {
        let svc = s2.clone();
        async move {
            let full: Vec<String> = std::iter::once("sctrl".to_string()).chain(argv).collect();
            let cli = match Cli::try_parse_from(&full) {
                Ok(c) => c,
                Err(e) => {
                    let _ = sink.send(e.to_string());
                    return 2;
                }
            };
            if matches!(cli.cmd, Some(Cmd::Daemon) | Some(Cmd::Gui) | None) {
                let _ = sink.send("sctrl: an S-CTRL is already running".into());
                return 2;
            }
            let out = sink.clone();
            let mut env = Env::shared(
                svc,
                Box::new(move |s| {
                    let _ = out.send(s);
                }),
            );
            match cli::run(&cli, &mut env).await {
                Ok(()) => 0,
                Err(e) => {
                    let msg = if cli.json {
                        json!({"ok": false, "error": e.kind(), "message": e.message()}).to_string()
                    } else {
                        format!("sctrl: {}", e.message())
                    };
                    let _ = sink.send(msg);
                    e.code()
                }
            }
        }
    };
    let mut state = svc.watch_state();
    tokio::spawn(async move {
        loop {
            eprintln!("sctrl daemon: {}", state.borrow_and_update().summary());
            if state.changed().await.is_err() {
                return;
            }
        }
    });
    let r = tokio::select! {
        r = ipc::serve(env!("SCTRL_BUILD_ID"), handler) => r,
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    ipc::cleanup();
    if let Some(l) = svc.link() {
        l.close().await;
    }
    match r {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("sctrl daemon: {e:#}");
            3
        }
    }
}
