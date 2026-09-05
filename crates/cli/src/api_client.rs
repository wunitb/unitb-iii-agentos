use std::collections::BTreeMap;

use anyhow::{Context, Result};
use reqwest::{Method, RequestBuilder, Url};

use crate::{API_BASE, bootstrap, runtime_paths};

pub(crate) struct AgentosApiClient {
    client: reqwest::Client,
    base: Url,
    bearer: Option<String>,
}

impl AgentosApiClient {
    pub(crate) fn from_runtime() -> Result<Self> {
        let runtime_dir = runtime_paths()?.runtime_dir;
        let dotenv = bootstrap::load_dotenv(&runtime_dir)?;
        let shell = crate::unicode_environment(std::env::vars_os());
        let (base, bearer) = resolve_api_settings(&dotenv, &shell);
        Self::new(&base, bearer)
    }

    pub(crate) fn new(base: &str, bearer: Option<String>) -> Result<Self> {
        let base = validate_base(base)?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("Cannot build AgentOS API client")?;
        Ok(Self {
            client,
            base,
            bearer: bearer.filter(|key| !key.trim().is_empty()),
        })
    }

    pub(crate) fn base_url(&self) -> String {
        self.base.as_str().trim_end_matches('/').to_string()
    }

    fn api_url(&self, path: &str) -> Result<Url> {
        if !(path == "/api" || path.starts_with("/api/") || path.starts_with("/api?")) {
            anyhow::bail!("AgentOS API path must be relative to /api, got {path:?}");
        }
        let url = self
            .base
            .join(path.trim_start_matches('/'))
            .context("Invalid AgentOS API path")?;
        if url.scheme() != self.base.scheme()
            || url.host_str() != self.base.host_str()
            || url.port_or_known_default() != self.base.port_or_known_default()
        {
            anyhow::bail!("AgentOS API request escaped the configured origin");
        }
        Ok(url)
    }

    fn request(&self, method: Method, path: impl AsRef<str>) -> RequestBuilder {
        // Paths are internal CLI constructions. Keep the fallible validator
        // separately testable while making it impossible for reqwest to receive
        // an unvalidated absolute URL from these call sites.
        let url = self
            .api_url(path.as_ref())
            .expect("internal AgentOS API path must begin with /api");
        let request = self.client.request(method, url);
        match &self.bearer {
            Some(key) => request.bearer_auth(key),
            None => request,
        }
    }

    pub(crate) fn get(&self, path: impl AsRef<str>) -> RequestBuilder {
        self.request(Method::GET, path)
    }

    pub(crate) fn post(&self, path: impl AsRef<str>) -> RequestBuilder {
        self.request(Method::POST, path)
    }

    pub(crate) fn patch(&self, path: impl AsRef<str>) -> RequestBuilder {
        self.request(Method::PATCH, path)
    }

    pub(crate) fn delete(&self, path: impl AsRef<str>) -> RequestBuilder {
        self.request(Method::DELETE, path)
    }
}

fn validate_base(raw: &str) -> Result<Url> {
    let mut url = Url::parse(raw.trim()).context("Invalid AGENTOS_API_URL")?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        anyhow::bail!("AGENTOS_API_URL must be an absolute http(s) URL");
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        anyhow::bail!("AGENTOS_API_URL must not contain credentials, query, or fragment");
    }
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

fn selected_setting(
    name: &str,
    dotenv: &BTreeMap<String, String>,
    shell: &BTreeMap<String, String>,
) -> Option<String> {
    dotenv
        .get(name)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| shell.get(name).filter(|value| !value.trim().is_empty()))
        .cloned()
}

pub(crate) fn selected_api_bearer(
    dotenv: &BTreeMap<String, String>,
    shell: &BTreeMap<String, String>,
) -> Option<String> {
    selected_setting("AGENTOS_API_KEY", dotenv, shell)
}

