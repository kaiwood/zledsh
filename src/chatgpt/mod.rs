mod credentials;
mod oauth;

use std::fmt;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::{Value, json};

const MAX_BODY: usize = 2 * 1024 * 1024;
const MAX_ANSWER: usize = 64 * 1024;

type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    SignIn,
    Message(&'static str),
    Response {
        message: &'static str,
        status: u16,
        format: &'static str,
        request_id: Option<String>,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SignIn => f.write_str("ChatGPT sign-in required; run kwsh --chatgpt"),
            Self::Message(message) => f.write_str(message),
            Self::Response {
                message,
                status,
                format,
                request_id,
            } => {
                write!(f, "{message} (HTTP {status}, {format}")?;
                if let Some(id) = request_id {
                    write!(f, ", request ID {id}")?;
                }
                f.write_str(")")
            }
        }
    }
}

#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    model: Option<String>,
    reasoning_effort: Option<String>,
}

impl Settings {
    fn load(home: &Path) -> Result<Self> {
        const INVALID: Error =
            Error::Message("invalid ~/.kwsh/chatgpt.json; expected model and/or reasoning_effort");
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(home.join(".kwsh/chatgpt.json"))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(_) => return Err(INVALID),
        };
        let metadata = file.metadata().map_err(|_| INVALID)?;
        if !metadata.is_file() || metadata.len() > 4096 {
            return Err(INVALID);
        }
        let mut bytes = Vec::new();
        file.take(4097)
            .read_to_end(&mut bytes)
            .map_err(|_| INVALID)?;
        if bytes.len() > 4096 {
            return Err(INVALID);
        }
        let settings: Self = serde_json::from_slice(&bytes).map_err(|_| INVALID)?;
        if settings.reasoning_effort.as_deref().is_some_and(|effort| {
            !matches!(
                effort,
                "none" | "minimal" | "low" | "medium" | "high" | "xhigh"
            )
        }) {
            return Err(Error::Message(
                "reasoning_effort must be none, minimal, low, medium, high, or xhigh",
            ));
        }
        Ok(settings)
    }
}

pub struct Session {
    agent: ureq::Agent,
    home: PathBuf,
    pub model: String,
    reasoning_effort: Option<String>,
}

impl Session {
    pub fn connect(
        home: &Path,
        preferred_model: Option<&str>,
        allow_sign_in: bool,
        cancelled: &AtomicBool,
    ) -> Result<Self> {
        let settings = Settings::load(home)?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .https_only(true)
            .proxy(None)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(30)))
            .user_agent(concat!("kwsh/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        let token = access_token(&agent, home, allow_sign_in, cancelled)?;
        oauth::check_cancelled(cancelled)?;
        let reply = agent
            .get("https://api.openai.com/v1/models")
            .header("Authorization", format!("Bearer {token}"))
            .call()
            .map_err(request_error)?;
        let model = choose_model(
            &read_json(reply)?,
            preferred_model.or(settings.model.as_deref()),
        )?;
        Ok(Self {
            agent,
            home: home.to_owned(),
            model,
            reasoning_effort: settings.reasoning_effort,
        })
    }

    pub fn answer(&self, question: &str, cancelled: &AtomicBool) -> Result<String> {
        if question.len() > 8192 {
            return Err(Error::Message("instructions must be at most 8192 bytes"));
        }
        oauth::check_cancelled(cancelled)?;
        let token = access_token(&self.agent, &self.home, false, cancelled)?;
        oauth::check_cancelled(cancelled)?;
        let reply = self
            .agent
            .post("https://api.openai.com/v1/responses")
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "text/event-stream")
            .config()
            .http_status_as_error(false)
            .build()
            .send_json(request_body(
                &self.model,
                self.reasoning_effort.as_deref(),
                question,
            ))
            .map_err(request_error)?;
        read_answer(reply, cancelled)
    }
}

fn request_body(model: &str, effort: Option<&str>, question: &str) -> Value {
    let mut body = json!({
        "model": model,
        "input": [{"role": "user", "content": question}],
        "store": false,
        "stream": true
    });
    if let Some(effort) = effort {
        body["reasoning"] = json!({"effort": effort});
    }
    body
}

