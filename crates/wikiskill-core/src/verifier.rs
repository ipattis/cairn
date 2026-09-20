//! The deterministic verifier: runs under the rollout Seatbelt profile after each
//! session and returns a score from 0 to 1. No LLM or Jev judgment enters the score.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::eval::{Expect, Task};
use crate::sandbox::{Isolation, ProfileRequest};
use crate::Result;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifierConfig {
    /// Hard cap on a single check. A hung check scores 0 rather than stalling the run.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// The verifier should not need the network; denying it keeps scores reproducible.
    #[serde(default)]
    pub allow_network: bool,
}

fn default_timeout() -> u64 {
    300
}

impl Default for VerifierConfig {
    fn default() -> Self {
        Self {
            timeout_secs: default_timeout(),
            allow_network: false,
        }
    }
}

/// One task's score plus the evidence behind it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Score {
    pub task_id: String,
    /// 0.0 to 1.0 inclusive.
    pub score: f64,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Trimmed tail of the check's output, for the raw note.
    pub output_tail: String,
}

impl Score {
    pub fn zero(task_id: &str, reason: &str) -> Self {
        Self {
            task_id: task_id.to_string(),
            score: 0.0,
            exit_code: None,
            timed_out: false,
            output_tail: reason.to_string(),
        }
    }
}

/// Mean score over a split, in 0..=1.
pub fn mean(scores: &[Score]) -> f64 {
    if scores.is_empty() {
        return 0.0;
    }
    scores.iter().map(|s| s.score).sum::<f64>() / scores.len() as f64
}

pub struct Verifier {
    config: VerifierConfig,
    isolation: Box<dyn Isolation>,
    /// Denied to the check for the same reason as to the agent: keep the wiki out.
    deny_read: Vec<PathBuf>,
}

impl Verifier {
    pub fn new(
        config: VerifierConfig,
        isolation: Box<dyn Isolation>,
        deny_read: Vec<PathBuf>,
    ) -> Self {
        Self {
            config,
            isolation,
            deny_read,
        }
    }

