//! bead: treadle-cli — arg parsing, engine selection, exit codes.
//!
//! ```text
//! treadle [--engine vm|tree] <file.tr>
//! treadle --both <file.tr>
//! ```
//!
//! # It renders a finished `Output`; it does not stream
//!
//! §6/`.33` pins this and it is the one thing in here worth being careful about.
//! A CLI that wrote each `print` as it happened would show output that neither
//! engine's `Output` contains: `print "a", 1/0;` appends **no** line, so a
//! streaming CLI would print `a` before the error while both engines say the
//! program printed nothing. That is a third answer, and the whole point of this
//! crate is that there are only ever two. So: run to completion, then write
//! `output.to_string()` — the §5 canonical rendering — and nothing else.
//!
//! # Exit codes
//!
//! | code | meaning                                                     |
//! |------|-------------------------------------------------------------|
//! | 0    | the program ran to completion with no error                 |
//! | 1    | the program produced a `TreadleError` (still prints lines)   |
//! | 2    | the CLI failed: bad flag, missing file, unreadable path      |
//! | 3    | `--both` only: the two engines disagreed                     |
//!
//! 3 is not in the bead's table because the bead's table describes running *a*
//! program; a divergence is not a property of the program at all. It needs its
//! own code so `--both` is usable from a script — folding it into 1 would make
//! "this program errors" and "the engines disagree" indistinguishable, which is
//! exactly the confusion `--both` exists to remove.

use std::io::Write;
use std::path::PathBuf;

use crate::engine::Engine;
use crate::output::{self, Output};

const USAGE: &str = "usage: treadle [--engine vm|tree | --both] <file.tr>";

const EXIT_PROGRAM_ERROR: i32 = 1;
const EXIT_CLI_ERROR: i32 = 2;
const EXIT_DIVERGED: i32 = 3;

/// Which engine(s) a run uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Which {
    Vm,
    Tree,
    Both,
}

/// The default engine is the **tree-walker**.
///
/// Not because it is better — §4 requires the two to be indistinguishable, so
/// "better" has no observable meaning here. Because when a user reports that
/// treadle did the wrong thing, the tree-walker has the fewest stages between
/// their source and the behaviour they saw (parse, walk) where the VM has three
/// (parse, compile, execute), so the same complaint is cheaper to trace. A user
/// who cares which engine ran says so with `--engine`; a user who suspects the
/// answer itself says `--both`.
const DEFAULT_ENGINE: Which = Which::Tree;

#[derive(Debug, PartialEq, Eq)]
enum Args {
    Help,
    Run { which: Which, path: PathBuf },
}

/// Entry point. Returns the process exit code; never panics, never `unwrap`s.
pub fn main() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (which, path) = match parse(&argv) {
        Ok(Args::Help) => {
            write_all(&mut std::io::stdout(), &format!("{USAGE}\n"));
            return 0;
        }
        Ok(Args::Run { which, path }) => (which, path),
        Err(e) => {
            eprintln!("treadle: {e}\ntreadle: {USAGE}");
            return EXIT_CLI_ERROR;
        }
    };

    let src = match std::fs::read_to_string(&path) {
        Ok(src) => src,
        Err(e) => {
            eprintln!("treadle: cannot read {}: {e}", path.display());
            return EXIT_CLI_ERROR;
        }
    };

    let (out, err, code) = execute(which, &src);
    // `write_all` rather than `print!`: `println!`/`print!` panic on a broken
    // pipe, and `treadle prog.tr | head -1` is a broken pipe. §3 says `run`
    // never panics; it would be silly for the printing to.
    write_all(&mut std::io::stdout(), &out);
    write_all(&mut std::io::stderr(), &err);
    code
}

