//! Minimal blocking HTTP seam. Production uses ureq; tests plug in a fake.

use std::time::Duration;

use crate::{Error, Result, USER_AGENT};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Post,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RequestBody {
    None,
    Json(Vec<u8>),
    Form(Vec<(String, String)>),
}

/// A request. `Debug` is deliberately not derived: headers carry bearer tokens
/// and form bodies carry OAuth codes and refresh tokens.
#[derive(Clone)]
pub(crate) struct HttpRequest {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: RequestBody,
    pub timeout: Duration,
    pub max_response_bytes: u64,
}

impl HttpRequest {
    pub fn new(method: Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: RequestBody::None,
            timeout: Duration::from_secs(35),
            max_response_bytes: 32 * 1024 * 1024,
        }
    }

    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    pub fn json(mut self, body: &serde_json::Value) -> Self {
        self.body = RequestBody::Json(serde_json::to_vec(body).expect("serializing a Value"));
        self
    }

    pub fn form(mut self, fields: &[(&str, &str)]) -> Self {
        self.body = RequestBody::Form(
            fields
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        );
        self
    }

    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    pub fn max_response_bytes(mut self, n: u64) -> Self {
        self.max_response_bytes = n;
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HttpResponse {
    pub status: u16,
    /// Header names are lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub(crate) trait HttpTransport: Send + Sync + 'static {
    /// Sends the request. Non-2xx statuses are returned as `Ok`; only transport
    /// failures are `Err`.
    fn send(&self, req: HttpRequest) -> Result<HttpResponse>;
}

pub(crate) struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .https_only(true)
            .user_agent(USER_AGENT)
            .timeout_connect(Some(Duration::from_secs(10)))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl HttpTransport for UreqTransport {
    fn send(&self, req: HttpRequest) -> Result<HttpResponse> {
        let result = match req.method {
            Method::Get => {
                let mut b = self.agent.get(&req.url);
                for (k, v) in &req.headers {
                    b = b.header(*k, v.as_str());
                }
                b.config().timeout_global(Some(req.timeout)).build().call()
            }
            Method::Post => {
                let mut b = self.agent.post(&req.url);
                for (k, v) in &req.headers {
                    b = b.header(*k, v.as_str());
                }
                let b = b.config().timeout_global(Some(req.timeout)).build();
                match &req.body {
                    RequestBody::None => b.send_empty(),
                    RequestBody::Json(bytes) => b
                        .header("content-type", "application/json")
                        .send(bytes.as_slice()),
                    RequestBody::Form(fields) => {
                        b.send_form(fields.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                    }
                }
            }
        };
        let mut resp = result.map_err(map_ureq_error)?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|v| (k.as_str().to_ascii_lowercase(), v.to_owned()))
            })
            .collect();
        let body = resp
            .body_mut()
            .with_config()
            .limit(req.max_response_bytes)
            .read_to_vec()
            .map_err(map_ureq_error)?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

fn map_ureq_error(e: ureq::Error) -> Error {
    match e {
        ureq::Error::Timeout(_) => Error::Timeout,
        ureq::Error::BodyExceedsLimit(n) => {
            Error::Network(format!("response larger than {n} bytes"))
        }
        // ureq error messages contain URLs at most, never headers.
        other => Error::Network(other.to_string()),
    }
}

#[cfg(test)]
pub(crate) mod fake {
    //! Scripted transport for tests.

    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    /// A recorded request (owned copy, bodies decoded for assertions).
    #[derive(Debug, Clone)]
    pub struct Recorded {
        pub method: Method,
        pub url: String,
        pub headers: Vec<(&'static str, String)>,
        pub json: Option<serde_json::Value>,
        pub form: Option<Vec<(String, String)>>,
    }

    impl Recorded {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
        pub fn form_value(&self, name: &str) -> Option<&str> {
            self.form
                .as_ref()?
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        }
    }

    #[derive(Default)]
    pub struct FakeTransport {
        responses: Mutex<VecDeque<Result<HttpResponse>>>,
        pub requests: Mutex<Vec<Recorded>>,
    }

    impl FakeTransport {
        pub fn new() -> Self {
            Self::default()
        }
        pub fn push(&self, status: u16, body: &str) -> &Self {
            self.push_with_headers(status, &[], body)
        }
        pub fn push_with_headers(
            &self,
            status: u16,
            headers: &[(&str, &str)],
            body: &str,
        ) -> &Self {
            self.responses.lock().unwrap().push_back(Ok(HttpResponse {
                status,
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.to_ascii_lowercase(), (*v).to_owned()))
                    .collect(),
                body: body.as_bytes().to_vec(),
            }));
            self
        }
        pub fn push_bytes(&self, status: u16, body: &[u8]) -> &Self {
            self.responses.lock().unwrap().push_back(Ok(HttpResponse {
                status,
                headers: Vec::new(),
                body: body.to_vec(),
            }));
            self
        }
        pub fn push_err(&self, e: Error) -> &Self {
            self.responses.lock().unwrap().push_back(Err(e));
            self
        }
        pub fn requests(&self) -> Vec<Recorded> {
            self.requests.lock().unwrap().clone()
        }
        pub fn remaining(&self) -> usize {
            self.responses.lock().unwrap().len()
        }
    }

    impl HttpTransport for FakeTransport {
        fn send(&self, req: HttpRequest) -> Result<HttpResponse> {
            let (json, form) = match &req.body {
                RequestBody::None => (None, None),
                RequestBody::Json(b) => (Some(serde_json::from_slice(b).unwrap()), None),
                RequestBody::Form(f) => (None, Some(f.clone())),
            };
            self.requests.lock().unwrap().push(Recorded {
                method: req.method,
                url: req.url.clone(),
                headers: req.headers.clone(),
                json,
                form,
            });
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| panic!("FakeTransport: no response scripted for {}", req.url))
        }
    }

    impl HttpTransport for std::sync::Arc<FakeTransport> {
        fn send(&self, req: HttpRequest) -> Result<HttpResponse> {
            (**self).send(req)
        }
    }
}