    /// Runs `task`'s check inside `work_dir` (the agent's copy of the workspace).
    pub async fn score(&self, task: &Task, work_dir: &Path) -> Result<Score> {
        let request = ProfileRequest {
            work_dir: work_dir
                .canonicalize()
                .unwrap_or_else(|_| work_dir.to_path_buf()),
            // A verifier is a check program in the work tree; it has no agent home.
            agent_home: None,
            deny_read: self.deny_read.clone(),
            allow_network: self.config.allow_network,
        };
        let mut cmd = self
            .isolation
            .command(&request, &task.check_program, &task.check_args)
            .await?;
        cmd.current_dir(work_dir);
        cmd.env("WIKISKILL_VERIFIER", "1");
        cmd.kill_on_drop(true);

        let output = match tokio::time::timeout(
            Duration::from_secs(self.config.timeout_secs),
            cmd.output(),
        )
        .await
        {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                return Ok(Score {
                    task_id: task.id.clone(),
                    score: 0.0,
                    exit_code: None,
                    timed_out: false,
                    output_tail: format!("check failed to run: {e}"),
                })
            }
            Err(_) => {
                return Ok(Score {
                    task_id: task.id.clone(),
                    score: 0.0,
                    exit_code: None,
                    timed_out: true,
                    output_tail: format!(
                        "check timed out after {}s",
                        self.config.timeout_secs
                    ),
                })
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let combined = format!("{stdout}{stderr}");
        let score = interpret(&task.expect, output.status.success(), &stdout);

        Ok(Score {
            task_id: task.id.clone(),
            score,
            exit_code: output.status.code(),
            timed_out: false,
            output_tail: tail(&combined, 2000),
        })
    }
}

/// Turns a check's result into a score. Split out so the parsing rules are testable
/// without spawning anything.
pub fn interpret(expect: &Expect, success: bool, stdout: &str) -> f64 {
    match expect {
        Expect::ExitZero => {
            if success {
                1.0
            } else {
                0.0
            }
        }
        Expect::StdoutContains { text } => {
            if stdout.contains(text.as_str()) {
                1.0
            } else {
                0.0
            }
        }
        Expect::Fraction => last_non_empty_line(stdout)
            .and_then(parse_fraction)
            .unwrap_or(0.0),
        Expect::Score => last_non_empty_line(stdout)
            .and_then(|line| line.trim().parse::<f64>().ok())
            .map(|v| v.clamp(0.0, 1.0))
            .unwrap_or(0.0),
    }
}

fn last_non_empty_line(text: &str) -> Option<&str> {
    text.lines().rev().find(|l| !l.trim().is_empty())
}

fn parse_fraction(line: &str) -> Option<f64> {
    let (passed, total) = line.trim().split_once('/')?;
    let passed: f64 = passed.trim().parse().ok()?;
    let total: f64 = total.trim().parse().ok()?;
    if total <= 0.0 {
        return Some(0.0);
    }
    Some((passed / total).clamp(0.0, 1.0))
}

fn tail(text: &str, max: usize) -> String {
    let trimmed = text.trim_end();
    if trimmed.len() <= max {
        return trimmed.to_string();
    }
    let start = trimmed.len() - max;
    // Do not slice through a multi-byte character.
    let start = (start..trimmed.len())
        .find(|i| trimmed.is_char_boundary(*i))
        .unwrap_or(trimmed.len());
    format!("…{}", &trimmed[start..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::NoIsolation;

    fn task(program: &str, args: &[&str], expect: Expect) -> Task {
        Task {
            id: "t1".into(),
            prompt: "p".into(),
            workspace: PathBuf::from("/tmp"),
            check_program: program.into(),
            check_args: args.iter().map(|s| s.to_string()).collect(),
            expect,
            tags: vec![],
        }
    }

    #[test]
    fn fraction_parsing_is_the_documented_shape() {
        assert_eq!(interpret(&Expect::Fraction, true, "3/4\n"), 0.75);
        assert_eq!(interpret(&Expect::Fraction, false, "noise\n0/7\n"), 0.0);
        assert_eq!(interpret(&Expect::Fraction, true, "7/7"), 1.0);
        // Garbage scores zero rather than panicking or guessing.
        assert_eq!(interpret(&Expect::Fraction, true, "what"), 0.0);
        assert_eq!(interpret(&Expect::Fraction, true, "1/0"), 0.0);
    }

    #[test]
    fn score_expectation_is_clamped() {
        assert_eq!(interpret(&Expect::Score, true, "0.42"), 0.42);
        assert_eq!(interpret(&Expect::Score, true, "9.0"), 1.0);
        assert_eq!(interpret(&Expect::Score, true, "-1"), 0.0);
    }

    #[test]
    fn exit_and_contains_expectations() {
        assert_eq!(interpret(&Expect::ExitZero, true, ""), 1.0);
        assert_eq!(interpret(&Expect::ExitZero, false, ""), 0.0);
        let expect = Expect::StdoutContains {
            text: "ok".into(),
        };
        assert_eq!(interpret(&expect, false, "all ok here"), 1.0);
        assert_eq!(interpret(&expect, true, "nope"), 0.0);
    }

    #[tokio::test]
    async fn scores_a_real_check() {
        let dir = tempfile::tempdir().unwrap();
        let verifier = Verifier::new(
            VerifierConfig::default(),
            Box::new(NoIsolation),
            vec![],
        );
        let score = verifier
            .score(&task("/bin/echo", &["6/8"], Expect::Fraction), dir.path())
            .await
            .unwrap();
        assert_eq!(score.score, 0.75);
        assert_eq!(score.exit_code, Some(0));
        assert!(!score.timed_out);
    }

    #[tokio::test]
    async fn a_missing_check_program_scores_zero() {
        let dir = tempfile::tempdir().unwrap();
        let verifier =
            Verifier::new(VerifierConfig::default(), Box::new(NoIsolation), vec![]);
        let score = verifier
            .score(
                &task("/nonexistent/check", &[], Expect::ExitZero),
                dir.path(),
            )
            .await
            .unwrap();
        assert_eq!(score.score, 0.0);
        assert!(score.output_tail.contains("check failed to run"));
    }

    #[tokio::test]
    async fn a_hung_check_times_out_and_scores_zero() {
        let dir = tempfile::tempdir().unwrap();
        let verifier = Verifier::new(
            VerifierConfig {
                timeout_secs: 1,
                allow_network: false,
            },
            Box::new(NoIsolation),
            vec![],
        );
        let score = verifier
            .score(&task("/bin/sleep", &["30"], Expect::ExitZero), dir.path())
            .await
            .unwrap();
        assert!(score.timed_out);
        assert_eq!(score.score, 0.0);
    }

    #[test]
    fn mean_of_an_empty_split_is_zero() {
        assert_eq!(mean(&[]), 0.0);
        assert_eq!(
            mean(&[Score::zero("a", "x"), {
                let mut s = Score::zero("b", "x");
                s.score = 1.0;
                s
            }]),
            0.5
        );
    }
}
