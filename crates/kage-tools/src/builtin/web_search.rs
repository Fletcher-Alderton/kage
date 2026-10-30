//! `web_search` tool: search the web and return the results' titles,
//! URLs and snippets.
//!
//! DuckDuckGo's HTML endpoint answers by default and needs no key.
//! `[tools.web_search]` can name a SearXNG instance's JSON API instead,
//! or the Brave Search API with a key, for a search that does not
//! depend on scraping a page.

use std::fmt::Write as _;
use std::io::Read as _;
use std::time::Duration;

use kage_core::config::{SearchEngine, WebSearchConfig};
use kage_core::{Risk, ToolOutput};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Tool, ToolContext, ToolError, schema_for, ssrf};

/// The variable the Brave Search key is read from by default.
const BRAVE_KEY_ENV: &str = "BRAVE_API_KEY";

/// The most results Brave's `count` parameter accepts. Its API documents
/// 20 as the ceiling, and `[tools.web_search] max_count` does not bound
/// it, so a call is clamped here whatever the config says.
const BRAVE_MAX_COUNT: usize = 20;

/// The largest answer page read.
const MAX_BYTES: u64 = 2_000_000;

/// Input shape for the `web_search` tool.
#[derive(Debug, Deserialize, JsonSchema)]
struct WebSearchInput {
    /// What to search for.
    query: String,
    /// How many results to return. Defaults to 8, at most 20.
    #[serde(default)]
    count: Option<usize>,
}

/// One search result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchResult {
    /// The page title.
    pub title: String,
    /// The page URL.
    pub url: String,
    /// The engine's excerpt of the page.
    pub snippet: String,
}

/// Search the web through the configured engine.
#[derive(Debug, Default)]
pub struct WebSearchTool {
    config: WebSearchConfig,
}

impl WebSearchTool {
    /// A tool searching through the engine `config` names.
    #[must_use]
    pub fn new(config: WebSearchConfig) -> Self {
        Self { config }
    }

    /// The most results a call may ask for: `[tools.web_search]
    /// max_count`, but never above what the engine accepts. Brave
    /// documents `count` as max 20, so a larger `max_count` would send a
    /// value its API does not promise to honour.
    fn max_count(&self) -> usize {
        match self.config.engine {
            SearchEngine::Brave => self.config.max_count.min(BRAVE_MAX_COUNT),
            SearchEngine::Duckduckgo | SearchEngine::Searxng => self.config.max_count,
        }
    }
}

impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> &'static str {
        "Search the web. Returns up to `count` results, each with its title, \
         URL and a short snippet. Use web_fetch to read a result's page."
    }

    fn schema(&self) -> serde_json::Value {
        let mut schema = schema_for::<WebSearchInput>();
        crate::schema::set_property_doc(
            &mut schema,
            "count",
            &format!(
                "How many results to return. Defaults to {}, at most {}.",
                // A `count` above the ceiling is clamped at call time, so
                // the doc must not promise more than the call can return.
                self.config.count.min(self.max_count()),
                self.max_count()
            ),
        );
        schema
    }

    fn risk(&self) -> Risk {
        Risk::Network
    }

    fn execute(
        &self,
        input: serde_json::Value,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let input: WebSearchInput = serde_json::from_value(input)?;
        let query = input.query.trim();
        if query.is_empty() {
            return Err(ToolError::InvalidInput("query is empty".to_owned()));
        }
        if cx.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let count = input
            .count
            .unwrap_or(self.config.count)
            .clamp(1, self.max_count().max(1));
        let timeout = Duration::from_millis(self.config.timeout_ms);
        let mut results = match self.config.engine {
            SearchEngine::Duckduckgo => duckduckgo(query, timeout)?,
            SearchEngine::Searxng => {
                let base = self.config.url.as_deref().ok_or_else(|| {
                    ToolError::Other(
                        "[tools.web_search] engine = \"searxng\" needs a url".to_owned(),
                    )
                })?;
                searxng(base, query, timeout)?
            }
            SearchEngine::Brave => {
                let env = self.config.api_key_env.as_deref().unwrap_or(BRAVE_KEY_ENV);
                let key = std::env::var(env)
                    .ok()
                    .filter(|key| !key.is_empty())
                    .ok_or_else(|| {
                        ToolError::Other(format!("Brave Search needs a key: {env} is not set"))
                    })?;
                brave(&key, query, count, timeout)?
            }
        };
        results.truncate(count);
        Ok(output(query, &results))
    }
}