fn access_token(
    agent: &ureq::Agent,
    home: &Path,
    allow_sign_in: bool,
    cancelled: &AtomicBool,
) -> Result<String> {
    let store = credentials::Store::lock(home, cancelled)?;
    let previous = store.load()?;
    let current = match previous.as_ref() {
        Some(saved) if saved.usable_at(credentials::now()?) => return Ok(saved.access.clone()),
        Some(saved) => match oauth::refresh(agent, saved) {
            Err(Error::SignIn) if allow_sign_in => {
                oauth::authorize(agent, store.directory(), cancelled, Some(saved))?
            }
            result => result?,
        },
        None if allow_sign_in => oauth::authorize(agent, store.directory(), cancelled, None)?,
        None => return Err(Error::SignIn),
    };
    // A refresh may already have rotated the token by the time Ctrl+C arrives.
    // Commit the replacement before honoring cancellation in the caller.
    store.save(&current)?;
    Ok(current.access)
}

fn read_answer(
    mut reply: ureq::http::Response<ureq::Body>,
    cancelled: &AtomicBool,
) -> Result<String> {
    oauth::check_cancelled(cancelled)?;
    let status = reply.status();
    let media_type = reply
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(str::trim);
    let format = match media_type {
        Some(kind) if kind.eq_ignore_ascii_case("text/event-stream") => "event stream",
        Some(kind)
            if kind.eq_ignore_ascii_case("application/json")
                || kind.eq_ignore_ascii_case("application/problem+json") =>
        {
            "JSON"
        }
        Some(kind) if kind.eq_ignore_ascii_case("text/html") => "HTML",
        None => "missing content type",
        _ => "unexpected content type",
    };
    let request_id = reply
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|id| {
            id.strip_prefix("req_").is_some_and(|suffix| {
                !suffix.is_empty()
                    && suffix.len() <= 64
                    && suffix.bytes().all(|b| b.is_ascii_hexdigit())
            })
        })
        .map(ToOwned::to_owned);
    let result = match (status.is_success(), format) {
        (true, "event stream") => parse_stream(reply.body_mut().as_reader(), cancelled),
        (success, "JSON") => read_json_answer(
            reply.body_mut().as_reader(),
            success,
            status.as_u16(),
            cancelled,
        ),
        (true, "missing content type") => {
            read_untyped_answer(reply.body_mut().as_reader(), cancelled)
        }
        (false, _) => Err(Error::Message(status_message(status.as_u16()))),
        (true, _) => Err(Error::Message(
            "expected a ChatGPT event stream or completed JSON response",
        )),
    };
    result.map_err(|e| with_response(e, status.as_u16(), format, request_id))
}

fn read_json_answer(
    input: impl Read,
    success: bool,
    status: u16,
    cancelled: &AtomicBool,
) -> Result<String> {
    let mut body = Vec::new();
    input
        .take((MAX_BODY + 1) as u64)
        .read_to_end(&mut body)
        .map_err(|_| Error::Message("could not read ChatGPT JSON response"))?;
    if body.len() > MAX_BODY {
        return Err(Error::Message("oversized ChatGPT JSON response"));
    }
    oauth::check_cancelled(cancelled)?;
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| Error::Message("invalid ChatGPT JSON response"))?;
    if let Some(message) = service_error(&value["error"]) {
        Err(Error::Message(message))
    } else if success {
        completed_json(&value)
    } else {
        Err(Error::Message(status_message(status)))
    }
}

fn read_untyped_answer(input: impl Read, cancelled: &AtomicBool) -> Result<String> {
    let mut input = BufReader::new(input.take((MAX_BODY + 1) as u64));
    // Classify by the first non-whitespace byte, then run the normal parser.
    // Neither path publishes output without a completed Responses result.
    let first = loop {
        oauth::check_cancelled(cancelled)?;
        let bytes = input
            .fill_buf()
            .map_err(|_| Error::Message("could not read ChatGPT response"))?;
        if let Some(index) = bytes.iter().position(|b| !b.is_ascii_whitespace()) {
            let first = bytes[index];
            input.consume(index);
            break first;
        }
        if bytes.is_empty() {
            return Err(Error::Message("ChatGPT returned an empty response"));
        }
        let len = bytes.len();
        input.consume(len);
    };
    match first {
        b'{' => read_json_answer(input, true, 200, cancelled),
        b':' | b'd' | b'e' | b'i' | b'r' => parse_stream(input, cancelled),
        _ => Err(Error::Message("unrecognized ChatGPT response format")),
    }
}