/// Parse argv (without the program name) into a command.
///
/// Split out from [`main`] because it is the only part with branches worth
/// testing without a subprocess.
fn parse(argv: &[String]) -> Result<Args, String> {
    let mut which: Option<Which> = None;
    let mut path: Option<PathBuf> = None;
    let mut it = argv.iter();

    while let Some(arg) = it.next() {
        let a = arg.as_str();
        if a == "-h" || a == "--help" {
            return Ok(Args::Help);
        } else if a == "--both" {
            select(&mut which, Which::Both)?;
        } else if a == "--engine" || a.starts_with("--engine=") {
            let value = match a.strip_prefix("--engine=") {
                Some(v) => v.to_string(),
                None => it
                    .next()
                    .ok_or("--engine needs a value: vm or tree")?
                    .clone(),
            };
            select(
                &mut which,
                match value.as_str() {
                    "vm" => Which::Vm,
                    "tree" => Which::Tree,
                    other => return Err(format!("unknown engine '{other}': expected vm or tree")),
                },
            )?;
        } else if a.starts_with('-') && a != "-" {
            return Err(format!("unknown flag '{a}'"));
        } else if path.replace(PathBuf::from(a)).is_some() {
            return Err("more than one input file".to_string());
        }
    }

    Ok(Args::Run {
        which: which.unwrap_or(DEFAULT_ENGINE),
        path: path.ok_or("no input file")?,
    })
}

/// Engine selection is one choice, so a second one is a mistake rather than an
/// override — `--engine vm --both` has no answer that is not a guess.
fn select(slot: &mut Option<Which>, w: Which) -> Result<(), String> {
    if slot.is_some() {
        return Err("choose one of --engine or --both, once".to_string());
    }
    *slot = Some(w);
    Ok(())
}

/// Run `src` and return `(stdout, stderr, exit code)`.
///
/// stdout is *only* ever the §5 canonical rendering, byte for byte, so
/// `treadle a.tr` and `treadle --both a.tr` produce identical stdout and the
/// `--both` reporting cannot contaminate a comparison someone pipes it into.
fn execute(which: Which, src: &str) -> (String, String, i32) {
    if which == Which::Vm {
        return rendered(crate::vm::Vm::new().run(src));
    }
    if which == Which::Tree {
        return rendered(crate::tree::eval::Eval::new().run(src));
    }

    // The breadcrumbs are the point, not chatter. Both engines' `Engine::run`
    // already runs on a 64 MiB thread, but an engine that overflows even that
    // ABORTS the process — and an abort is not a panic, so `catch_unwind` would
    // not see it and no `Output` is ever produced. That asymmetry is invisible
    // to the differential fuzzer for exactly this reason (`assert_eq!` needs two
    // values), and it is real: `print 1 + 1 + ...` with 20000 terms returned
    // 20001 on tree and aborted on vm. Naming each engine before it runs is the
    // whole in-process mechanism available: if treadle dies here, the last line
    // on stderr is the engine that killed it. LIMITATION: it tells you *which*
    // died, not what the other would have said — rerun with `--engine` for that.
    eprintln!("treadle: running vm...");
    let a = crate::vm::Vm::new().run(src);
    eprintln!("treadle: running tree...");
    let b = crate::tree::eval::Eval::new().run(src);

    let stdout = a.to_string();
    if a == b {
        let msg = format!("treadle: vm and tree agree ({} bytes)\n", stdout.len());
        (stdout, msg, exit_code(&a))
    } else {
        // `output::diff` and not a second comparator: a divergence found by hand
        // must be reported in the same words as one found by the fuzzer, or the
        // two reports cannot be compared with each other.
        (stdout, output::diff("vm", &a, "tree", &b), EXIT_DIVERGED)
    }
}

fn rendered(out: Output) -> (String, String, i32) {
    let code = exit_code(&out);
    (out.to_string(), String::new(), code)
}

fn exit_code(out: &Output) -> i32 {
    if out.failed() {
        EXIT_PROGRAM_ERROR
    } else {
        0
    }
}

