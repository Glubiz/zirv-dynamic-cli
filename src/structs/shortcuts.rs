use serde::Deserialize;
use std::collections::HashMap;

/// Structure representing the shortcuts mapping file.
#[derive(Debug, Deserialize)]
pub struct Shortcuts {
    /// A mapping of shortcut keys to script file names.
    pub shortcuts: HashMap<String, String>,
}
