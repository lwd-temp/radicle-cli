use rad_issue::{run, Options, HELP};
use radicle_terminal as term;

fn main() {
    term::run_command::<Options, _>(HELP, "Command", run);
}