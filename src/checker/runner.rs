use crate::checker::config::{Expect, ExpectKind, Matcher, Step, Vars};
use crate::checker::executor::{CommandExecutor, CommandOutput};
use crate::checker::template::render;
use anyhow::Context;
use regex::Regex;
use std::collections::HashMap;

pub struct StepRunner<R: CommandExecutor> {
    runner: R,

    verbose: bool,
}

impl<R: CommandExecutor> StepRunner<R> {
    pub fn new(runner: R, verbose: bool) -> Self {
        Self { runner, verbose }
    }

    pub fn run(&self, step: &Step, ctx: &mut Vars) -> anyhow::Result<Option<CommandOutput>> {
        let attempts = step.retry.as_ref().map_or(1, |r| r.times);
        let mut last_err = None;

        for attempt in 1..=attempts {
            match self.run_once(step, ctx) {
                Ok((out, scope)) => {
                    *ctx = scope;
                    return Ok(Some(out));
                }
                Err(e) => {
                    if attempt < attempts {
                        if self.verbose {
                            eprintln!("attempt {attempt}/{attempts} failed: {e:#}");
                        }

                        if let Some(r) = &step.retry {
                            std::thread::sleep(r.interval);
                        }
                    }

                    last_err = Some(e);
                }
            }
        }

        let err = last_err.expect("at least one attempt failed");
        if step.allow_failure {
            if self.verbose {
                eprintln!("step failed (allowed): {err:#}");
            }
            return Ok(None);
        }

        Err(err)
    }

    fn run_once(&self, step: &Step, ctx: &Vars) -> anyhow::Result<(CommandOutput, Vars)> {
        let cmd = render(&step.cmd, ctx)?;
        let out = self
            .runner
            .run(&cmd, step.timeout)
            .with_context(|| format!("failed to run `{cmd:?}`"))?;

        if out.is_timeout {
            anyhow::bail!("step timed out after {:?}", out.elapsed)
        }

        let mut scope = ctx.clone();
        extract(&out, &step.extract, &mut scope)?;
        self.check(&out, &step.expect, &mut scope)?;

        Ok((out, scope))
    }

    fn check(&self, out: &CommandOutput, expect: &[Expect], ctx: &mut Vars) -> anyhow::Result<()> {
        let mut failed = 0_usize;

        for (i, c) in expect.iter().enumerate() {
            if let Err(e) = self.check_once(out, &c.kind, ctx) {
                let e = e.context(format!("failed to check expect #{i}"));
                if !c.continue_on_failure {
                    return Err(e);
                }

                if self.verbose {
                    eprintln!("{e:#} (continuing)");
                }
                failed += 1;
            }
        }

        anyhow::ensure!(failed == 0, "failed to check {failed} expect(s)");

        Ok(())
    }

    fn check_once(
        &self,
        out: &CommandOutput,
        kind: &ExpectKind,
        ctx: &mut Vars,
    ) -> anyhow::Result<()> {
        Ok(match kind {
            ExpectKind::Exec(step) => self.run(step, ctx).map(drop)?,
            ExpectKind::ExitCode(code) => check_exit_code(out, *code)?,
            ExpectKind::Stdout(ms) => check_stream(out, Stream::Stdout, ms, ctx)?,
            ExpectKind::Stderr(ms) => check_stream(out, Stream::Stderr, ms, ctx)?,
        })
    }
}

/// Pure: run each extract pattern (e.g. regex with 1 capture group)
pub fn extract(
    out: &CommandOutput,
    rules: &HashMap<String, String>,
    ctx: &mut Vars,
) -> anyhow::Result<()> {
    for (name, pat) in rules {
        let re = Regex::new(pat).with_context(|| format!("extract `{name}`: bad regex /{pat}/"))?;

        let caps = re.captures(&out.stdout).with_context(|| {
            format!(
                "extract `{name}`: stdout does not match /{pat}/, got {:?}",
                out.stdout
            )
        })?;
        let group = if re.captures_len() > 1 { 1 } else { 0 };
        let m = caps.get(group).with_context(|| {
            format!("extract `{name}`: group 1 of /{pat}/ did not participate in the match")
        })?;

        ctx.insert(name.clone(), m.as_str().to_owned());
    }

    Ok(())
}

#[derive(Debug, Copy, Clone)]
enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    fn name(&self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
        }
    }

    fn get(self, out: &CommandOutput) -> &str {
        match self {
            Stream::Stdout => &out.stdout,
            Stream::Stderr => &out.stderr,
        }
    }
}

fn check_stream(
    out: &CommandOutput,
    stream: Stream,
    ms: &[Matcher],
    ctx: &mut Vars,
) -> anyhow::Result<()> {
    for (i, m) in ms.iter().enumerate() {
        check_text(out, stream, m, ctx)
            .with_context(|| format!("{} matcher #{}", stream.name(), i + 1))?;
    }

    Ok(())
}

