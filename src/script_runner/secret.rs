use serde::{Deserialize, Serialize};

/// Represents a secret definition in the script.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Secret {
    /// The placeholder name to be substituted (e.g. "commit_password").
    pub name: String,
    /// The environment variable name where the secret value is stored (e.g. "COMMIT_PASSWORD").
    pub env_var: String,
}
