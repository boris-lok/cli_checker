use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Debug)]
pub struct UndefinedVariableError(pub String);

impl std::error::Error for UndefinedVariableError {}

impl std::fmt::Display for UndefinedVariableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "undefined variable: {}", self.0)
    }
}

/// A regex for matching variable references in templates.
static VAR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{\{\s*([^}]+?)\s*}}").unwrap());

/// Render a template string, substituting variables from the given context.
/// Returns an error if a variable is referenced but not defined in the context.
///
/// ctx: { 'known' => 'v' }
/// e.g. `{{known}}` -> `v`
pub fn render(text: &str, ctx: &HashMap<String, String>) -> Result<String, UndefinedVariableError> {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;

    // extract the variable references from the template
    for caps in VAR_RE.captures_iter(text) {
        let whole = caps.get(0).unwrap();
        // push the text before the variable reference
        // e.g. `test-{{known}}` -> push `test-` to **out**
        out.push_str(&text[last..whole.start()]);
        // resolve the variable reference `known`
        out.push_str(&resolve(&caps[1], ctx)?);
        // move the cursor to the end of the variable reference
        last = whole.end();
    }
    out.push_str(&text[last..]);

    Ok(out)
}

/// Resolve a variable reference, returning the value or an error if the variable is undefined.
///
/// Support uuid, random, timestamp, date, env.<name>
/// e.g. `{{uuid}}` -> `01234567-89ab-cdef-0123-456789abcdef`
fn resolve(inner: &str, ctx: &HashMap<String, String>) -> Result<String, UndefinedVariableError> {
    // `None` means no `|` at all; `Some("")` means "default to the empty string".
    // Everything after the first `|` is the fallback, so a default may contain `|`.
    let (expr, fallback) = match inner.split_once('|') {
        Some((expr, fallback)) => (expr.trim(), Some(fallback.trim())),
        None => (inner.trim(), None),
    };

    Ok(match expr {
        "uuid" => uuid::Uuid::new_v4().to_string(),
        "random" => uuid::Uuid::new_v4().simple().to_string()[..8].to_string(),
        "timestamp" => chrono::Utc::now().timestamp().to_string(),
        "date" => chrono::Local::now().format("%Y-%m-%d").to_string(),
        _ => {
            let found = match expr.strip_prefix("env.") {
                Some(name) => std::env::var(name).ok(),
                None => ctx.get(expr).cloned(),
            };
            match (found, fallback) {
                (Some(v), _) => v,
                (None, Some(f)) => f.to_string(),
                (None, None) => return Err(UndefinedVariableError(expr.to_string())),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use crate::checker::template::render;
    use std::collections::HashMap;

    #[test]
    fn test_render() {
        let ctx = HashMap::from([("known".to_string(), "v".to_string())]);
        assert_eq!(render("{{ known | fb }}", &ctx).unwrap(), "v");
        assert_eq!(render("text-{{known}}", &ctx).unwrap(), "text-v");
        assert_eq!(render("{{ x | }}", &ctx).unwrap(), ""); // empty default
        assert_eq!(render("{{ x | a|b }}", &ctx).unwrap(), "a|b"); // pipe in default
        assert!(render("{{ x }}", &ctx).is_err());
    }
}
