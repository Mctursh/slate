use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use slate_replay::Halt;

const DOCS: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/blob/master/README.md");

pub enum Problem<'a> {
    Replay { slot: u64, halt: &'a Halt },
    EndState { slot: u64, mismatches: Vec<String> },
}

pub struct Run<'a> {
    pub args: &'a [String],
    pub disk_store: bool,
    pub verified: usize,
    pub last_verified: Option<u64>,
    pub covered_through: u64,
}

impl Problem<'_> {
    fn slot(&self) -> u64 {
        match self {
            Problem::Replay { slot, .. } | Problem::EndState { slot, .. } => *slot,
        }
    }
}

pub fn report_path(store_path: &str, disk_store: bool, slot: u64) -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let base = if disk_store {
        cwd.join(store_path)
            .parent()
            .map_or_else(|| cwd.clone(), Path::to_path_buf)
    } else {
        cwd
    };
    base.join(format!("halt-{slot}")).join("report.txt")
}

pub fn resume_command(args: &[String], disk_store: bool) -> String {
    let mut out: Vec<&str> = args.iter().map(String::as_str).collect();
    if disk_store {
        out.retain(|a| *a != "--overwrite");
        if !out.contains(&"--resume") {
            out.push("--resume");
        }
    }
    out.into_iter().map(quote).collect::<Vec<_>>().join(" ")
}

fn quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=,@%+".contains(c));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

pub fn render(problem: &Problem, run: &Run, report: &Path) -> String {
    let slot = problem.slot();
    let mut s = String::new();
    let (retryable, anchor) = match problem {
        Problem::Replay { halt, .. } => match halt {
            Halt::StateDivergence { computed, vote } => {
                let _ = writeln!(s, "halted at slot {slot}: state divergence");
                let _ = writeln!(
                    s,
                    "  status: replayed to bank hash {computed}, consensus voted {vote}"
                );
                let _ = writeln!(s, "          state diverged at or before this slot");
                (true, "state-divergence")
            }
            Halt::ExecutionDivergence {
                tx_index,
                signature,
                issues,
            } => {
                let _ = writeln!(s, "halted at slot {slot}: execution divergence");
                let _ = writeln!(
                    s,
                    "  status: transaction {tx_index} ({signature}) replayed differently from the chain"
                );
                for issue in issues {
                    let _ = writeln!(s, "          {issue}");
                }
                (true, "execution-divergence")
            }
            Halt::Unsanitizable {
                tx_index,
                signature,
                error,
            } => {
                let _ = writeln!(s, "halted at slot {slot}: unsanitizable transaction");
                let _ = writeln!(
                    s,
                    "  status: transaction {tx_index} ({signature}) could not be prepared: {error}"
                );
                (true, "unsanitizable-transaction")
            }
        },
        Problem::EndState { mismatches, .. } => {
            let _ = writeln!(
                s,
                "end state differs from the snapshot at slot {slot}: {} account(s)",
                mismatches.len()
            );
            let _ = writeln!(
                s,
                "  status: the replay finished, but its accounts differ from the real snapshot"
            );
            for m in mismatches.iter().take(5) {
                let _ = writeln!(s, "          {m}");
            }
            (false, "end-state-mismatch")
        }
    };

    let last = run
        .last_verified
        .map_or_else(|| "none yet".to_string(), |l| l.to_string());
    let _ = writeln!(
        s,
        "verified: {} slots consensus-verified this run, last {last}",
        run.verified
    );
    let _ = writeln!(s, "coverage: recorded up to slot {}", run.covered_through);

    let report = report.display();
    if retryable {
        let command = resume_command(run.args, run.disk_store);
        if run.disk_store {
            let _ = writeln!(s, "  action: resume once, a transient fault won't repeat:");
        } else {
            let _ = writeln!(
                s,
                "  action: re-run once (a memory store can't resume), a transient fault won't repeat:"
            );
        }
        let _ = writeln!(s, "            {command}");
        let _ = writeln!(
            s,
            "          if it halts at slot {slot} again it's real: open an issue and attach"
        );
        let _ = writeln!(s, "            {report}");
    } else {
        let _ = writeln!(
            s,
            "  action: open an issue and attach the report, which lists every mismatched account:"
        );
        let _ = writeln!(s, "            {report}");
    }
    let _ = writeln!(s, "     see: {DOCS}#{anchor}");
    s
}

