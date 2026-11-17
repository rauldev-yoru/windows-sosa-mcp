//! The tool contract (`contracts/tools.json`, see contracts/README.md).
//!
//! The JSON file is the single source of truth for names, descriptions,
//! inputSchema, outputSchema and annotations: it is embedded at build time,
//! `tools/list` serves it verbatim and every call is validated against the
//! tool's inputSchema before anything runs.

use std::collections::HashMap;
use std::sync::OnceLock;

use rmcp::model::Tool;
use serde::Deserialize;
use serde_json::Value;

/// Raw embedded contract file.
pub const CONTRACTS_JSON: &str = include_str!("../contracts/tools.json");

#[derive(Deserialize)]
struct ContractFile {
    tools: Vec<Value>,
}

/// Parsed contract with one compiled JSON Schema validator per tool.
pub struct Contract {
    /// Tools in the original order.
    pub tools: Vec<Tool>,
    validators: HashMap<String, jsonschema::Validator>,
}

impl Contract {
    /// Parse a contract document and compile every inputSchema.
    pub fn parse(json: &str) -> Result<Self, String> {
        let file: ContractFile =
            serde_json::from_str(json).map_err(|e| format!("invalid contract JSON: {e}"))?;
        let mut tools = Vec::with_capacity(file.tools.len());
        let mut validators = HashMap::new();
        for raw in file.tools {
            let schema = raw
                .get("inputSchema")
                .cloned()
                .ok_or("tool without inputSchema")?;
            let tool: Tool =
                serde_json::from_value(raw).map_err(|e| format!("invalid tool entry: {e}"))?;
            let validator = jsonschema::validator_for(&schema)
                .map_err(|e| format!("{}: invalid inputSchema: {e}", tool.name))?;
            if validators
                .insert(tool.name.to_string(), validator)
                .is_some()
            {
                return Err(format!("duplicate tool name {}", tool.name));
            }
            tools.push(tool);
        }
        Ok(Self { tools, validators })
    }

    /// The embedded contract, parsed once.
    pub fn embedded() -> Result<&'static Self, String> {
        static CONTRACT: OnceLock<Result<Contract, String>> = OnceLock::new();
        CONTRACT
            .get_or_init(|| Self::parse(CONTRACTS_JSON))
            .as_ref()
            .map_err(Clone::clone)
    }

    /// True when `name` is part of the contract.
    pub fn contains(&self, name: &str) -> bool {
        self.validators.contains_key(name)
    }

    /// Validate `arguments` against the tool's inputSchema. Returns the first
    /// error message.
    pub fn validate(&self, name: &str, arguments: &Value) -> Result<(), String> {
        let validator = self
            .validators
            .get(name)
            .ok_or_else(|| "Unknown tool".to_string())?;
        match validator.iter_errors(arguments).next() {
            None => Ok(()),
            Some(error) => {
                let path = error.instance_path().to_string();
                let mut message = error.to_string();
                if !path.is_empty() {
                    message = format!("{message} (at {path})");
                }
                Err(message.chars().take(2000).collect())
            }
        }
    }
}
