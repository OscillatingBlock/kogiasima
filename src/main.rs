use std::env;
use std::process;

use mini_docker::{Container, run};

fn main() {
    let args: Vec<String> = env::args().collect();
    let command = parse_command(&args);

    match command.as_str() {
        "run" => {
            let cmd_args = Vec::from(&args[2..]);

            let Ok(container) = Container::build(cmd_args) else {
                eprintln!("error: failed to create container");
                process::exit(1);
            };
            container.run();
        }
        _ => {
            eprintln!("error: unknown command '{}'", command);
            process::exit(1);
        }
    }
}

fn parse_command(args: &Vec<String>) -> &String {
    if args.len() < 2 || (args.get(1).unwrap().eq("run") && args.len() < 3) {
        eprintln!("error: you must provide at least  two arguments with `mini-docker run`");
        process::exit(1);
    }

    let command = args.get(1).unwrap_or_else(|| {
        eprintln!("error: you must provide a command to run");
        process::exit(1);
    });
    command
}
