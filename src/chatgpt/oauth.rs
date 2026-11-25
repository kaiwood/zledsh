use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use ring::{digest, rand};
use serde_json::Value;

use super::{
    Error, Result,
    credentials::{self, Credentials},
};

const ISSUER: &str = "https://auth.openai.com";
const RESOURCE: &str = "https://api.openai.com/v1";
const CALLBACK: &str = "/auth/callback";
const TEMP_CLIENT: &str = "dynamic_agent_client";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";

pub(super) fn authorize(
    agent: &ureq::Agent,
    home: &Path,
    cancelled: &AtomicBool,
    previous: Option<&Credentials>,
) -> Result<Credentials> {
    let host = host_id(home)
        .map_err(|_| Error::Message("could not read or create the ChatGPT host ID"))?;
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|_| Error::Message("could not start the sign-in callback"))?;
    listener
        .set_nonblocking(true)
        .map_err(|_| Error::Message("could not configure the sign-in callback"))?;
    let address = listener
        .local_addr()
        .map_err(|_| Error::Message("could not read the callback address"))?
        .to_string();
    let redirect = format!("http://{address}{CALLBACK}");
    let state = secret()?;
    let nonce = secret()?;
    let verifier = secret()?;
    let challenge = URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()));
    let mut fields = vec![
        (
            "client_id",
            previous.map_or(TEMP_CLIENT, |saved| saved.client.as_str()),
        ),
        ("ext_agent_host_id", &host),
        ("response_type", "code"),
        ("redirect_uri", &redirect),
        ("scope", SCOPES),
        ("resource", RESOURCE),
        ("state", &state),
        ("nonce", &nonce),
        ("code_challenge_method", "S256"),
        ("code_challenge", &challenge),
    ];
    if previous.is_none() {
        fields.push(("agent_name_hint", "kwsh"));
    }
    let query = form(&fields);
    let url = format!("{ISSUER}/api/accounts/authorize?{query}");
    check_cancelled(cancelled)?;
    let status = Command::new("/usr/bin/open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| Error::Message("could not open the browser"))?;
    if !status.success() {
        return Err(Error::Message("could not open the browser"));
    }
    let grant = await_callback(
        listener,
        &address,
        &state,
        cancelled,
        previous.map(|saved| saved.client.as_str()),
    )?;
    check_cancelled(cancelled)?;
    let body = form(&[
        ("grant_type", "authorization_code"),
        ("client_id", &grant.client),
        ("code", &grant.code),
        ("code_verifier", &verifier),
        ("redirect_uri", &redirect),
        ("resource", RESOURCE),
    ]);
    let reply = agent
        .post("https://auth.openai.com/api/accounts/oauth/token")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .send(body)
        .map_err(|_| {
            Error::Message("OpenAI rejected the authorization code or the token request failed")
        })?;
    let reply = super::read_json(reply)?;
    let (_, id) = plan_tokens(&reply)?;
    check_cancelled(cancelled)?;
    let keys = agent
        .get("https://auth.openai.com/.well-known/jwks.json")
        .call()
        .map_err(|_| Error::Message("could not fetch OpenAI signing keys"))?;
    let subject = verify_identity(id, &super::read_json(keys)?, &grant.client, &nonce)?;
    if previous.is_some_and(|saved| saved.subject != subject) {
        return Err(Error::Message(
            "ChatGPT returned a different account for this registration",
        ));
    }
    credentials_from(&reply, grant.client, subject)
}

pub(super) fn refresh(agent: &ureq::Agent, saved: &Credentials) -> Result<Credentials> {
    let body = form(&[
        ("grant_type", "refresh_token"),
        ("client_id", &saved.client),
        ("refresh_token", &saved.refresh),
        ("resource", RESOURCE),
    ]);
    let mut reply = agent
        .post("https://auth.openai.com/api/accounts/oauth/token")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .config()
        .http_status_as_error(false)
        .build()
        .send(body)
        .map_err(|_| Error::Message("ChatGPT token refresh failed; try again later"))?;
    if !reply.status().is_success() {
        if matches!(reply.status().as_u16(), 400 | 401) {
            let error: std::result::Result<Value, _> =
                reply.body_mut().with_config().limit(8192).read_json();
            if error.ok().is_some_and(|value| {
                matches!(
                    value["error"].as_str(),
                    Some(
                        "invalid_grant"
                            | "invalid_refresh_token"
                            | "token_expired"
                            | "refresh_token_expired"
                            | "refresh_token_invalidated"
                            | "refresh_token_reused"
                    )
                )
            }) {
                return Err(Error::SignIn);
            }
        }
        return Err(Error::Message(
            "ChatGPT token refresh failed; saved credentials were not changed",
        ));
    }
    credentials_from(
        &super::read_json(reply)?,
        saved.client.clone(),
        saved.subject.clone(),
    )
}

