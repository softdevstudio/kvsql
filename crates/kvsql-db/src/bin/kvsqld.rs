//! kvsqld: serve a KvSQL database over HTTP/JSON.

use std::io::Write;
use std::net::TcpListener;
use std::process::ExitCode;
use std::sync::Arc;

use kvsql_db::server::{self, Config};
use kvsql_engine::Db;

const USAGE: &str = "\
kvsqld: serve a KvSQL database over HTTP/JSON

Usage: kvsqld --db PATH [--listen ADDR] [--token TOKEN] [--max-connections N]

  --db PATH             database file (created if missing)
  --listen ADDR         address to listen on (default 127.0.0.1:7878)
  --token TOKEN         require 'Authorization: Bearer TOKEN' (or set KVSQL_TOKEN)
  --max-connections N   connections served at once (default 256)
  --version             print the version
  --help                print this text

The API is described in docs/server.md of the KvSQL repository.";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut db_path = None;
    let mut listen = "127.0.0.1:7878".to_string();
    let mut cfg = Config {
        token: std::env::var("KVSQL_TOKEN").ok().filter(|t| !t.is_empty()),
        ..Config::default()
    };
    while let Some(a) = args.next() {
        let mut value = |name: &str| {
            args.next().unwrap_or_else(|| {
                eprintln!("kvsqld: {name} needs a value");
                std::process::exit(2);
            })
        };
        match a.as_str() {
            "--db" => db_path = Some(value("--db")),
            "--listen" => listen = value("--listen"),
            "--token" => cfg.token = Some(value("--token")),
            "--max-connections" => match value("--max-connections").parse() {
                Ok(n) if n > 0 => cfg.max_connections = n,
                _ => {
                    eprintln!("kvsqld: --max-connections needs a positive number");
                    return ExitCode::from(2);
                }
            },
            "--version" | "-V" => {
                println!("kvsqld {}", kvsql_db::VERSION);
                return ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("kvsqld: unknown option {other}\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(db_path) = db_path else {
        eprintln!("kvsqld: --db PATH is required\n\n{USAGE}");
        return ExitCode::from(2);
    };
    let db = match Db::open(&db_path) {
        Ok(db) => Arc::new(db),
        Err(e) => {
            eprintln!("kvsqld: cannot open {db_path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let listener = match TcpListener::bind(&listen) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("kvsqld: cannot listen on {listen}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or(listen);
    // Scripts and tests read this line to find the port.
    println!(
        "kvsqld {} listening on {addr}, database {db_path}",
        kvsql_db::VERSION
    );
    let _ = std::io::stdout().flush();
    if cfg.token.is_none() && !addr.starts_with("127.") && !addr.starts_with("[::1]") {
        eprintln!("kvsqld: warning: no --token set and listening beyond localhost");
    }
    server::serve(listener, db, cfg);
    ExitCode::SUCCESS
}
