use crate::checker::config::{Expect, ExpectKind, JsonMatcher, JsonMatcherOp, Matcher, Step, Vars};
use crate::checker::executor::{CommandExecutor, CommandOutput};
use crate::checker::template::render;
use anyhow::Context;
use regex::Regex;
use serde_norway::Value;
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
            ExpectKind::Json(ms) => check_json(out, ms)?,
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

/// Check the command output by given stream.
/// If the stream does not match the given matchers, return an error.
fn check_stream(
    out: &CommandOutput,
    stream: Stream,
    ms: &[Matcher],
    ctx: &mut Vars,
) -> anyhow::Result<()> {
    for (i, m) in ms.iter().enumerate() {
        let name = stream.name();
        let text = stream.get(out);
        check_text(name, text, m, ctx)
            .with_context(|| format!("{} matcher #{}", stream.name(), i + 1))?;
    }

    Ok(())
}

/// The function to check the text against the given matcher.
/// If the text does not match the matcher, return an error.
fn check_text(name: &str, text: &str, m: &Matcher, ctx: &mut Vars) -> anyhow::Result<()> {
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

/// The function checks the exit_code
fn check_exit_code(out: &CommandOutput, want: i32) -> anyhow::Result<()> {
    anyhow::ensure!(
        out.exit_code == Some(want),
        "expected exit code {want}, got {:?}{}",
        out.exit_code,
        stderr_suffix(out)
    );
    Ok(())
}

/// The function checks the json output from the stdout
/// If the json output does not match the given json_matcher, return an error.
fn check_json(out: &CommandOutput, ms: &[JsonMatcher]) -> anyhow::Result<()> {
    let doc: Value = serde_norway::from_str(&out.stdout)?;
    for m in ms.iter() {
        inner_check_json(&doc, m)?
    }

    Ok(())
}

/// The function checks the json output from the stdout
/// If the json output does not match the given json_matcher, return an error.
fn inner_check_json(doc: &Value, json_matcher: &JsonMatcher) -> anyhow::Result<()> {
    let path = json_matcher
        .key
        .split('.')
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    let found = lookup(&doc, &path);
    let expected = &json_matcher.value;

    let passed = match json_matcher.op {
        JsonMatcherOp::Exists => found.is_some(),
        JsonMatcherOp::NotExists => found.is_none(),
        JsonMatcherOp::Eq => found.is_some_and(|v| json_eq(v, expected)),
        JsonMatcherOp::NotEq => !found.is_some_and(|v| json_eq(v, expected)),
        JsonMatcherOp::Contains => found.is_some_and(|v| json_contains(v, expected)),
        JsonMatcherOp::NotContains => !found.is_some_and(|v| json_contains(v, expected)),
    };

    if passed {
        return Ok(());
    }

    let actual = match found {
        Some(v) => format!("{v:?}"),
        None => "<missing>".to_string(),
    };

    match json_matcher.op {
        JsonMatcherOp::Exists | JsonMatcherOp::NotExists => anyhow::bail!(
            "json check failed: key `{}` {:?}, but it was {}",
            json_matcher.key,
            json_matcher.op,
            if found.is_some() {
                "present"
            } else {
                "missing"
            },
        ),
        _ => anyhow::bail!(
            "json check failed: `{}` {:?} {:?}\n  actual: {}",
            json_matcher.key,
            json_matcher.op,
            expected,
            actual,
        ),
    }
}

/// Checks if the json output contains the given needle.
fn json_contains(hay: &Value, needle: &Value) -> bool {
    match (hay, needle) {
        (Value::String(h), Value::String(n)) => h.contains(n.as_str()),
        (Value::Sequence(items), n) => items.iter().any(|i| json_eq(i, n)),
        (Value::Mapping(m), Value::String(k)) => m.contains_key(k),
        _ => false,
    }
}

/// Checks if the json output is equal to the given needle.
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

/// Looks up a value in the json output.
pub fn lookup<'a>(mut cur: &'a Value, path: &[String]) -> Option<&'a Value> {
    for seg in path {
        cur = match cur {
            Value::Mapping(m) => m.get(seg)?,
            Value::Sequence(a) => match seg.parse::<usize>() {
                Ok(i) => a.get(i)?,
                Err(_) => a.iter().find_map(|item| {
                    let obj = item.as_mapping()?;
                    (obj.get("key")?.as_str()? == seg)
                        .then(|| obj.get("value"))
                        .flatten()
                })?,
            },
            _ => return None,
        }
    }

    Some(cur)
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
    use crate::checker::config::{
        Expect, ExpectKind, Expr, JsonMatcher, JsonMatcherOp, Matcher, Retry, Step, Vars,
    };
    use crate::checker::executor::{CommandExecutor, CommandOutput};
    use crate::checker::runner::{StepRunner, check_json, check_text};
    use regex::Regex;
    use serde_norway::{Mapping, Number, Value};
    use std::collections::{HashMap, VecDeque};
    use std::str::FromStr;
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

    const LIMIT_DOC: &str = r#"{
    "a": "123",
    "zero": 0,
    "neg": -1,
    "neg_zero": -0.0,
    "max_u64": 18446744073709551615,
    "min_i64": -9223372036854775808,
    "big": 9007199254740993,
    "empty_str": "",
    "empty_list": [],
    "empty_obj": {},
    "nothing": null,
    "list": [1, 2, "three"],
    "a.b": "dotted",
    "numkey": { "0": "zero" },
    "名字": "unicode",
    "tags": [
        { "key": "env", "value": "prod" },
        { "key": "env", "value": "dev" }
    ]
}"#;

    fn output(stdout: &str) -> CommandOutput {
        CommandOutput {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: Some(0),
            is_timeout: false,
            elapsed: Duration::from_millis(100),
        }
    }

    fn jm(key: &str, op: JsonMatcherOp, value: Value) -> JsonMatcher {
        JsonMatcher {
            key: key.to_string(),
            op,
            value,
        }
    }

    fn s(v: &str) -> Value {
        Value::String(v.to_string())
    }

    fn num(v: &str) -> Value {
        Value::Number(Number::from_str(v).unwrap())
    }

    #[test]
    fn check_json_limits_pass() {
        use JsonMatcherOp::*;
        let out = output(LIMIT_DOC);

        let matcher = vec![
            // numeric boundaries
            jm("zero", Eq, num("0")),
            jm("zero", Eq, num("0.0")),
            jm("neg", Eq, num("-1")),
            jm("neg_zero", Eq, num("0")), // -0.0 == 0
            jm("max_u64", Eq, num("18446744073709551615")),
            jm("min_i64", Eq, num("-9223372036854775808")),
            // empty values
            jm("empty_str", Eq, s("")),
            jm("empty_str", Contains, s("")),
            jm("empty_list", Eq, Value::Sequence(vec![])),
            jm("empty_obj", Eq, Value::Mapping(Mapping::new())),
            jm("empty_list", NotContains, num("1")),
            jm("empty_obj", NotContains, s("x")),
            // null
            jm("nothing", Exists, Value::Null),
            jm("nothing", Eq, Value::Null),
            jm("nothing.x", NotExists, Value::Null), // can't descend into null
            // array index boundaries
            jm("list.0", Eq, num("1")),
            jm("list.2", Eq, s("three")),
            jm("list.3", NotExists, Value::Null), // one past the end
            jm("list.-1", NotExists, Value::Null), // negative index unsupported
            jm("list.99999999999999999999", NotExists, Value::Null), // overflows usize
            jm("list.01", Eq, num("2")),          // leading zero parses as 1
            // key edge cases
            jm("numkey.0", Eq, s("zero")), // numeric-looking map key
            jm("名字", Eq, s("unicode")),
            jm("a.b", NotExists, Value::Null), // dotted key is unreachable
            jm("list.", NotExists, Value::Null), // trailing dot -> empty segment
            jm("tags..env", NotExists, Value::Null), // empty middle segment
            // key/value array lookup
            jm("tags.env", Eq, s("prod")), // first match wins
            jm("tags.missing", NotExists, Value::Null),
            jm("tags.key", NotExists, Value::Null), // "key" field name isn't a key
            // descending through scalars
            jm("a.0", NotExists, Value::Null),
            jm("zero.x", NotExists, Value::Null),
        ];

        let res = check_json(&out, &matcher);
        assert!(res.is_ok(), "{res:?}");
    }

    #[test]
    fn check_json_limits_fail() {
        use JsonMatcherOp::*;
        let out = output(LIMIT_DOC);

        let cases = vec![
            jm("a", Eq, num("123")),           // string vs number
            jm("zero", Eq, Value::Null),       // 0 is not null
            jm("empty_str", Eq, Value::Null),  // "" is not null
            jm("empty_list", Eq, Value::Null), // [] is not null
            jm("empty_list", Contains, num("1")),
            jm("empty_obj", Contains, s("x")),
            jm("zero", Contains, num("0")), // contains on a number
            jm("nothing", NotExists, Value::Null), // null still exists
            jm("nothing", Contains, s("")), // contains on null
            jm("list.3", Exists, Value::Null),
            jm("list.-1", Exists, Value::Null),
            jm("a.b", Exists, Value::Null),
            jm("tags.env", Eq, s("dev")), // second duplicate is shadowed
            jm("", Exists, Value::Null),  // see note
        ];

        for m in cases {
            let desc = format!("{} {:?} {:?}", m.key, m.op, m.value);
            let res = check_json(&out, std::slice::from_ref(&m));
            assert!(res.is_err(), "expected failure: {desc}");
        }
    }

    #[test]
    fn check_json_deep_nesting() {
        let depth = 64;
        let doc = format!("{}1{}", r#"{"a":"#.repeat(depth), "}".repeat(depth));
        let key = vec!["a"; depth].join(".");

        let res = check_json(&output(&doc), &[jm(&key, JsonMatcherOp::Eq, num("1"))]);
        assert!(res.is_ok(), "{res:?}");
    }

    const LIMIT_TEXT: &str = "hello world\nLine2\twith tab\r\n你好，世界\ncafé\0\nend";

    fn check(text: &str, m: &Matcher) -> anyhow::Result<()> {
        check_text("stdout", text, m, &mut Vars::default())
    }

    fn c(v: &str) -> Matcher {
        Matcher::Contains(v.into())
    }
    fn nc(v: &str) -> Matcher {
        Matcher::NotContains(v.into())
    }
    fn eq(v: &str) -> Matcher {
        Matcher::Eq(v.into())
    }
    fn ne(v: &str) -> Matcher {
        Matcher::NotEq(v.into())
    }
    fn re(v: &str) -> Matcher {
        Matcher::Match(Expr(Regex::new(v).unwrap()))
    }

    /// Runs every case and reports all that don't match `expect_ok`.
    fn assert_all(text: &str, cases: &[Matcher], expect_ok: bool) {
        let bad: Vec<String> = cases
            .iter()
            .filter(|m| check(text, m).is_ok() != expect_ok)
            .map(|m| format!("{m:?}"))
            .collect();
        assert!(
            bad.is_empty(),
            "expected {} for:\n  {}",
            if expect_ok { "pass" } else { "failure" },
            bad.join("\n  ")
        );
    }

    #[test]
    fn check_text_limits_pass() {
        let cases = vec![
            // contains
            c(""), // empty needle always matches
            c("hello"),
            c("world\nLine2"), // spans a newline
            c("\t"),
            c("\r\n"),
            c("\0"), // NUL byte
            c("你好，世界"),
            c("café"),
            c(LIMIT_TEXT), // whole text contains itself
            // not contains
            nc("Hello"), // case-sensitive
            nc("\n\n"),
            nc("你好世界"),                // missing full-width comma
            nc("cafe\u{301}"),             // decomposed é is different bytes
            nc(&format!("{LIMIT_TEXT}!")), // longer than the text
            // eq / not eq
            eq(LIMIT_TEXT),
            ne(&format!("{LIMIT_TEXT}\n")), // trailing newline matters
            ne(&LIMIT_TEXT.replace("\r\n", "\n")),
            ne(&LIMIT_TEXT.to_uppercase()),
            ne(""),
            // regex
            re(""), // empty regex matches anything
            re(r"^hello"),
            re(r"end$"),
            re(r"(?m)^Line2"),
            re(r"(?i)HELLO"),
            re(r"(?s)hello.*end"), // dot matches newline with (?s)
            re(r"\p{Han}+"),
            re(r"\r\n"),
            re(r"\x00"),
            re(r"\bworld\b"),
        ];
        assert_all(LIMIT_TEXT, &cases, true);
    }

    #[test]
    fn check_text_limits_fail() {
        let cases = vec![
            c("Hello"),
            c("\n\n"),
            c("cafe\u{301}"),
            c(&format!("{LIMIT_TEXT}!")),
            nc(""), // every string contains ""
            nc("end"),
            nc("\0"),
            eq(""),
            eq(&format!("{LIMIT_TEXT}\n")),
            eq(&LIMIT_TEXT.replace("\r\n", "\n")),
            eq(&format!(" {LIMIT_TEXT}")), // leading whitespace matters
            ne(LIMIT_TEXT),
            re(r"^Line2"),     // ^ is start of text without (?m)
            re(r"hello.*end"), // dot doesn't match newline by default
            re(r"^$"),
            re(r"\bworl\b"),
            re(r"HELLO"),
        ];
        assert_all(LIMIT_TEXT, &cases, false);
    }

    #[test]
    fn check_text_empty_text() {
        assert_all(
            "",
            &[c(""), eq(""), nc("x"), ne("x"), re(""), re(r"^$")],
            true,
        );
        assert_all(
            "",
            &[c("x"), nc(""), eq("x"), ne(""), re(r"."), re(r"\s")],
            false,
        );
    }

    #[test]
    fn check_text_large_text() {
        let text = format!("{}needle", "a".repeat(1 << 20)); // ~1 MiB
        assert_all(
            &text,
            &[c("needle"), re(r"needle$"), nc("needles"), ne("needle")],
            true,
        );
        assert_all(&text, &[c("aneedlea"), re(r"^needle"), eq("needle")], false);
    }

    #[test]
    fn check_text_error_escapes_control_chars() {
        let msg = check("a\r\n\t\0", &eq("x")).unwrap_err().to_string();
        assert_eq!(msg, r#"stdout should equal "x", but was "a\r\n\t\0""#);
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
