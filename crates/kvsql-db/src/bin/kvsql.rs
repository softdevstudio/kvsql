//! kvsql: a shell for KvSQL, against a server or a database file.

use std::io::{self, IsTerminal};
use std::process::ExitCode;

use kvsql_db::http::Client;
use kvsql_db::shell::{self, Backend};
use kvsql_engine::Db;

const USAGE: &str = "\
kvsql: SQL and key-value shell for KvSQL

Usage: kvsql [--url URL | --file PATH] [--token TOKEN] [-c COMMAND]...

  --url URL       kvsqld to talk to (default http://127.0.0.1:7878, or KVSQL_URL)
  --file PATH     open a database file directly instead of a server
  --token TOKEN   bearer token for the server (or KVSQL_TOKEN)
  -c COMMAND      run one command and exit; may be repeated
  --version       print the version
  --help          print this text

With no -c, commands are read from standard input. Type .help inside the
shell for the key-value commands.";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut url = std::env::var("KVSQL_URL").unwrap_or_else(|_| "http://127.0.0.1:7878".into());
    let mut file = None;
    let mut token = std::env::var("KVSQL_TOKEN").ok().filter(|t| !t.is_empty());
    let mut commands = Vec::new();
    while let Some(a) = args.next() {
        let mut value = |name: &str| {
            args.next().unwrap_or_else(|| {
                eprintln!("kvsql: {name} needs a value");
                std::process::exit(2);
            })
        };
        match a.as_str() {
            "--url" => url = value("--url"),
            "--file" => file = Some(value("--file")),
            "--token" => token = Some(value("--token")),
            "-c" => commands.push(value("-c")),
            "--version" | "-V" => {
                println!("kvsql {}", kvsql_db::VERSION);
                return ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("kvsql: unknown option {other}\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let mut backend = match file {
        Some(path) => match Db::open(&path) {
            Ok(db) => Backend::Local(db),
            Err(e) => {
                eprintln!("kvsql: cannot open {path}: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => match Client::new(&url, token) {
            Ok(c) => Backend::Remote(c),
            Err(e) => {
                eprintln!("kvsql: {e}");
                return ExitCode::from(2);
            }
        },
    };
    if !commands.is_empty() {
        let mut failed = false;
        for c in &commands {
            match shell::run_line(&mut backend, c) {
                None => break,
                Some(Ok(s)) => {
                    if !s.is_empty() {
                        println!("{s}");
                    }
                }
                Some(Err(e)) => {
                    eprintln!("{e}");
                    failed = true;
                }
            }
        }
        return if failed {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        };
    }
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    if interactive {
        println!(
            "kvsql {}. Type .help for commands, .quit to leave.",
            kvsql_db::VERSION
        );
    }
    match shell::repl(
        &mut backend,
        &mut stdin.lock(),
        &mut io::stdout(),
        interactive,
    ) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) if interactive => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("kvsql: {e}");
            ExitCode::FAILURE
        }
    }
}
