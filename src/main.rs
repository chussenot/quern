fn main() {
    // `quern [--db <dir>] [script.sql]`. Arg parsing, the line loop and the
    // output format all live in repl.rs so the .slt harness can call them too;
    // this stays a one-liner over the exit code.
    std::process::exit(quern::repl::main());
}
