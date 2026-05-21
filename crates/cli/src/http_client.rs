use open_sandbox_agent::tunnel::{ForwardRequest, ForwardResponse, HttpClient};
use open_sandbox_contracts::error::AgentError;

pub struct ReqwestHttpClient {
    client: reqwest::Client,
}

impl ReqwestHttpClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl HttpClient for ReqwestHttpClient {
    async fn send(
        &self,
        port: u16,
        request: ForwardRequest,
    ) -> Result<ForwardResponse, AgentError> {
        let url = format!("http://127.0.0.1:{port}{}", request.uri);

        let method: reqwest::Method = request
            .method
            .parse()
            .map_err(|e: http::method::InvalidMethod| AgentError::Internal {
                detail: e.to_string(),
            })?;

        let mut builder = self.client.request(method, &url);
        for (key, value) in &request.headers {
            builder = builder.header(key.as_str(), value.as_str());
        }
        builder = builder.body(request.body);

        let response = builder.send().await.map_err(|e| AgentError::Internal {
            detail: e.to_string(),
        })?;

        let status_code = response.status().as_u16() as u32;
        let headers = response
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let body = response
            .bytes()
            .await
            .map_err(|e| AgentError::Internal {
                detail: e.to_string(),
            })?
            .to_vec();

        Ok(ForwardResponse {
            status_code,
            headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::net::TcpListener;

    async fn spawn_test_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    let body = "hello from sandbox";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nX-Test: sandbox\r\n\r\n{}",
                        body.len(),
                        body,
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });

        port
    }

    #[tokio::test]
    async fn send_forwards_request_and_returns_response() {
        let port = spawn_test_server().await;
        let client = ReqwestHttpClient::new();

        let response = client
            .send(
                port,
                ForwardRequest {
                    method: "GET".into(),
                    uri: "/".into(),
                    headers: HashMap::new(),
                    body: vec![],
                },
            )
            .await
            .unwrap();

        assert_eq!(response.status_code, 200);
        assert_eq!(response.body, b"hello from sandbox");
    }

    #[tokio::test]
    async fn send_forwards_post_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).await.unwrap();
            let request_text = String::from_utf8_lossy(&buf[..n]);
            let received_body = request_text.split("\r\n\r\n").nth(1).unwrap_or("");
            let body = format!("echoed: {}", received_body);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body,
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });

        let client = ReqwestHttpClient::new();
        let response = client
            .send(
                port,
                ForwardRequest {
                    method: "POST".into(),
                    uri: "/data".into(),
                    headers: HashMap::from([("content-type".into(), "text/plain".into())]),
                    body: b"payload".to_vec(),
                },
            )
            .await
            .unwrap();

        assert_eq!(response.status_code, 200);
        assert!(String::from_utf8_lossy(&response.body).contains("payload"));
    }

    #[tokio::test]
    async fn send_returns_error_for_unreachable_port() {
        let client = ReqwestHttpClient::new();
        let result = client
            .send(
                1,
                ForwardRequest {
                    method: "GET".into(),
                    uri: "/".into(),
                    headers: HashMap::new(),
                    body: vec![],
                },
            )
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn send_preserves_response_headers() {
        let port = spawn_test_server().await;
        let client = ReqwestHttpClient::new();

        let response = client
            .send(
                port,
                ForwardRequest {
                    method: "GET".into(),
                    uri: "/".into(),
                    headers: HashMap::new(),
                    body: vec![],
                },
            )
            .await
            .unwrap();

        assert_eq!(
            response.headers.get("x-test").map(|s| s.as_str()),
            Some("sandbox")
        );
    }
}
