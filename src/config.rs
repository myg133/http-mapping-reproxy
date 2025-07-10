#![allow(dead_code, unused_imports)]
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub dify_url: String,
    pub sso_url: Option<String>,
    pub config_path: String,
    pub use_mode: UseMode,
    pub dify_host: Option<String>,
    pub self_host: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum UseMode {
    Proxy,
    Normal,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PathConfig {
    pub request: RequestMapConfig,
    pub response: ResponseMapConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RequestMapConfig {
    pub target_service: ServiceType,
    pub method_mapping: Option<MethodMapping>,
    pub body_conversion: Option<BodyConversion>,
    pub mix_mappings: Vec<MixMapping>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ResponseMapConfig {
    pub method_mapping: Option<MethodMapping>,
    pub body_conversion: Option<BodyConversion>,
    pub mix_mappings: Vec<MixMapping>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ServiceType {
    Dify,
    SSO,
    Redirect(Option<String>),
    SSE(String),
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MethodMapping {
    GetToPost,
    PostToGet,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MixMapping {
    pub source: MixSource,
    pub target: MixTarget,
    pub action: MixAction,
    #[serde(default)]
    pub transformations: Option<Vec<Transformation>>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Transformation {
    Base64Decode,
    Base64Encode,
    Split { separator: String, index: usize },
    Replace { from: String, to: String },
    Format { format: String },
    Append { value: String },
    Extract { regex: String },
    If,
    Merge,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MixAction {
    Move,
    Copy,
    DeleteSrc,
    AddTarget(String),
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MixSource {
    Header(String),
    BodyField(String),
    Query(String),
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MixTarget {
    Header(String),
    BodyField(String),
    Query(String),
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum BodyConversion {
    FormToJson,
    JsonToForm,
}