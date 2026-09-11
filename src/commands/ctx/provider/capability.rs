use serde::{Deserialize, Serialize};

use super::{ModelId, Protocol};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Capability {
    #[serde(with = "unknown")]
    Unknown,
    Declared {
        declared: bool,
    },
    Verified {
        verified: bool,
    },
}

mod unknown {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str("unknown")
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<(), D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value == "unknown" {
            Ok(())
        } else {
            Err(serde::de::Error::custom("expected \"unknown\""))
        }
    }
}

impl Capability {
    pub const fn declared(value: bool) -> Self {
        Self::Declared { declared: value }
    }

    #[cfg(test)]
    pub fn is_verified(self) -> bool {
        matches!(self, Self::Verified { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelCapabilities {
    pub tools: Capability,
    pub streaming: Capability,
    pub vision: Capability,
    pub structured_output: Capability,
    pub prompt_caching: Capability,
    pub reasoning_controls: Capability,
    pub continuation: Capability,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub source: &'static str,
}

const SOURCE: &str =
    "declared from vendor docs, 2026-09; unverified until N07/N08 record a live validation";

/// Declared provider documentation only. N02 never returns `Verified`;
/// N07/N08 may upgrade individual fields after a live validation.
pub fn declared(protocol: Protocol, model: &ModelId) -> ModelCapabilities {
    let mut capabilities = ModelCapabilities {
        tools: Capability::Unknown,
        streaming: Capability::Unknown,
        vision: Capability::Unknown,
        structured_output: Capability::Unknown,
        prompt_caching: Capability::Unknown,
        reasoning_controls: Capability::Unknown,
        continuation: Capability::Unknown,
        context_window: crate::commands::ctx::catalogue::vendor(&model.vendor).and_then(|vendor| {
            crate::commands::ctx::catalogue::context_window(vendor, Some(&model.id))
        }),
        max_output_tokens: None,
        source: SOURCE,
    };
    let id = model.id.to_ascii_lowercase();
    match protocol {
        Protocol::AnthropicMessages if id.starts_with("claude-") => {
            capabilities.tools = Capability::declared(true);
            capabilities.streaming = Capability::declared(true);
            capabilities.vision = Capability::declared(true);
            capabilities.structured_output = Capability::declared(true);
            capabilities.prompt_caching = Capability::declared(true);
            capabilities.reasoning_controls = Capability::declared(true);
            capabilities.continuation = Capability::declared(false);
        }
        Protocol::OpenAiResponses if id.starts_with("gpt-") || id.starts_with('o') => {
            capabilities.tools = Capability::declared(true);
            capabilities.streaming = Capability::declared(true);
            capabilities.vision = Capability::declared(true);
            capabilities.structured_output = Capability::declared(true);
            capabilities.prompt_caching = Capability::declared(true);
            capabilities.reasoning_controls = Capability::declared(true);
            capabilities.continuation = Capability::declared(true);
        }
        Protocol::GoogleGenerativeAi if id.starts_with("gemini-") => {
            capabilities.tools = Capability::declared(true);
            capabilities.streaming = Capability::declared(true);
            capabilities.vision = Capability::declared(true);
            capabilities.structured_output = Capability::declared(true);
        }
        Protocol::OpenAiChatCompatible => {
            capabilities.streaming = Capability::declared(true);
        }
        _ => {}
    }
    capabilities
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_table_never_claims_live_verification() {
        for protocol in [
            Protocol::AnthropicMessages,
            Protocol::OpenAiResponses,
            Protocol::OpenAiChatCompatible,
            Protocol::GoogleGenerativeAi,
            Protocol::GoogleVertex,
            Protocol::AwsBedrock,
        ] {
            let caps = declared(
                protocol,
                &ModelId {
                    vendor: "anthropic".into(),
                    id: "claude-sonnet-5".into(),
                },
            );
            assert!(
                [
                    caps.tools,
                    caps.streaming,
                    caps.vision,
                    caps.structured_output,
                    caps.prompt_caching,
                    caps.reasoning_controls,
                    caps.continuation,
                ]
                .into_iter()
                .all(|capability| !capability.is_verified())
            );
        }
    }

    #[test]
    fn capability_json_uses_the_tri_state_contract() {
        assert_eq!(
            serde_json::to_string(&Capability::Unknown).unwrap(),
            "\"unknown\""
        );
        assert_eq!(
            serde_json::to_string(&Capability::declared(true)).unwrap(),
            r#"{"declared":true}"#
        );
    }
}