pub fn report(problem: &Problem, run: &Run, report: &Path) -> String {
    let cwd =
        std::env::current_dir().map_or_else(|_| "unknown".into(), |d| d.display().to_string());
    let mut s = format!(
        "slate-backfill {} ({})\nagave {}\ncwd: {cwd}\ncommand: {}\n\n",
        env!("CARGO_PKG_VERSION"),
        env!("SLATE_GIT_COMMIT"),
        env!("SLATE_AGAVE_VERSION"),
        run.args
            .iter()
            .map(|a| quote(a))
            .collect::<Vec<_>>()
            .join(" "),
    );
    s.push_str(&render(problem, run, report));
    if let Problem::EndState { mismatches, .. } = problem {
        s.push_str("\nmismatched accounts:\n");
        for m in mismatches {
            let _ = writeln!(s, "  {m}");
        }
    }
    s
}

pub fn write_report(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_hash::Hash;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn run(args: &[String], disk_store: bool) -> Run<'_> {
        Run {
            args,
            disk_store,
            verified: 1_000,
            last_verified: Some(99),
            covered_through: 99,
        }
    }

    #[test]
    fn the_resume_command_adds_resume_and_drops_overwrite() {
        let a = args(&[
            "slate-backfill",
            "snap dir/s.tar.zst",
            "--overwrite",
            "--to",
            "9",
        ]);
        assert_eq!(
            resume_command(&a, true),
            "slate-backfill 'snap dir/s.tar.zst' --to 9 --resume"
        );
        let resumed = args(&["slate-backfill", "--resume", "--to", "9"]);
        assert_eq!(
            resume_command(&resumed, true),
            "slate-backfill --resume --to 9"
        );
    }

    #[test]
    fn a_memory_store_reruns_the_same_command() {
        let a = args(&["slate-backfill", "--to", "9"]);
        assert_eq!(resume_command(&a, false), "slate-backfill --to 9");
    }

    #[test]
    fn the_report_sits_beside_a_disk_store() {
        assert_eq!(
            report_path("/data/accounts.redb", true, 7),
            Path::new("/data/halt-7/report.txt")
        );
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            report_path("accounts.redb", true, 7),
            cwd.join("halt-7/report.txt")
        );
        assert_eq!(
            report_path("/data/accounts.redb", false, 7),
            cwd.join("halt-7/report.txt")
        );
    }

    #[test]
    fn a_state_divergence_names_both_hashes_and_the_next_step() {
        let (computed, vote) = (Hash::new_unique(), Hash::new_unique());
        let halt = Halt::StateDivergence { computed, vote };
        let a = args(&["slate-backfill", "--to", "100"]);
        let text = render(
            &Problem::Replay {
                slot: 100,
                halt: &halt,
            },
            &run(&a, true),
            Path::new("/data/halt-100/report.txt"),
        );
        assert!(
            text.starts_with("halted at slot 100: state divergence\n"),
            "{text}"
        );
        assert!(text.contains(&computed.to_string()) && text.contains(&vote.to_string()));
        assert!(text.contains("slate-backfill --to 100 --resume"));
        assert!(text.contains("/data/halt-100/report.txt"));
        assert!(text.contains("README.md#state-divergence"));
    }

    #[test]
    fn an_execution_divergence_names_the_transaction() {
        let halt = Halt::ExecutionDivergence {
            tx_index: 3,
            signature: "5sig".into(),
            issues: vec!["fee: chain 5000, replay 10000".into()],
        };
        let a = args(&["slate-backfill"]);
        let text = render(
            &Problem::Replay {
                slot: 7,
                halt: &halt,
            },
            &run(&a, true),
            Path::new("r.txt"),
        );
        assert!(text.contains("transaction 3 (5sig)"), "{text}");
        assert!(text.contains("fee: chain 5000, replay 10000"));
        assert!(text.contains("#execution-divergence"));
    }
}