fn check_text(
    out: &CommandOutput,
    stream: Stream,
    m: &Matcher,
    ctx: &mut Vars,
) -> anyhow::Result<()> {
    let name = stream.name();
    let text = stream.get(out);

    let (ok, expected) = match m {
        Matcher::Contains(s) => {
            let s = render(s, ctx)?;
            (text.contains(&*s), format!("contain {s:?}"))
        }
        Matcher::NotContains(s) => {
            let s = render(s, ctx)?;
            (!text.contains(&*s), format!("not contain {s:?}"))
        }
        Matcher::Eq(s) => {
            let s = render(s, ctx)?;
            (text == s, format!("equal {s:?}"))
        }
        Matcher::NotEq(s) => {
            let s = render(s, ctx)?;
            (text != s, format!("not equal {s:?}"))
        }
        Matcher::Match(e) => (e.is_match(&text), format!("match /{}/", e.as_str())),
    };

    anyhow::ensure!(ok, "{name} should {expected}, but was {text:?}");
    Ok(())
}

fn check_exit_code(out: &CommandOutput, want: i32) -> anyhow::Result<()> {
    anyhow::ensure!(
        out.exit_code == Some(want),
        "expected exit code {want}, got {:?}{}",
        out.exit_code,
        stderr_suffix(out)
    );
    Ok(())
}

fn stderr_suffix(out: &CommandOutput) -> String {
    let err = out.stderr.trim();
    if err.is_empty() {
        String::new()
    } else {
        format!("\nstderr: {err}")
    }
}

