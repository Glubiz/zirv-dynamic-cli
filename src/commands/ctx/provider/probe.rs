use std::time::Duration;

use super::credential::Credential;
use super::{AuthScheme, Protocol, ProviderSpec};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeResult {
    Unreachable(String),
    Http { status: u16, model_ids: Vec<String> },
}

pub trait Probe {
    fn models(
        &self,
        endpoint_url: &str,
        spec: &ProviderSpec,
        credential: Option<&Credential>,
    ) -> ProbeResult;
}

pub struct HttpProbe {
    agent: ureq::Agent,
}

impl Default for HttpProbe {
    fn default() -> Self {
        Self {
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(10)))
                .timeout_connect(Some(Duration::from_secs(10)))
                .build()
                .into(),
        }
    }
}

impl Probe for HttpProbe {
    fn models(
        &self,
        endpoint_url: &str,
        spec: &ProviderSpec,
        credential: Option<&Credential>,
    ) -> ProbeResult {
        let Some(path) = spec.models_list_path else {
            return ProbeResult::Unreachable("provider has no models-list endpoint".into());
        };
        let url = format!("{}{}", endpoint_url.trim_end_matches('/'), path);
        let mut request = self.agent.get(&url);
        if let Some(credential) = credential {
            match spec.auth {
                AuthScheme::Header {
                    name,
                    scheme,
                    extra_headers,
                } => {
                    let value = scheme.map_or_else(
                        || credential.secret.expose().to_string(),
                        |scheme| format!("{scheme} {}", credential.secret.expose()),
                    );
                    request = request.header(name, value);
                    for (name, value) in extra_headers {
                        request = request.header(*name, *value);
                    }
                }
                AuthScheme::Query { name } => {
                    request = request.query(name, credential.secret.expose());
                }
            }
        } else if let AuthScheme::Header { extra_headers, .. } = spec.auth {
            for (name, value) in extra_headers {
                request = request.header(*name, *value);
            }
        }
        match request.call() {
            Ok(mut response) => {
                let status = response.status().as_u16();
                if status != 200 {
                    return ProbeResult::Http {
                        status,
                        model_ids: Vec::new(),
                    };
                }
                let body = match response.body_mut().read_to_string() {
                    Ok(body) => body,
                    Err(error) => return ProbeResult::Unreachable(error.to_string()),
                };
                ProbeResult::Http {
                    status,
                    model_ids: parse_model_ids(spec.protocol, &body),
                }
            }
            Err(ureq::Error::StatusCode(status)) => ProbeResult::Http {
                status,
                model_ids: Vec::new(),
            },
            Err(error) => ProbeResult::Unreachable(error.to_string()),
        }
    }
}

fn parse_model_ids(protocol: Protocol, body: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let key = if protocol == Protocol::GoogleGenerativeAi {
        "models"
    } else {
        "data"
    };
    value
        .get(key)
        .and_then(|models| models.as_array())
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let field = if protocol == Protocol::GoogleGenerativeAi {
                "name"
            } else {
                "id"
            };
            model
                .get(field)
                .and_then(|id| id.as_str())
                .map(|id| id.strip_prefix("models/").unwrap_or(id).to_string())
        })
        .collect()
}

#[cfg(test)]
#[derive(Clone)]
pub struct FakeProbe {
    result: ProbeResult,
}

#[cfg(test)]
impl FakeProbe {
    pub fn new(result: ProbeResult) -> Self {
        Self { result }
    }
}

#[cfg(test)]
impl Probe for FakeProbe {
    fn models(&self, _: &str, _: &ProviderSpec, _: Option<&Credential>) -> ProbeResult {
        self.result.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_list_parsers_keep_only_ids() {
        let openai = parse_model_ids(
            Protocol::OpenAiResponses,
            r#"{"data":[{"id":"gpt-5"},{"name":"ignored"}],"secret":"no"}"#,
        );
        assert_eq!(openai, ["gpt-5"]);
        let google = parse_model_ids(
            Protocol::GoogleGenerativeAi,
            r#"{"models":[{"name":"models/gemini-3"}]}"#,
        );
        assert_eq!(google, ["gemini-3"]);
    }
}
