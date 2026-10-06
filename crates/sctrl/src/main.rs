//! `sctrl`: S-CTRL's command line. One command per job, `--json` for
//! scripts and agents. Exit codes: 0 ok, 2 usage, 3 not connected,
//! 4 refused by safety, 5 timeout, 6 the radio reported an error.

mod cli;

fn main() {
    std::process::exit(cli::main());
}