/// The tool's answer: a numbered list for the model, the results as data
/// for a client.
fn output(query: &str, results: &[SearchResult]) -> ToolOutput {
    let mut text = String::new();
    if results.is_empty() {
        let _ = write!(text, "No results for {query}.");
    }
    for (ix, result) in results.iter().enumerate() {
        let _ = writeln!(text, "{}. {}\n   {}", ix + 1, result.title, result.url);
        if !result.snippet.is_empty() {
            let _ = writeln!(text, "   {}", result.snippet);
        }
    }
    ToolOutput {
        is_error: false,
        text: text.trim_end().to_owned(),
        structured: Some(serde_json::json!({ "query": query, "results": results })),
        terminate: false,
        media: Vec::new(),
    }
}

fn get(url: &url::Url, headers: &[(&str, &str)], timeout: Duration) -> Result<String, ToolError> {
    let agent = ssrf::agent_for_url(
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false),
        url,
    )?;
    let mut request = agent
        .get(url.as_str())
        .header("user-agent", "kage/0.1 (+https://github.com/QaidVoid/kage)");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let response = request
        .call()
        .map_err(|e| ToolError::Other(format!("search failed: {e}")))?;
    let status = response.status().as_u16();
    let mut body = String::new();
    response
        .into_body()
        .into_reader()
        .take(MAX_BYTES)
        .read_to_string(&mut body)
        .map_err(|e| ToolError::Other(format!("read search results: {e}")))?;
    if !(200..300).contains(&status) {
        let host = url.host_str().unwrap_or("the engine");
        return Err(ToolError::Other(match error_detail(&body) {
            Some(detail) => format!("{host} answered http {status}: {detail}"),
            None => format!("{host} answered http {status}"),
        }));
    }
    Ok(body)
}

/// The reason a JSON error answer gives, as Brave Search and most JSON
/// APIs put it.
fn error_detail(body: &str) -> Option<String> {
    let json: serde_json::Value = serde_json::from_str(body).ok()?;
    [
        &json["error"]["detail"],
        &json["error"]["message"],
        &json["message"],
    ]
    .into_iter()
    .find_map(|detail| detail.as_str().map(str::to_owned))
}

fn duckduckgo(query: &str, timeout: Duration) -> Result<Vec<SearchResult>, ToolError> {
    let mut url = url::Url::parse("https://html.duckduckgo.com/html/").expect("static url");
    url.query_pairs_mut().append_pair("q", query);
    let page = get(&url, &[], timeout)?;
    let results = parse_duckduckgo(&page);
    if results.is_empty() && page.contains("anomaly") {
        return Err(ToolError::Other(
            "DuckDuckGo refused the search as automated; try again later, or set a SearXNG \
             instance or a Brave Search key under [tools.web_search]"
                .to_owned(),
        ));
    }
    Ok(results)
}

/// The results of a DuckDuckGo HTML answer page, ads left out.
fn parse_duckduckgo(page: &str) -> Vec<SearchResult> {
    let mut results = Vec::new();
    for block in page.split("class=\"result__a\"").skip(1) {
        let Some(href) = attr(block, "href") else {
            continue;
        };
        let Some(url) = result_url(&href) else {
            continue;
        };
        let title = block
            .split_once('>')
            .and_then(|(_, rest)| rest.split_once("</a>"))
            .map(|(title, _)| text_of(title))
            .unwrap_or_default();
        let snippet = block
            .split_once("class=\"result__snippet\"")
            .and_then(|(_, rest)| rest.split_once('>'))
            .map(|(_, rest)| {
                let end = ["</a>", "</div>"]
                    .iter()
                    .filter_map(|close| rest.find(close))
                    .min()
                    .unwrap_or(rest.len());
                text_of(&rest[..end])
            })
            .unwrap_or_default();
        if title.is_empty() || results.iter().any(|r: &SearchResult| r.url == url) {
            continue;
        }
        results.push(SearchResult {
            title,
            url,
            snippet,
        });
    }
    results
}

/// The value of attribute `name` at the start of `tag`.
fn attr(tag: &str, name: &str) -> Option<String> {
    let start = tag.find(&format!("{name}=\""))? + name.len() + 2;
    let end = tag[start..].find('"')? + start;
    Some(decode(&tag[start..end]))
}

/// The page a DuckDuckGo result link leads to: the `uddg` target of its
/// redirect, or the link itself. `None` for an ad.
fn result_url(href: &str) -> Option<String> {
    let absolute = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_owned()
    };
    let parsed = url::Url::parse(&absolute).ok()?;
    if parsed.path().starts_with("/y.js") {
        return None;
    }
    match parsed.query_pairs().find(|(key, _)| key == "uddg") {
        Some((_, target)) => Some(target.into_owned()),
        None => Some(absolute),
    }
}