/// Write everything, and give up quietly if the far end is gone.
fn write_all(w: &mut impl Write, s: &str) {
    let _ = w.write_all(s.as_bytes());
    let _ = w.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Args, String> {
        parse(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn run_of(r: Result<Args, String>) -> (Which, PathBuf) {
        match r {
            Ok(Args::Run { which, path }) => (which, path),
            other => panic!("expected a run, got {other:?}"),
        }
    }

    // --- arg parsing ------------------------------------------------------

    #[test]
    fn cli_defaults_to_the_tree_engine() {
        let (which, path) = run_of(p(&["a.tr"]));
        assert_eq!(which, Which::Tree);
        assert_eq!(path, PathBuf::from("a.tr"));
    }

    #[test]
    fn cli_selects_either_engine_by_name() {
        assert_eq!(run_of(p(&["--engine", "vm", "a.tr"])).0, Which::Vm);
        assert_eq!(run_of(p(&["--engine", "tree", "a.tr"])).0, Which::Tree);
        // `--engine=vm` too: the flag is worth typing either way.
        assert_eq!(run_of(p(&["--engine=vm", "a.tr"])).0, Which::Vm);
        // Flags may follow the file.
        assert_eq!(run_of(p(&["a.tr", "--engine", "vm"])).0, Which::Vm);
    }

    #[test]
    fn cli_selects_both_engines() {
        assert_eq!(run_of(p(&["--both", "a.tr"])).0, Which::Both);
    }

    #[test]
    fn cli_rejects_an_unknown_engine_value() {
        let e = p(&["--engine", "interp", "a.tr"]).unwrap_err();
        assert!(e.contains("interp") && e.contains("vm or tree"), "{e}");
        // An empty value is not silently the default either.
        assert!(p(&["--engine=", "a.tr"]).is_err());
    }

    #[test]
    fn cli_rejects_engine_with_no_value_at_all() {
        assert!(p(&["--engine"]).unwrap_err().contains("--engine needs"));
        // ...and does not swallow the filename as the value, which would then
        // fail as "no input file" and blame the wrong argument.
        let e = p(&["--engine", "a.tr"]).unwrap_err();
        assert!(e.contains("a.tr") && e.contains("vm or tree"), "{e}");
    }

    #[test]
    fn cli_rejects_an_unknown_flag() {
        assert!(p(&["--verbose", "a.tr"]).unwrap_err().contains("--verbose"));
        assert!(p(&["-x", "a.tr"]).unwrap_err().contains("-x"));
    }

    #[test]
    fn cli_rejects_a_missing_or_duplicated_file() {
        assert!(p(&[]).unwrap_err().contains("no input file"));
        assert!(p(&["--both"]).unwrap_err().contains("no input file"));
        assert!(p(&["a.tr", "b.tr"]).unwrap_err().contains("more than one"));
    }

    #[test]
    fn cli_rejects_two_engine_selections() {
        assert!(p(&["--engine", "vm", "--both", "a.tr"]).is_err());
        assert!(p(&["--both", "--engine", "vm", "a.tr"]).is_err());
    }

    #[test]
    fn cli_help_is_a_command_not_an_error() {
        assert_eq!(p(&["--help"]), Ok(Args::Help));
        assert_eq!(p(&["-h"]), Ok(Args::Help));
    }

    // --- rendering and exit codes ----------------------------------------

    #[test]
    fn cli_prints_the_lines_before_an_error_and_exits_one() {
        for which in [Which::Vm, Which::Tree, Which::Both] {
            let (out, _, code) = execute(which, "print 1;\nprint 2/0;\nprint 3;\n");
            assert_eq!(code, 1, "{which:?}");
            // Terminated, not separated; the error line is the §3 Display form.
            assert!(out.starts_with("1\n"), "{which:?}: {out:?}");
            assert!(out.ends_with('\n'), "{which:?}: {out:?}");
            assert!(out.contains("error: "), "{which:?}: {out:?}");
            // §6/.33: no partial line for the `print` that failed.
            assert_eq!(out.lines().count(), 2, "{which:?}: {out:?}");
        }
    }

    #[test]
    fn cli_exits_zero_and_renders_nothing_for_a_program_that_prints_nothing() {
        let (out, _, code) = execute(Which::Tree, "let x = 1;\n");
        assert_eq!((out.as_str(), code), ("", 0));
    }

    #[test]
    fn cli_both_agrees_and_keeps_stdout_byte_identical_to_one_engine() {
        let src = "print 1; print \"\"; print \"a\\nb\";";
        let (solo, _, solo_code) = execute(Which::Tree, src);
        let (out, err, code) = execute(Which::Both, src);
        assert_eq!(out, solo, "--both must not contaminate stdout");
        assert_eq!(code, solo_code);
        assert!(err.starts_with("treadle: vm and tree agree ("), "{err:?}");
    }

    #[test]
    fn cli_both_reports_a_divergence_through_output_diff() {
        // No real divergence exists (that is the point), so this pins the
        // reporting shape against the one comparator both paths must share.
        let mut a = Output::new();
        a.push_line("1");
        let b = Output::new();
        let report = output::diff("vm", &a, "tree", &b);
        assert!(report.starts_with("divergence: vm vs tree"), "{report}");
        assert!(report.contains("first differing byte"), "{report}");
    }

    // --- the real binary --------------------------------------------------

    /// `CARGO_BIN_EXE_treadle` is only set for integration targets, and this
    /// bead owns one file. The test binary lives in `<target>/debug/deps/`, so
    /// the bin is two levels up — and `cargo test` builds it.
    fn treadle_bin() -> PathBuf {
        let mut p = std::env::current_exe().expect("current_exe");
        p.pop();
        p.pop();
        p.push(format!("treadle{}", std::env::consts::EXE_SUFFIX));
        p
    }

    #[test]
    fn cli_real_binary_honours_stdout_stderr_and_all_three_exit_codes() {
        let exe = treadle_bin();
        assert!(exe.is_file(), "no binary at {}", exe.display());

        let dir = std::env::temp_dir().join(format!("treadle-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let ok = dir.join("ok.tr");
        let bad = dir.join("bad.tr");
        std::fs::write(&ok, "print 1;\nprint \"two\";\n").expect("write");
        std::fs::write(&bad, "print 1;\nprint 2/0;\n").expect("write");

        let run = |args: &[&std::ffi::OsStr]| {
            std::process::Command::new(&exe)
                .args(args)
                .output()
                .expect("spawn treadle")
        };
        let os = std::ffi::OsStr::new;

        // 0: ran clean. stdout is the canonical rendering and stderr is silent.
        let r = run(&[ok.as_os_str()]);
        assert_eq!(r.status.code(), Some(0), "{r:?}");
        assert_eq!(String::from_utf8_lossy(&r.stdout), "1\ntwo\n");
        assert_eq!(String::from_utf8_lossy(&r.stderr), "");

        // Same bytes on the other engine — §4, checked through the real binary.
        let vm = run(&[os("--engine"), os("vm"), ok.as_os_str()]);
        assert_eq!(vm.stdout, r.stdout);
        assert_eq!(vm.status.code(), Some(0));

        // 1: the program errored, and its earlier line survived.
        let r = run(&[bad.as_os_str()]);
        assert_eq!(r.status.code(), Some(1), "{r:?}");
        let out = String::from_utf8_lossy(&r.stdout);
        assert!(out.starts_with("1\n") && out.contains("error: "), "{out:?}");

        // 0 with the agreement note on stderr, not stdout.
        let r = run(&[os("--both"), ok.as_os_str()]);
        assert_eq!(r.status.code(), Some(0), "{r:?}");
        assert_eq!(String::from_utf8_lossy(&r.stdout), "1\ntwo\n");
        let err = String::from_utf8_lossy(&r.stderr);
        assert!(err.contains("vm and tree agree"), "{err:?}");

        // 2: missing file, unknown flag, no file. Diagnostic on stderr, and
        // stdout stays empty so a pipeline sees nothing rather than garbage.
        for args in [
            vec![os("no-such-file.tr")],
            vec![os("--nope"), ok.as_os_str()],
            vec![],
        ] {
            let r = run(&args);
            assert_eq!(r.status.code(), Some(2), "{args:?} -> {r:?}");
            assert!(r.stdout.is_empty(), "{args:?}");
            assert!(
                String::from_utf8_lossy(&r.stderr).starts_with("treadle: "),
                "{args:?} -> {:?}",
                String::from_utf8_lossy(&r.stderr)
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