fn credentials_from(reply: &Value, client: String, subject: String) -> Result<Credentials> {
    check_plan(reply)?;
    let ttl = reply["expires_in"]
        .as_u64()
        .filter(|ttl| *ttl > 0)
        .ok_or(Error::Message("missing ChatGPT token expiry"))?;
    let expires_at = credentials::now()?
        .checked_add(ttl)
        .ok_or(Error::Message("invalid ChatGPT token expiry"))?;
    Ok(Credentials {
        client,
        subject,
        expires_at,
        access: token(reply, "access_token")
            .ok_or(Error::Message("missing OpenAI access token"))?
            .into(),
        refresh: token(reply, "refresh_token")
            .ok_or(Error::Message("missing OpenAI refresh token"))?
            .into(),
    })
}

fn form(fields: &[(&str, &str)]) -> String {
    form_urlencoded::Serializer::new(String::new())
        .extend_pairs(fields)
        .finish()
}

fn secret() -> Result<String> {
    let bytes: [u8; 32] = rand::generate(&rand::SystemRandom::new())
        .map_err(|_| Error::Message("could not obtain secure randomness"))?
        .expose();
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn host_id(home: &Path) -> io::Result<String> {
    let path = home.join(".kwsh-chatgpt-host");
    let read = || {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        let stat = file.metadata()?;
        if !stat.is_file()
            || stat.uid() != unsafe { libc::getuid() }
            || stat.nlink() != 1
            || stat.mode() & 0o7777 != 0o600
        {
            return Err(io::Error::other("unsafe host ID file"));
        }
        let mut id = String::new();
        file.take(64).read_to_string(&mut id)?;
        if !valid_host_id(&id) {
            return Err(io::Error::other("invalid host ID"));
        }
        Ok(id)
    };
    match read() {
        Ok(id) => return Ok(id),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut bytes: [u8; 16] = rand::generate(&rand::SystemRandom::new())
        .map_err(|_| io::Error::other("randomness unavailable"))?
        .expose();
    bytes[6] = (bytes[6] & 15) | 0x40;
    bytes[8] = (bytes[8] & 63) | 0x80;
    let mut id = String::from("urn:uuid:");
    for (i, byte) in bytes.iter().enumerate() {
        use std::fmt::Write as _;
        if matches!(i, 4 | 6 | 8 | 10) {
            id.push('-');
        }
        let _ = write!(id, "{byte:02x}");
    }
    let mut file = tempfile::NamedTempFile::new_in(home)?;
    file.write_all(id.as_bytes())?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(&path) {
        Ok(_) => Ok(id),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => read(),
        Err(error) => Err(error.error),
    }
}

fn valid_host_id(id: &str) -> bool {
    id.strip_prefix("urn:uuid:").is_some_and(|id| {
        id.len() == 36
            && id.bytes().enumerate().all(|(i, byte)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    byte == b'-'
                } else {
                    byte.is_ascii_hexdigit()
                }
            })
    })
}

struct Grant {
    code: String,
    client: String,
}

enum Callback {
    Invalid,
    Denied,
    Accepted(Grant),
}

fn parse_callback(
    request: &[u8],
    address: &str,
    expected_state: &str,
    expected_client: Option<&str>,
) -> Option<Callback> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut parsed = httparse::Request::new(&mut headers);
    if !parsed.parse(request).ok()?.is_complete() || parsed.method != Some("GET") {
        return None;
    }
    let mut hosts = parsed
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("host"));
    if hosts.next()?.value != address.as_bytes() || hosts.next().is_some() {
        return None;
    }
    let (path, query) = parsed.path?.split_once('?')?;
    if path != CALLBACK {
        return None;
    }
    let mut params = std::collections::HashMap::new();
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        if params.insert(key, value).is_some() {
            return Some(Callback::Invalid);
        }
    }
    if params.get("state")?.as_ref() != expected_state {
        return Some(Callback::Invalid);
    }
    if params.contains_key("error") {
        return Some(Callback::Denied);
    }
    let code = params.remove("code")?;
    let supplied = params.remove("client_id");
    if expected_client
        .zip(supplied.as_deref())
        .is_some_and(|(expected, actual)| expected != actual)
    {
        return Some(Callback::Invalid);
    }
    let client = supplied.as_deref().or(expected_client)?;
    if code.is_empty()
        || code.len() > 4096
        || !code.bytes().all(|b| b.is_ascii_graphic())
        || client == TEMP_CLIENT
        || client.is_empty()
        || client.len() > 256
        || !client
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return Some(Callback::Invalid);
    }
    Some(Callback::Accepted(Grant {
        code: code.into_owned(),
        client: client.to_owned(),
    }))
}