/// `html` as plain text: tags dropped, entities decoded, spaces folded.
fn text_of(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    decode(&text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

/// Builds the SearXNG search endpoint for `base` and `query`. The base
/// is normalised to end in `/` first: `Url::join` is the RFC 3986
/// merge, which treats the base's last path segment as a file name, so
/// a plain join would turn `https://host/searxng` into
/// `https://host/search`.
fn searxng_url(base: &str, query: &str) -> Result<url::Url, String> {
    let mut base = url::Url::parse(base).map_err(|e| format!("[tools.web_search] url: {e}"))?;
    let path = base.path();
    if !path.is_empty() && !path.ends_with('/') {
        base.set_path(&format!("{path}/"));
    }
    let mut endpoint = base
        .join("search")
        .map_err(|e| format!("[tools.web_search] url: {e}"))?;
    endpoint
        .query_pairs_mut()
        .append_pair("q", query)
        .append_pair("format", "json");
    Ok(endpoint)
}

fn searxng(base: &str, query: &str, timeout: Duration) -> Result<Vec<SearchResult>, ToolError> {
    let url = searxng_url(base, query).map_err(ToolError::Other)?;
    let endpoint = match url.host_str() {
        Some(host) => format!("{host}{}", url.path()),
        None => "the SearXNG instance".to_owned(),
    };
    let body = get(&url, &[], timeout)?;
    let json: serde_json::Value = serde_json::from_str(&body).map_err(|_| {
        ToolError::Other(format!(
            "{endpoint} did not answer with JSON; enable the json format in its settings"
        ))
    })?;
    Ok(json["results"]
        .as_array()
        .map(|results| {
            results
                .iter()
                .filter_map(|result| {
                    Some(SearchResult {
                        title: result["title"].as_str()?.to_owned(),
                        url: result["url"].as_str()?.to_owned(),
                        snippet: result["content"].as_str().unwrap_or_default().to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

fn brave(
    key: &str,
    query: &str,
    count: usize,
    timeout: Duration,
) -> Result<Vec<SearchResult>, ToolError> {
    let mut url =
        url::Url::parse("https://api.search.brave.com/res/v1/web/search").expect("static url");
    url.query_pairs_mut()
        .append_pair("q", query)
        .append_pair("count", &count.to_string());
    let body = get(
        &url,
        &[
            ("accept", "application/json"),
            ("x-subscription-token", key),
        ],
        timeout,
    )?;
    let json: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| ToolError::Other(format!("Brave Search answered something else: {e}")))?;
    Ok(parse_brave(&json))
}

/// The web results of a Brave Search API answer.
fn parse_brave(json: &serde_json::Value) -> Vec<SearchResult> {
    json["web"]["results"]
        .as_array()
        .map(|results| {
            results
                .iter()
                .filter_map(|result| {
                    Some(SearchResult {
                        title: text_of(result["title"].as_str()?),
                        url: result["url"].as_str()?.to_owned(),
                        snippet: text_of(result["description"].as_str().unwrap_or_default()),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{output, parse_brave, parse_duckduckgo, result_url, searxng_url};

    #[test]
    fn searxng_url_keeps_a_base_path_without_a_trailing_slash() {
        let url = searxng_url("https://host/searxng", "rust").unwrap();
        assert!(
            url.as_str().starts_with("https://host/searxng/search?"),
            "{}",
            url.as_str()
        );
    }

    #[test]
    fn searxng_url_keeps_a_base_path_with_a_trailing_slash() {
        let url = searxng_url("https://host/searxng/", "rust").unwrap();
        assert!(
            url.as_str().starts_with("https://host/searxng/search?"),
            "{}",
            url.as_str()
        );
    }

    #[test]
    fn searxng_url_joins_a_root_base_and_carries_the_query() {
        let url = searxng_url("https://host", "rust engines").unwrap();
        assert_eq!(
            url.as_str(),
            "https://host/search?q=rust+engines&format=json"
        );
    }

    /// Brave documents `count` as max 20. `max_count` is not bounded by
    /// `validate`, so a larger one must not reach the API, and the schema
    /// the model reads must state the real ceiling.
    #[test]
    fn brave_count_never_exceeds_twenty() {
        use crate::Tool as _;

        let brave = super::WebSearchTool::new(kage_core::config::WebSearchConfig {
            engine: kage_core::config::SearchEngine::Brave,
            max_count: 50,
            ..Default::default()
        });
        assert_eq!(brave.max_count(), 20);
        let doc = brave.schema()["properties"]["count"]["description"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(doc.contains("at most 20"), "{doc}");

        // A `count` above the ceiling is clamped, so the doc says 20.
        let over = super::WebSearchTool::new(kage_core::config::WebSearchConfig {
            engine: kage_core::config::SearchEngine::Brave,
            count: 25,
            max_count: 30,
            ..Default::default()
        });
        let doc = over.schema()["properties"]["count"]["description"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(doc.contains("Defaults to 20, at most 20"), "{doc}");

        // Every other engine keeps the configured ceiling.
        let searxng = super::WebSearchTool::new(kage_core::config::WebSearchConfig {
            engine: kage_core::config::SearchEngine::Searxng,
            max_count: 50,
            ..Default::default()
        });
        assert_eq!(searxng.max_count(), 50);
    }

    #[test]
    fn brave_without_a_key_names_the_variable() {
        use crate::{Tool as _, ToolContext};

        let tool = super::WebSearchTool::new(kage_core::config::WebSearchConfig {
            engine: kage_core::config::SearchEngine::Brave,
            url: None,
            api_key_env: Some("KAGE_TEST_UNSET_BRAVE_KEY".into()),
            ..Default::default()
        });
        let cancel = kage_core::CancelFlag::new();
        let err = tool
            .execute(
                serde_json::json!({"query": "x"}),
                &ToolContext::new(std::path::Path::new("."), &cancel),
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("KAGE_TEST_UNSET_BRAVE_KEY"),
            "{err}"
        );
    }

    #[test]
    fn an_error_answer_gives_its_reason() {
        let brave = r#"{"error":{"code":"SUBSCRIPTION_TOKEN_INVALID","detail":"The provided subscription token is invalid.","status":422}}"#;
        assert_eq!(
            super::error_detail(brave).as_deref(),
            Some("The provided subscription token is invalid.")
        );
        assert_eq!(super::error_detail("<html>busy</html>"), None);
    }

    #[test]
    fn a_brave_answer_reads_as_results() {
        let json = serde_json::json!({"web": {"results": [
            {"title": "Backoff &amp; jitter", "url": "https://a.example/",
             "description": "Add <strong>jitter</strong> to retries."},
            {"title": "No url"},
            {"title": "Bare", "url": "https://b.example/"}
        ]}});
        let results = parse_brave(&json);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Backoff & jitter");
        assert_eq!(results[0].snippet, "Add jitter to retries.");
        assert_eq!(results[1].snippet, "");
        assert!(parse_brave(&serde_json::json!({})).is_empty());
    }

    const PAGE: &str = r#"
        <div class="result results_links">
          <h2 class="result__title">
            <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Faws.amazon.com%2Fblogs%2Fbackoff%2F&amp;rut=abc">Exponential Backoff &amp; <b>Jitter</b></a>
          </h2>
          <a class="result__snippet" href="//duckduckgo.com/l/?uddg=x">Add <b>jitter</b> to   spread retries.</a>
        </div>
        <div class="result result--ad">
          <a rel="nofollow" class="result__a" href="https://duckduckgo.com/y.js?ad_domain=x">Buy retries</a>
        </div>
        <div class="result">
          <a rel="nofollow" class="result__a" href="https://example.com/plain">Plain</a>
        </div>
    "#;

    /// Asks DuckDuckGo for real; run with `--ignored` to check the page
    /// shape still parses.
    #[test]
    #[ignore = "reaches the network"]
    fn duckduckgo_answers_with_results() {
        let results = super::duckduckgo(
            "rust programming language",
            std::time::Duration::from_secs(20),
        )
        .unwrap();
        assert!(results.len() >= 3, "{results:?}");
        assert!(
            results.iter().all(|r| r.url.starts_with("http")),
            "{results:?}"
        );
    }

    #[test]
    fn a_duckduckgo_page_reads_as_results_without_ads() {
        let results = parse_duckduckgo(PAGE);
        assert_eq!(results.len(), 2, "{results:?}");
        assert_eq!(results[0].title, "Exponential Backoff & Jitter");
        assert_eq!(results[0].url, "https://aws.amazon.com/blogs/backoff/");
        assert_eq!(results[0].snippet, "Add jitter to spread retries.");
        assert_eq!(results[1].url, "https://example.com/plain");
        assert_eq!(results[1].snippet, "");
        assert_eq!(result_url("https://duckduckgo.com/y.js?x=1"), None);
    }

    #[test]
    fn the_answer_numbers_the_results_and_carries_them_as_data() {
        let out = output("jitter", &parse_duckduckgo(PAGE));
        assert!(out.text.starts_with("1. Exponential Backoff & Jitter\n   https://aws.amazon.com/blogs/backoff/\n   Add jitter"), "{}", out.text);
        let structured = out.structured.unwrap();
        assert_eq!(structured["query"], "jitter");
        assert_eq!(structured["results"][1]["title"], "Plain");
        assert_eq!(output("none", &[]).text, "No results for none.");
    }
}
