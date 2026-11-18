use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_norway::Value;
use std::collections::HashMap;
use std::time::Duration;

pub type Vars = HashMap<String, String>;

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    /// The name of Task
    pub name: String,

    /// The variables for the task
    #[serde(default)]
    pub vars: Vars,

    /// Setup steps
    ///
    /// Before running the task, the setups will be executed in first order.
    #[serde(default)]
    pub setups: Vec<Step>,

    /// Run step
    ///
    /// The target task that will be executed.
    pub run: Step,

    /// Teardown steps
    ///
    /// A list of steps that will be executed after the task is finished for cleaning up.
    #[serde(default)]
    pub teardown: Vec<Step>,

    /// Polling after the task is finished
    ///
    /// If set, we will save the record to the SQLite and poll it periodically.
    pub poll: Option<Poll>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Step {
    /// Description of the step
    pub description: String,

    /// The command to be executed
    pub cmd: String,
    #[serde(default)]

    /// Extract variables from the output (stdout/stderr)
    pub extract: HashMap<String, String>,

    /// Expectations for the step
    #[serde(default)]
    pub expect: Vec<Expect>,

    /// Allow the step to fail
    ///
    /// If set to true, the step will continue executing even if it fails
    #[serde(default)]
    pub allow_failure: bool,

    /// Retry the step if it fails
    pub retry: Option<Retry>,

    /// Timeout for the step
    #[serde(default, with = "humantime_serde")]
    pub timeout: Option<Duration>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Expect {
    /// Contains different types of expectations
    #[serde(flatten)]
    pub kind: ExpectKind,

    /// Continue on failure
    ///
    /// If set to true, the step will continue executing even if this expectation fails
    #[serde(default)]
    pub continue_on_failure: bool,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum ExpectKind {
    /// Execute another step as an expectation
    Exec(Box<Step>),

    /// Expect the exit code of the step to match the given value
    ExitCode(i32),

    /// Expect the stdout of the step to match the given value
    Stdout(Vec<Matcher>),

    /// Expect the stderr of the step to match the given value
    Stderr(Vec<Matcher>),

    /// Expect the JSON output of the step to match the given value
    /// use stdout only
    Json(Vec<JsonMatcher>),
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(try_from = "String", into = "String")]
pub struct Expr(pub Regex);

impl TryFrom<String> for Expr {
    type Error = regex::Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Regex::new(&value).map(Expr)
    }
}

impl From<Expr> for String {
    fn from(value: Expr) -> Self {
        value.0.as_str().to_string()
    }
}

impl std::ops::Deref for Expr {
    type Target = Regex;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum Matcher {
    /// check if the string contains the given value
    Contains(String),

    /// check if the string is equal to the given value
    Eq(String),

    /// check if the string doesn't contain the given value
    NotContains(String),

    /// check if the string is not equal to the given value
    NotEq(String),

    /// check if the string matches the given regex
    Match(Expr),
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Retry {
    /// The number of times to retry the step
    pub times: i32,

    /// The interval between retries
    #[serde(default, with = "humantime_serde")]
    pub interval: Duration,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Poll {
    /// Polling when run is passed
    #[serde(default)]
    pub when: PollWhen,

    /// The step to run it periodically
    pub run: Step,

    /// The interval between polls
    #[serde(default, with = "humantime_serde")]
    pub interval: Duration,

    /// The maximum duration to run the polling
    #[serde(default, with = "humantime_serde")]
    pub max_duration: Duration,
}

#[derive(Debug, Default, Deserialize, Serialize, Eq, PartialEq, Clone)]
#[serde(rename_all = "snake_case")]
pub enum PollWhen {
    #[default]
    RunPassed,
    Always,
    RunFailed,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum JsonMatcherOp {
    Eq,
    NotEq,
    Contains,
    NotContains,
    Exists,
    NotExists,
    Match,
}

/// The raw structure for a JSON matcher and we will convert it to a JsonMatcher
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RawJsonMatcher {
    /// The key to match
    /// If we want to match a nested key, use dot notation, e.g. "a.b.c"
    pub key: String,

    /// The operation to perform on the value
    /// e.g. eq, neq, contains, not_contains, exists, not_exists
    pub op: JsonMatcherOp,

    /// The value we expect
    #[serde(default)]
    pub value: Value,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(try_from = "RawJsonMatcher", into = "RawJsonMatcher")]
pub struct JsonMatcher {
    /// The key to match
    /// If we want to match a nested key, use dot notation, e.g. "a.b.c"
    pub key: String,

    /// The operation to perform on the value
    /// e.g. eq, neq, contains, not_contains, exists, not_exists
    pub op: JsonMatcherOp,

    /// The value we expect
    pub value: Value,

    /// The regex we expect
    /// when the op == match, it will compile the value into a regex
    pub regex: Option<Regex>,
}

impl TryFrom<RawJsonMatcher> for JsonMatcher {
    type Error = String;

    fn try_from(raw: RawJsonMatcher) -> Result<Self, Self::Error> {
        if raw.key.is_empty() || raw.key.split('.').any(str::is_empty) {
            return Err(format!("invalid key {:?}", raw.key));
        }

        let regex = match raw.op {
            JsonMatcherOp::Match => {
                let Value::String(pat) = &raw.value else {
                    return Err(format!(
                        "`match` on {:?} needs a string pattern, got {:?}",
                        raw.key, raw.value
                    ));
                };
                Some(
                    Regex::new(pat)
                        .map_err(|e| format!("invalid regex {pat:?} for key {:?}: {e}", raw.key))?,
                )
            }
            JsonMatcherOp::Exists | JsonMatcherOp::NotExists => None,
            _ => None,
        };

        Ok(Self {
            key: raw.key,
            op: raw.op,
            value: raw.value,
            regex,
        })
    }
}

impl From<JsonMatcher> for RawJsonMatcher {
    fn from(value: JsonMatcher) -> Self {
        Self {
            key: value.key,
            op: value.op,
            value: value.value,
        }
    }
}