#[cfg(test)]
mod tests {
    use crate::checker::config::{Expect, ExpectKind, Matcher, Retry, Step};
    use crate::checker::executor::{CommandExecutor, CommandOutput};
    use crate::checker::runner::StepRunner;
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Debug, Default)]
    struct FakeExecutor {
        responses: Mutex<VecDeque<anyhow::Result<CommandOutput>>>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl FakeExecutor {
        fn with(responses: Vec<anyhow::Result<CommandOutput>>) -> Self {
            Self {
                responses: Mutex::new(responses.into()),
                calls: Arc::new(Mutex::new(vec![])),
            }
        }

        fn call_handle(&self) -> Arc<Mutex<Vec<String>>> {
            Arc::clone(&self.calls)
        }
    }

    impl CommandExecutor for FakeExecutor {
        fn run(&self, command: &str, _: Option<Duration>) -> anyhow::Result<CommandOutput> {
            self.calls.lock().unwrap().push(command.to_string());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected extra call: {command}"))
        }
    }

    #[test]
    fn render_cmd_and_passed() {
        let executor = FakeExecutor::with(vec![Ok(CommandOutput {
            stdout: "stdout".into(),
            stderr: "".into(),
            exit_code: Some(0),
            is_timeout: false,
            elapsed: Duration::from_millis(100),
        })]);

        let step = create_step(false, None, None);

        let step_runner = StepRunner::new(executor, true);

        let mut ctx = HashMap::new();
        let result = step_runner.run(&step, &mut ctx);
        assert!(result.is_ok());
    }

    #[test]
    fn retry_until_success() {
        let executor = FakeExecutor::with(vec![
            Err(anyhow::anyhow!("make the first try is an error.")),
            Ok(CommandOutput {
                stdout: "stdout".into(),
                stderr: "".into(),
                exit_code: Some(0),
                is_timeout: false,
                elapsed: Duration::from_millis(100),
            }),
        ]);

        let step = create_step(
            false,
            Some(Retry {
                times: 2,
                interval: Duration::from_millis(100),
            }),
            None,
        );

        let step_runner = StepRunner::new(executor, true);

        let mut ctx = HashMap::new();
        let result = step_runner.run(&step, &mut ctx);
        assert!(result.is_ok());
    }

    #[test]
    fn give_up_after_retries() {
        let executor = FakeExecutor::with(vec![
            Err(anyhow::anyhow!("make the first try is an error.")),
            Err(anyhow::anyhow!("make the second try is an error.")),
            Ok(CommandOutput {
                stdout: "stdout".into(),
                stderr: "".into(),
                exit_code: Some(0),
                is_timeout: false,
                elapsed: Duration::from_millis(100),
            }),
        ]);

        let step = create_step(
            false,
            Some(Retry {
                times: 2,
                interval: Duration::from_millis(100),
            }),
            None,
        );

        let step_runner = StepRunner::new(executor, true);

        let mut ctx = HashMap::new();
        let result = step_runner.run(&step, &mut ctx);
        assert!(result.is_err());
    }

    #[test]
    fn timeout_is_a_failure() {
        let executor = FakeExecutor::with(vec![Ok(CommandOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            is_timeout: true,
            elapsed: Duration::from_mins(2),
        })]);

        let step = create_step(false, None, Some(Duration::from_mins(1)));

        let step_runner = StepRunner::new(executor, true);

        let mut ctx = HashMap::new();
        let result = step_runner.run(&step, &mut ctx);
        assert!(result.is_err());
    }

    #[test]
    fn executor_error_is_a_failure() {
        let executor = FakeExecutor::with(vec![Err(anyhow::anyhow!("executor error"))]);

        let step = create_step(false, None, None);

        let step_runner = StepRunner::new(executor, true);

        let mut ctx = HashMap::new();
        let result = step_runner.run(&step, &mut ctx);
        assert!(result.is_err());
    }

    #[test]
    fn allow_failure() {
        let executor = FakeExecutor::with(vec![Err(anyhow::anyhow!("executor error"))]);

        let step = create_step(true, None, None);

        let step_runner = StepRunner::new(executor, true);

        let mut ctx = HashMap::new();
        let result = step_runner.run(&step, &mut ctx);
        assert!(result.as_ref().is_ok_and(|r| r.is_none()));
    }

    #[test]
    fn extraction() {
        let executor = FakeExecutor::with(vec![Ok(CommandOutput {
            stdout: "id: 123".into(),
            stderr: "".into(),
            exit_code: Some(0),
            is_timeout: false,
            elapsed: Duration::from_millis(100),
        })]);

        let mut step = create_step(false, None, None);
        step.extract = HashMap::from([("id".to_string(), "[0-9]+".to_string())]);

        let step_runner = StepRunner::new(executor, true);

        let mut ctx = HashMap::new();
        let result = step_runner.run(&step, &mut ctx);
        assert!(result.is_ok());
        assert!(result.as_ref().is_ok_and(|r| r.is_some()));
        assert_eq!(ctx.get("id").unwrap(), "123");
    }

    #[test]
    fn failed_attempt_leaves_ctx_untouched() {
        let executor = FakeExecutor::with(vec![
            Err(anyhow::anyhow!("make the first try is an error.")),
            Ok(CommandOutput {
                stdout: "id: 123".into(),
                stderr: "".into(),
                exit_code: Some(0),
                is_timeout: false,
                elapsed: Duration::from_millis(100),
            }),
        ]);

        let mut step = create_step(false, None, None);
        step.extract = HashMap::from([("id".to_string(), "[0-9]+".to_string())]);

        let step_runner = StepRunner::new(executor, true);

        let mut ctx = HashMap::new();
        let result = step_runner.run(&step, &mut ctx);
        assert!(result.is_err());
        assert_eq!(ctx.len(), 0);
    }

    #[test]
    fn nested_exec_uses_same_executor_and_sees_parent_vars() {
        let executor = FakeExecutor::with(vec![
            Ok(CommandOutput {
                stdout: "id: 123".into(),
                stderr: "".into(),
                exit_code: Some(0),
                is_timeout: false,
                elapsed: Duration::from_millis(100),
            }),
            Ok(CommandOutput {
                stdout: "all passed".into(),
                stderr: "".into(),
                exit_code: Some(0),
                is_timeout: false,
                elapsed: Duration::from_millis(100),
            }),
        ]);

        let calls = executor.call_handle();

        let mut step = create_step(false, None, None);
        step.extract = HashMap::from([("id".to_string(), "[0-9]+".to_string())]);
        let mut second_step = create_step(false, None, None);
        second_step.cmd = "get {{ id }}".to_string();
        step.expect.extend(vec![Expect {
            kind: ExpectKind::Exec(Box::new(second_step)),
            continue_on_failure: false,
        }]);

        let step_runner = StepRunner::new(executor, true);

        let mut ctx = HashMap::new();
        let result = step_runner.run(&step, &mut ctx);
        assert!(result.is_ok());

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2, "calls: {calls:?}");
        assert_eq!(calls[1], "get 123");

        // The nested step's extracted var should also be visible in the parent ctx
        assert_eq!(ctx.get("id").map(String::as_str), Some("123"));
    }

    #[test]
    fn continue_on_fail_runs_remaining_checks_then_fails() {
        let executor = FakeExecutor::with(vec![Ok(CommandOutput {
            stdout: "123".into(),
            stderr: "".into(),
            exit_code: Some(0),
            is_timeout: false,
            elapsed: Duration::from_millis(100),
        })]);

        let mut step = create_step(false, None, None);
        step.expect = vec![
            Expect {
                kind: ExpectKind::Stdout(vec![Matcher::Contains("456".to_string())]),
                continue_on_failure: true,
            },
            Expect {
                kind: ExpectKind::Stdout(vec![Matcher::Contains("012".to_string())]),
                continue_on_failure: true,
            },
        ];

        let step_runner = StepRunner::new(executor, true);

        let mut ctx = HashMap::new();
        let result = step_runner.run(&step, &mut ctx);
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("check 2 expect(s)"));
    }

    fn create_step(allow_failure: bool, retry: Option<Retry>, timeout: Option<Duration>) -> Step {
        Step {
            description: "try to ping 127.0.0.1".to_string(),
            cmd: "ping 127.0.0.1".to_string(),
            extract: Default::default(),
            expect: vec![Expect {
                kind: ExpectKind::ExitCode(0),
                continue_on_failure: false,
            }],
            allow_failure,
            retry,
            timeout,
        }
    }
}