fn with_response(
    error: Error,
    status: u16,
    format: &'static str,
    request_id: Option<String>,
) -> Error {
    match error {
        Error::Message(message) => Error::Response {
            message,
            status,
            format,
            request_id,
        },
        response => response,
    }
}

fn completed_json(value: &Value) -> Result<String> {
    if value["object"] != "response" || value["status"] != "completed" || !value["error"].is_null()
    {
        return Err(Error::Message(
            "ChatGPT returned JSON without a completed response",
        ));
    }
    let mut answer = String::new();
    for item in value["output"].as_array().into_iter().flatten() {
        if item["type"] != "message" || item["role"] != "assistant" {
            continue;
        }
        for part in item["content"].as_array().into_iter().flatten() {
            if part["type"] == "output_text" {
                let text = part["text"]
                    .as_str()
                    .ok_or(Error::Message("invalid ChatGPT text output"))?;
                append_text(&mut answer, text)?;
            }
        }
    }
    if answer.trim().is_empty() {
        return Err(Error::Message("ChatGPT completed without a text answer"));
    }
    Ok(answer)
}

fn append_text(answer: &mut String, text: &str) -> Result<()> {
    if answer.len() + text.len() > MAX_ANSWER {
        return Err(Error::Message("ChatGPT answer exceeded 64 KiB"));
    }
    answer.push_str(text);
    Ok(())
}

fn service_error(error: &Value) -> Option<&'static str> {
    match error["code"].as_str()? {
        "subscription_sharing_user_not_eligible" => {
            Some("this ChatGPT account or workspace is not eligible for plan usage")
        }
        "subscription_sharing_usage_limit_exceeded" => Some("ChatGPT plan usage limit reached"),
        "subscription_sharing_usage_unavailable" => {
            Some("ChatGPT plan usage is temporarily unavailable")
        }
        "subscription_sharing_unsupported_capability" => {
            Some("ChatGPT plan usage does not support this request or model")
        }
        _ => None,
    }
}

fn status_message(status: u16) -> &'static str {
    match status {
        401 => "ChatGPT session expired or was rejected; restart kwsh --chatgpt",
        403 => "ChatGPT denied access for this account or model",
        429 => "ChatGPT usage or rate limit reached",
        503 => "ChatGPT service is temporarily unavailable",
        _ => "ChatGPT request failed",
    }
}

fn request_error(error: ureq::Error) -> Error {
    // Never display response bodies, URLs with OAuth values, or request headers.
    Error::Message(match error {
        ureq::Error::StatusCode(status) => status_message(status),
        _ => "ChatGPT request failed",
    })
}

