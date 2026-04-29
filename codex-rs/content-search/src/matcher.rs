use anyhow::Result;
use grep_regex::RegexMatcher;
use grep_regex::RegexMatcherBuilder;

/// Build a `RegexMatcher` from a user query string.
///
/// * `regex == false`  -> the query is treated as a literal substring (escaped).
/// * `whole_word`      -> the pattern is wrapped in `\b...\b`.
/// * `case_sensitive`  -> when false, the matcher is case insensitive.
pub fn build_matcher(
    query: &str,
    case_sensitive: bool,
    regex: bool,
    whole_word: bool,
) -> Result<RegexMatcher> {
    let pat = if regex {
        query.to_string()
    } else {
        regex::escape(query)
    };
    let pat = if whole_word {
        format!(r"\b{pat}\b")
    } else {
        pat
    };
    let mut b = RegexMatcherBuilder::new();
    b.case_insensitive(!case_sensitive);
    Ok(b.build(&pat)?)
}