fn await_callback(
    listener: TcpListener,
    address: &str,
    state: &str,
    cancelled: &AtomicBool,
    expected_client: Option<&str>,
) -> Result<Grant> {
    let deadline = Instant::now() + Duration::from_secs(180);
    while Instant::now() < deadline {
        check_cancelled(cancelled)?;
        match listener.accept() {
            Ok((mut stream, _)) => {
                let callback = read_callback(&mut stream)
                    .ok()
                    .and_then(|bytes| parse_callback(&bytes, address, state, expected_client));
                let (status, message) = match callback {
                    Some(Callback::Accepted(_)) => (
                        "200 OK",
                        "Authorization received. Return to kwsh to finish signing in.",
                    ),
                    Some(Callback::Denied) => {
                        ("200 OK", "Authorization declined. You may close this tab.")
                    }
                    _ => ("400 Bad Request", "Invalid sign-in callback."),
                };
                let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{message}",
                    message.len()
                );
                match callback {
                    Some(Callback::Accepted(grant)) => return Ok(grant),
                    Some(Callback::Denied) => {
                        return Err(Error::Message("ChatGPT sign-in was declined or failed"));
                    }
                    _ => {}
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                std::thread::sleep(Duration::from_millis(50))
            }
            Err(_) => return Err(Error::Message("sign-in callback failed")),
        }
    }
    Err(Error::Message("ChatGPT sign-in timed out"))
}

fn read_callback(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut request = Vec::new();
    let mut chunk = [0; 1024];
    while request.len() < 8192 && Instant::now() < deadline {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                request.extend_from_slice(&chunk[..n]);
                if request.len() <= 8192 && request.windows(4).any(|b| b == b"\r\n\r\n") {
                    return Ok(request);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("incomplete callback"))
}

fn plan_tokens(reply: &Value) -> Result<(&str, &str)> {
    check_plan(reply)?;
    Ok((
        token(reply, "access_token").ok_or(Error::Message("missing OpenAI access token"))?,
        token(reply, "id_token").ok_or(Error::Message("missing OpenAI ID token"))?,
    ))
}

fn check_plan(reply: &Value) -> Result<()> {
    let scopes = reply["scope"].as_str().unwrap_or_default();
    if !["resource.invoke", "chatgpt.tokens.use.direct"]
        .iter()
        .all(|scope| scopes.split_ascii_whitespace().any(|s| &s == scope))
    {
        return Err(Error::Message("ChatGPT plan permission was not granted"));
    }
    if !reply["token_type"]
        .as_str()
        .is_some_and(|value| value.eq_ignore_ascii_case("Bearer"))
    {
        return Err(Error::Message("unexpected OpenAI token type"));
    }
    Ok(())
}

fn token<'a>(reply: &'a Value, name: &str) -> Option<&'a str> {
    reply[name].as_str().filter(|value| {
        !value.is_empty() && value.len() <= 16384 && value.bytes().all(|b| b.is_ascii_graphic())
    })
}

fn verify_identity(token: &str, keys: &Value, client: &str, nonce: &str) -> Result<String> {
    const INVALID: Error = Error::Message("OpenAI ID token failed validation");
    let header = decode_header(token).map_err(|_| INVALID)?;
    let kid = header
        .kid
        .as_deref()
        .filter(|kid| !kid.is_empty())
        .ok_or(INVALID)?;
    if header.alg != Algorithm::RS256 {
        return Err(INVALID);
    }
    let mut matching = keys["keys"]
        .as_array()
        .ok_or(INVALID)?
        .iter()
        .filter(|key| key["kid"] == kid);
    let jwk = matching.next().ok_or(INVALID)?;
    if matching.next().is_some()
        || jwk["kty"] != "RSA"
        || jwk.get("use").is_some_and(|usage| usage != "sig")
        || jwk.get("alg").is_some_and(|alg| alg != "RS256")
    {
        return Err(INVALID);
    }
    let key = DecodingKey::from_rsa_components(
        jwk["n"].as_str().ok_or(INVALID)?,
        jwk["e"].as_str().ok_or(INVALID)?,
    )
    .map_err(|_| INVALID)?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.leeway = 0;
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[client]);
    let claims = decode::<Value>(token, &key, &validation)
        .map_err(|_| INVALID)?
        .claims;
    if claims["nonce"] != nonce
        || claims["sub"].as_str().is_none_or(str::is_empty)
        || claims.get("azp").is_some_and(|azp| azp != client)
        || claims["aud"]
            .as_array()
            .is_some_and(|aud| aud.len() > 1 && claims["azp"] != client)
    {
        return Err(INVALID);
    }
    Ok(claims["sub"].as_str().ok_or(INVALID)?.to_owned())
}