fn resolve_api_settings(
    dotenv: &BTreeMap<String, String>,
    shell: &BTreeMap<String, String>,
) -> (String, Option<String>) {
    (
        selected_setting("AGENTOS_API_URL", dotenv, shell).unwrap_or_else(|| API_BASE.to_string()),
        selected_api_bearer(dotenv, shell),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;

    #[test]
    fn active_dotenv_wins_and_blank_values_fall_back_to_shell() {
        let shell = BTreeMap::from([
            (
                "AGENTOS_API_URL".to_string(),
                "http://127.0.0.1:3001".to_string(),
            ),
            ("AGENTOS_API_KEY".to_string(), "shell-key".to_string()),
        ]);
        let dotenv = BTreeMap::from([
            (
                "AGENTOS_API_URL".to_string(),
                "http://127.0.0.1:3002".to_string(),
            ),
            ("AGENTOS_API_KEY".to_string(), "file-key".to_string()),
        ]);
        assert_eq!(
            resolve_api_settings(&dotenv, &shell),
            (
                "http://127.0.0.1:3002".to_string(),
                Some("file-key".to_string())
            )
        );
        let blank = BTreeMap::from([
            ("AGENTOS_API_URL".to_string(), "".to_string()),
            ("AGENTOS_API_KEY".to_string(), " ".to_string()),
        ]);
        assert_eq!(
            resolve_api_settings(&blank, &shell),
            (
                "http://127.0.0.1:3001".to_string(),
                Some("shell-key".to_string())
            )
        );
    }

    #[test]
    fn rejects_off_origin_and_non_api_paths_before_reqwest() {
        let client = AgentosApiClient::new("http://127.0.0.1:3111", Some("secret".into())).unwrap();
        for path in [
            "https://evil.example/api",
            "//evil.example/api",
            "/dashboard",
            "api/health",
        ] {
            assert!(client.api_url(path).is_err(), "accepted {path}");
        }
        assert_eq!(
            client.api_url("/api/health").unwrap().as_str(),
            "http://127.0.0.1:3111/api/health"
        );
        for base in [
            "file:///tmp/socket",
            "http://user:pass@127.0.0.1",
            "http://127.0.0.1?q=1",
        ] {
            assert!(
                AgentosApiClient::new(base, None).is_err(),
                "accepted {base}"
            );
        }
    }

    fn serve_requests(count: usize) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for _ in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut buffer = [0_u8; 8192];
                let size = stream.read(&mut buffer).unwrap();
                requests.push(String::from_utf8_lossy(&buffer[..size]).to_string());
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .unwrap();
            }
            requests
        });
        (address, handle)
    }

    #[tokio::test]
    async fn every_http_verb_attaches_the_selected_bearer() {
        let (base, server) = serve_requests(4);
        let client = AgentosApiClient::new(&base, Some("selected-key".into())).unwrap();
        client.get("/api/get").send().await.unwrap();
        client
            .post("/api/post")
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        client
            .patch("/api/patch")
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        client.delete("/api/delete").send().await.unwrap();
        let requests = server.join().unwrap();
        for (request, method) in requests.iter().zip(["GET", "POST", "PATCH", "DELETE"]) {
            assert!(request.starts_with(method), "{request}");
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer selected-key\r\n"),
                "{request}"
            );
        }
    }

    #[tokio::test]
    async fn redirects_are_not_followed_or_forwarded() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let reached = Arc::new(AtomicBool::new(false));
        let reached_thread = Arc::clone(&reached);
        let target_address = target.local_addr().unwrap();
        let target_thread = std::thread::spawn(move || {
            for _ in 0..20 {
                if target.accept().is_ok() {
                    reached_thread.store(true, Ordering::SeqCst);
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let redirect = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", redirect.local_addr().unwrap());
        let redirect_thread = std::thread::spawn(move || {
            let (mut stream, _) = redirect.accept().unwrap();
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer).unwrap();
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://{target_address}/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).unwrap();
        });
        let client = AgentosApiClient::new(&base, Some("never-forward".into())).unwrap();
        let response = client.get("/api/redirect").send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        redirect_thread.join().unwrap();
        target_thread.join().unwrap();
        assert!(!reached.load(Ordering::SeqCst));
    }
}
