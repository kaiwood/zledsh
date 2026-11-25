mod oauth;

use std::fmt;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::{Value, json};

const MAX_BODY: usize = 2 * 1024 * 1024;
const MAX_ANSWER: usize = 64 * 1024;

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug)]
pub struct Error(pub &'static str);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

pub struct Session {
    agent: ureq::Agent,
    token: String,
    pub model: String,
}

impl Session {
    pub fn connect(
        home: &Path,
        preferred_model: Option<&str>,
        cancelled: &AtomicBool,
    ) -> Result<Self> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .https_only(true)
            .proxy(None)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(30)))
            .user_agent(concat!("kwsh/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        let token = oauth::authorize(&agent, home, cancelled)?;
        oauth::check_cancelled(cancelled)?;
        let reply = agent
            .get("https://api.openai.com/v1/models")
            .header("Authorization", format!("Bearer {token}"))
            .call()
            .map_err(request_error)?;
        let model = choose_model(&read_json(reply)?, preferred_model)?;
        Ok(Self {
            agent,
            token,
            model,
        })
    }

    pub fn answer(&self, question: &str, cancelled: &AtomicBool) -> Result<String> {
        if question.len() > 8192 {
            return Err(Error("instructions must be at most 8192 bytes"));
        }
        oauth::check_cancelled(cancelled)?;
        let mut reply = self
            .agent
            .post("https://api.openai.com/v1/responses")
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Accept", "text/event-stream")
            .send_json(json!({
                "model": &self.model,
                "input": [{"role": "user", "content": question}],
                "store": false,
                "stream": true
            }))
            .map_err(request_error)?;
        let event_stream = reply
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("text/event-stream"))
            });
        if !reply.status().is_success() || !event_stream {
            return Err(Error("expected a ChatGPT event stream"));
        }
        parse_stream(reply.body_mut().as_reader(), cancelled)
    }
}

fn request_error(error: ureq::Error) -> Error {
    // Never display response bodies, URLs with OAuth values, or request headers.
    match error {
        ureq::Error::StatusCode(401) => {
            Error("ChatGPT session expired or was rejected; restart kwsh --chatgpt")
        }
        ureq::Error::StatusCode(403) => Error("ChatGPT denied access for this account or model"),
        ureq::Error::StatusCode(429) => Error("ChatGPT usage or rate limit reached"),
        _ => Error("ChatGPT request failed"),
    }
}

fn read_json(mut reply: ureq::http::Response<ureq::Body>) -> Result<Value> {
    if !reply.status().is_success() {
        return Err(Error("unexpected OpenAI response status"));
    }
    reply
        .body_mut()
        .with_config()
        .limit(MAX_BODY as u64)
        .read_json()
        .map_err(|_| Error("invalid or oversized OpenAI response"))
}

fn choose_model(catalog: &Value, preferred: Option<&str>) -> Result<String> {
    catalog["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|model| model["visibility"] == "list")
        .filter_map(|model| model["slug"].as_str())
        .filter(|slug| {
            !slug.is_empty()
                && slug.len() <= 128
                && slug
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
        .find(|slug| preferred.is_none_or(|model| model == *slug))
        .map(ToOwned::to_owned)
        .ok_or(Error(if preferred.is_some() {
            "configured model is not listed for this ChatGPT account"
        } else {
            "this ChatGPT account has no listed model"
        }))
}

fn parse_stream(input: impl Read, cancelled: &AtomicBool) -> Result<String> {
    let mut input = BufReader::new(input.take(MAX_BODY as u64));
    let mut answer = String::new();
    let mut data = String::new();
    let mut line = String::new();
    loop {
        oauth::check_cancelled(cancelled)?;
        line.clear();
        if input
            .read_line(&mut line)
            .map_err(|_| Error("could not read ChatGPT response"))?
            == 0
        {
            return Err(Error("ChatGPT response ended before completion"));
        }
        let field = line.trim_end_matches(['\r', '\n']);
        if let Some(value) = field.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
            continue;
        }
        if !field.is_empty() || data.is_empty() {
            continue;
        }
        let event: Value =
            serde_json::from_str(&data).map_err(|_| Error("invalid ChatGPT stream event"))?;
        data.clear();
        match event["type"].as_str() {
            Some("response.output_text.delta") => {
                let text = event["delta"]
                    .as_str()
                    .ok_or(Error("invalid ChatGPT text event"))?;
                if answer.len() + text.len() > MAX_ANSWER {
                    return Err(Error("ChatGPT answer exceeded 64 KiB"));
                }
                answer.push_str(text);
            }
            Some("response.completed") if !answer.trim().is_empty() => return Ok(answer),
            Some("response.completed") => {
                return Err(Error("ChatGPT completed without a text answer"));
            }
            Some("response.failed" | "response.incomplete" | "error") => {
                return Err(Error("ChatGPT did not complete the request"));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_only_visible_models_from_the_account() {
        let catalog = json!({"models": [
            {"slug": "hidden", "visibility": "hidden"},
            {"slug": "first", "visibility": "list"},
            {"slug": "second", "visibility": "list"}
        ]});
        assert_eq!(choose_model(&catalog, None).unwrap(), "first");
        assert_eq!(choose_model(&catalog, Some("second")).unwrap(), "second");
        assert!(choose_model(&catalog, Some("hidden")).is_err());
    }

    #[test]
    fn publishes_text_only_after_successful_completion() {
        let partial = concat!(
            "event: response.output_text.delta\r\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\r\n\r\n"
        );
        let cancelled = AtomicBool::new(false);
        let parse =
            |ending: &str| parse_stream(format!("{partial}{ending}").as_bytes(), &cancelled);
        assert!(parse("").is_err());
        for event in ["response.failed", "response.incomplete", "error"] {
            assert!(parse(&format!("data: {{\"type\":\"{event}\"}}\n\n")).is_err());
        }
        assert_eq!(
            parse("data: {\"type\":\"response.completed\"}\n\n").unwrap(),
            "hello"
        );
    }

    #[test]
    fn cancelled_stream_does_not_publish_a_reply() {
        let input = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\ndata: {\"type\":\"response.completed\"}\n\n";
        assert!(parse_stream(&input[..], &AtomicBool::new(true)).is_err());
    }
}