fn read_json(mut reply: ureq::http::Response<ureq::Body>) -> Result<Value> {
    if !reply.status().is_success() {
        return Err(Error::Message("unexpected OpenAI response status"));
    }
    reply
        .body_mut()
        .with_config()
        .limit(MAX_BODY as u64)
        .read_json()
        .map_err(|_| Error::Message("invalid or oversized OpenAI response"))
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
        .ok_or(Error::Message(if preferred.is_some() {
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
            .map_err(|_| Error::Message("could not read ChatGPT response"))?
            == 0
        {
            return Err(Error::Message("ChatGPT response ended before completion"));
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
        let event: Value = serde_json::from_str(&data)
            .map_err(|_| Error::Message("invalid ChatGPT stream event"))?;
        data.clear();
        match event["type"].as_str() {
            Some("response.output_text.delta") => {
                let text = event["delta"]
                    .as_str()
                    .ok_or(Error::Message("invalid ChatGPT text event"))?;
                append_text(&mut answer, text)?;
            }
            Some("response.completed") if !answer.trim().is_empty() => return Ok(answer),
            Some("response.completed") => {
                return Err(Error::Message("ChatGPT completed without a text answer"));
            }
            Some("response.failed" | "response.incomplete" | "error") => {
                let message = service_error(&event["response"]["error"])
                    .or_else(|| service_error(&event))
                    .unwrap_or("ChatGPT did not complete the request");
                return Err(Error::Message(message));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resumes_saved_credentials_without_network_or_browser() {
        let home = tempfile::tempdir().unwrap();
        let cancelled = AtomicBool::new(false);
        let agent = ureq::Agent::new_with_defaults();
        assert!(matches!(
            access_token(&agent, home.path(), false, &cancelled),
            Err(Error::SignIn)
        ));
        credentials::Store::lock(home.path(), &cancelled)
            .unwrap()
            .save(&credentials::Credentials {
                client: "fixture-client".into(),
                subject: "fixture-subject".into(),
                access: "fixture-access".into(),
                refresh: "fixture-refresh".into(),
                expires_at: credentials::now().unwrap() + 3600,
            })
            .unwrap();
        assert_eq!(
            access_token(&agent, home.path(), false, &cancelled).unwrap(),
            "fixture-access"
        );
        assert_eq!(
            access_token(&agent, home.path(), true, &cancelled).unwrap(),
            "fixture-access"
        );
    }

    fn response(status: u16, format: &str, body: &str) -> ureq::http::Response<ureq::Body> {
        ureq::http::Response::builder()
            .status(status)
            .header("content-type", format)
            .header("x-request-id", "req_0123abcd")
            .body(ureq::Body::builder().data(body.as_bytes()))
            .unwrap()
    }

    #[test]
    fn recognizes_completed_replies_without_content_type() {
        let cancelled = AtomicBool::new(false);
        let read = |body: &str| {
            let mut reply = response(200, "application/json", body);
            reply.headers_mut().remove("content-type");
            read_answer(reply, &cancelled)
        };
        let stream = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
            "data: {\"type\":\"response.completed\"}\n\n"
        );
        assert_eq!(read(stream).unwrap(), "hello");
        let json = r#"{"object":"response","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}]}"#;
        assert_eq!(read(json).unwrap(), "hello");
        assert!(read("").unwrap_err().to_string().contains("empty response"));
        assert!(read("<html>not an answer</html>").is_err());
        assert!(
            read(
                stream
                    .split("data: {\"type\":\"response.completed")
                    .next()
                    .unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn settings_select_model_and_pass_reasoning_only_when_configured() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(".kwsh/chatgpt.json");
        assert!(
            Settings::load(home.path())
                .unwrap()
                .reasoning_effort
                .is_none()
        );
        std::fs::create_dir(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"model":"chosen-model","reasoning_effort":"high"}"#,
        )
        .unwrap();
        let settings = Settings::load(home.path()).unwrap();
        let body = request_body(
            settings.model.as_deref().unwrap(),
            settings.reasoning_effort.as_deref(),
            "hello",
        );
        assert_eq!(body["model"], "chosen-model");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert!(
            request_body("chosen-model", None, "hello")
                .get("reasoning")
                .is_none()
        );
        std::fs::write(path, r#"{"reasoning_effort":"typo"}"#).unwrap();
        assert!(Settings::load(home.path()).is_err());
    }

    #[test]
    fn accepts_completed_json_but_does_not_publish_incomplete_output() {
        let mut body = json!({
            "object": "response", "status": "completed",
            "output": [{"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "hello"}
            ]}]
        });
        let cancelled = AtomicBool::new(false);
        let read = |body: &Value| {
            read_answer(
                response(200, "application/json; charset=utf-8", &body.to_string()),
                &cancelled,
            )
        };
        assert_eq!(read(&body).unwrap(), "hello");
        body["status"] = json!("incomplete");
        let error = read(&body).unwrap_err().to_string();
        assert!(error.contains("HTTP 200, JSON"));
        assert!(!error.contains("hello"));
    }

    #[test]
    fn reports_http_metadata_without_dumping_headers_or_error_bodies() {
        let cancelled = AtomicBool::new(false);
        let redirect = response(302, "text/html", "opaque page contents");
        let error = read_answer(redirect, &cancelled).unwrap_err().to_string();
        assert_eq!(
            error,
            "ChatGPT request failed (HTTP 302, HTML, request ID req_0123abcd)"
        );

        let mut denied = response(
            403,
            "application/json",
            r#"{"error":{"code":"subscription_sharing_user_not_eligible","message":"sensitive echo"}}"#,
        );
        denied
            .headers_mut()
            .insert("x-request-id", "unexpected-secret".parse().unwrap());
        let error = read_answer(denied, &cancelled).unwrap_err().to_string();
        assert_eq!(
            error,
            "this ChatGPT account or workspace is not eligible for plan usage (HTTP 403, JSON)"
        );
    }

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
