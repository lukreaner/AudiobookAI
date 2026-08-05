use std::{collections::BTreeMap, fmt, pin::Pin, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use url::Url;

use crate::{ProviderError, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Delete,
}

#[derive(Clone)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub url: Url,
    pub headers: BTreeMap<String, String>,
    pub body: Bytes,
    pub timeout: Duration,
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url_scheme", &self.url.scheme())
            .field("url_has_host", &self.url.host_str().is_some())
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("body_len", &self.body.len())
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl HttpRequest {
    pub fn json(method: HttpMethod, url: Url, value: &serde_json::Value) -> Result<Self> {
        let body = serde_json::to_vec(value)
            .map_err(|error| ProviderError::Configuration(error.to_string()))?;
        Ok(Self {
            method,
            url,
            headers: BTreeMap::from([("content-type".to_owned(), "application/json".to_owned())]),
            body: body.into(),
            timeout: Duration::from_secs(120),
        })
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }
}

#[derive(Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Bytes,
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("body_len", &self.body.len())
            .finish()
    }
}

pub type HttpByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + 'static>>;

pub struct HttpStreamResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: HttpByteStream,
}

impl fmt::Debug for HttpStreamResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpStreamResponse")
            .field("status", &self.status)
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("body", &"<byte stream>")
            .finish()
    }
}

impl HttpResponse {
    pub fn require_success(self) -> Result<Self> {
        if (200..300).contains(&self.status) {
            Ok(self)
        } else {
            Err(ProviderError::from_status(self.status, &self.body))
        }
    }
}

#[async_trait]
pub trait HttpTransport: fmt::Debug + Send + Sync {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse>;

    async fn execute_stream(&self, request: HttpRequest) -> Result<HttpStreamResponse> {
        let response = self.execute(request).await?.require_success()?;
        Ok(HttpStreamResponse {
            status: response.status,
            headers: response.headers,
            body: Box::pin(futures::stream::once(async move { Ok(response.body) })),
        })
    }
}

#[derive(Clone, Debug)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            // Provider credentials may only travel to the exact configured
            // endpoint. Never inherit HTTP(S)_PROXY or platform proxy settings;
            // explicit proxy support would require its own trusted-secret and
            // consent boundary.
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| {
                ProviderError::Transport("HTTP client initialization failed".to_owned())
            })?;
        Ok(Self { client })
    }
}

#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
        let method = match request.method {
            HttpMethod::Get => reqwest::Method::GET,
            HttpMethod::Post => reqwest::Method::POST,
            HttpMethod::Put => reqwest::Method::PUT,
            HttpMethod::Delete => reqwest::Method::DELETE,
        };
        let mut builder = self
            .client
            .request(method, request.url)
            .timeout(request.timeout)
            .body(request.body);
        for (name, value) in request.headers {
            builder = builder.header(name, value);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| safe_request_error(&error))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect();
        let body = response.bytes().await.map_err(|_| {
            ProviderError::Transport("provider response could not be read".to_owned())
        })?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }

    async fn execute_stream(&self, request: HttpRequest) -> Result<HttpStreamResponse> {
        let method = match request.method {
            HttpMethod::Get => reqwest::Method::GET,
            HttpMethod::Post => reqwest::Method::POST,
            HttpMethod::Put => reqwest::Method::PUT,
            HttpMethod::Delete => reqwest::Method::DELETE,
        };
        let mut builder = self
            .client
            .request(method, request.url)
            .timeout(request.timeout)
            .body(request.body);
        for (name, value) in request.headers {
            builder = builder.header(name, value);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| safe_request_error(&error))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect();
        if !(200..300).contains(&status) {
            let body = response.bytes().await.map_err(|_| {
                ProviderError::Transport("provider response could not be read".to_owned())
            })?;
            return Err(ProviderError::from_status(status, &body));
        }
        let body = response.bytes_stream().map(|chunk| {
            chunk.map_err(|_| {
                ProviderError::Transport("provider audio stream was interrupted".to_owned())
            })
        });
        Ok(HttpStreamResponse {
            status,
            headers,
            body: Box::pin(body),
        })
    }
}

fn safe_request_error(error: &reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        ProviderError::UncertainCharge
    } else if error.is_connect() {
        ProviderError::Transport("provider connection failed".to_owned())
    } else {
        ProviderError::Transport("provider request failed".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_debug_output_never_contains_payloads_or_header_values() {
        let endpoint_path = ["private", "endpoint", "value"].join("-");
        let request_header = ["request", "header", "value"].join("-");
        let request_body = ["request", "body", "value"].join("-");
        let request = HttpRequest {
            method: HttpMethod::Post,
            url: Url::parse(&format!("https://provider.invalid/{endpoint_path}"))
                .expect("test URL"),
            headers: BTreeMap::from([("authorization".to_owned(), request_header.clone())]),
            body: Bytes::from(request_body.clone()),
            timeout: Duration::from_secs(1),
        };
        let debug = format!("{request:?}");
        assert!(!debug.contains(&endpoint_path));
        assert!(!debug.contains(&request_header));
        assert!(!debug.contains(&request_body));

        let response_header = ["response", "header", "value"].join("-");
        let response_body = ["response", "body", "value"].join("-");
        let response = HttpResponse {
            status: 200,
            headers: BTreeMap::from([("set-cookie".to_owned(), response_header.clone())]),
            body: Bytes::from(response_body.clone()),
        };
        let debug = format!("{response:?}");
        assert!(!debug.contains(&response_header));
        assert!(!debug.contains(&response_body));
    }
}
