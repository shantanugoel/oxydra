use std::{collections::BTreeMap, time::Duration};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::ToolError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafetyTier {
    ReadOnly,
    SideEffecting,
    Privileged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JsonSchemaType {
    Object,
    String,
    Integer,
    Number,
    Boolean,
    Array,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonSchema {
    #[serde(rename = "type")]
    pub schema_type: JsonSchemaType,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, JsonSchema>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "additionalProperties"
    )]
    pub additional_properties: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<JsonSchema>>,
}

impl JsonSchema {
    pub fn new(schema_type: JsonSchemaType) -> Self {
        Self {
            schema_type,
            properties: BTreeMap::new(),
            required: Vec::new(),
            additional_properties: None,
            description: None,
            items: None,
        }
    }

    pub fn object(properties: BTreeMap<String, JsonSchema>, required: Vec<String>) -> Self {
        Self {
            schema_type: JsonSchemaType::Object,
            properties,
            required,
            additional_properties: Some(false),
            description: None,
            items: None,
        }
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn with_items(mut self, items: JsonSchema) -> Self {
        self.items = Some(Box::new(items));
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionDecl {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: JsonSchema,
}

impl FunctionDecl {
    pub fn new(
        name: impl Into<String>,
        description: Option<String>,
        parameters: JsonSchema,
    ) -> Self {
        Self {
            name: name.into(),
            description,
            parameters,
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn schema(&self) -> FunctionDecl;

    async fn execute(&self, args: &str) -> Result<String, ToolError>;

    fn timeout(&self) -> Duration;

    fn safety_tier(&self) -> SafetyTier;
}