pub(super) fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        Err(Error::Message("cancelled"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use serde_json::json;

    use super::*;

    #[test]
    fn callback_requires_exact_host_path_and_state() {
        let address = "127.0.0.1:12345";
        let parse = |request: &str| parse_callback(request.as_bytes(), address, "expected", None);
        assert!(
            matches!(parse("GET /auth/callback?state=expected&code=sample&client_id=oaiapp_test HTTP/1.1\r\nHost: 127.0.0.1:12345\r\n\r\n"), Some(Callback::Accepted(Grant { client, code })) if client == "oaiapp_test" && code == "sample")
        );
        assert!(matches!(
            parse(
                "GET /auth/callback?state=other&error=access_denied HTTP/1.1\r\nHost: 127.0.0.1:12345\r\n\r\n"
            ),
            Some(Callback::Invalid)
        ));
        assert!(matches!(
            parse(
                "GET /auth/callback?state=expected&error=access_denied HTTP/1.1\r\nHost: 127.0.0.1:12345\r\n\r\n"
            ),
            Some(Callback::Denied)
        ));
        for request in [
            "GET /other?state=expected&code=sample&client_id=oaiapp_test HTTP/1.1\r\nHost: 127.0.0.1:12345\r\n\r\n",
            "GET /auth/callback?state=expected&code=sample&client_id=oaiapp_test HTTP/1.1\r\nHost: other\r\n\r\n",
            "GET /auth/callback?state=expected&state=expected&code=sample&client_id=oaiapp_test HTTP/1.1\r\nHost: 127.0.0.1:12345\r\n\r\n",
            "GET /auth/callback?state=expected&code=sample&client_id=dynamic_agent_client HTTP/1.1\r\nHost: 127.0.0.1:12345\r\n\r\n",
        ] {
            assert!(!matches!(parse(request), Some(Callback::Accepted(_))));
        }
    }

    #[test]
    fn requires_explicit_plan_grant_in_token_response() {
        let mut response = json!({"token_type":"Bearer", "id_token":"id", "access_token":"access",
            "scope":"openid resource.invoke"});
        assert!(plan_tokens(&response).is_err());
        response["scope"] = json!("openid resource.invoke chatgpt.tokens.use.direct");
        assert_eq!(plan_tokens(&response).unwrap(), ("access", "id"));
        response["access_token"] = json!("bad\r\nheader");
        assert!(plan_tokens(&response).is_err());
    }

    #[test]
    fn verifies_signature_nonce_and_client_audience() {
        // This fixture has a throwaway RSA key and no real identity or credentials.
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/chatgpt_test_identity.json"
        ))
        .unwrap();
        let token = fixture["jwt"].as_str().unwrap();
        let keys = &fixture["jwks"];
        assert!(verify_identity(token, keys, "oaiapp_test", "test-nonce").is_ok());
        assert!(verify_identity(token, keys, "oaiapp_other", "test-nonce").is_err());
        assert!(verify_identity(token, keys, "oaiapp_test", "other-nonce").is_err());
        let (prefix, signature) = token.rsplit_once('.').unwrap();
        let mut signature = URL_SAFE_NO_PAD.decode(signature).unwrap();
        signature[0] ^= 1;
        let forged = format!("{prefix}.{}", URL_SAFE_NO_PAD.encode(signature));
        assert!(verify_identity(&forged, keys, "oaiapp_test", "test-nonce").is_err());
    }

    #[test]
    fn host_identifier_is_stable_and_rejects_unsafe_files() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join(".kwsh-chatgpt-host");
        let first = host_id(home.path()).unwrap();
        assert_eq!(host_id(home.path()).unwrap(), first);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let moved = home.path().join("target");
        fs::rename(&path, &moved).unwrap();
        symlink(&moved, &path).unwrap();
        assert!(host_id(home.path()).is_err());
    }
}
