#![allow(dead_code, unused_imports)]
use axum::{
    body::{Bytes, Body},
    http::{header, Method, StatusCode, Uri},
    response::sse::{Event, KeepAlive, Sse},
    response::IntoResponse,
    response::Response,
    routing::any,
    Router,
};
use base64::prelude::*;
use config::{Transformation, UseMode};
use futures_util::StreamExt;
use reqwest::{Client,ClientBuilder};
use hyper::{header::HeaderValue, HeaderMap};
use serde_json::{Map, Value};
use std::{
    collections::HashMap, convert::Infallible, fmt::format, net::SocketAddr, str::{self, FromStr}
};
use tokio::task::yield_now;
use tracing::{event, Level};
use url::form_urlencoded;

mod config;
use crate::config::{
    AppConfig, BodyConversion, MethodMapping, MixAction, MixSource, MixTarget, PathConfig,
    ServiceType,
};
use ::config::{Config, Environment};
use async_stream;
use regex::Regex;
use tracing_subscriber::filter::{EnvFilter, LevelFilter};

async fn load_config() -> anyhow::Result<(AppConfig, HashMap<String, PathConfig>)> {
    let _ = dotenv::dotenv().ok(); // 预加载 .env

    event!(Level::INFO, "Loading config");

    let app_config: AppConfig = Config::builder()
        .add_source(Environment::with_prefix("SSO_ADAPTER"))
        .build()
        .map_err(|e| event!(Level::ERROR, "Failed to build config: {}", e))
        .unwrap()
        .try_deserialize()
        .map_err(|e| event!(Level::ERROR, "Failed to deserialize config: {}", e))
        .unwrap();

    event!(Level::DEBUG, "Loaded config app_config: {:?}", app_config);

    match app_config.use_mode {
        UseMode::Normal => {
            // 处理普通模式下的配置加载逻辑
            app_config
                .sso_url
                .as_ref()
                .expect("SSO URL must be provided in Normal mode"); // 确保 SSO URL 存在
        }
        UseMode::Proxy => {
            // 处理其他模式下的配置加载逻辑
            app_config
                .dify_host
                .as_ref()
                .expect("Dify Host must be provided in Proxy mode"); // 确保 Dify Host 存在
        }
    } // 根据 use_mode 加载不同的配置逻辑

    let config_content = std::fs::read_to_string(&app_config.config_path)?;
    let path_configs: HashMap<String, PathConfig> = serde_yaml::from_str(&config_content)
        .map_err(|e| {
            event!(Level::ERROR, "Failed to parse config file: {}", e);
        })
        .unwrap();

    event!(
        Level::DEBUG,
        "Loaded config path_configs: {:?}",
        path_configs
    );

    Ok((app_config, path_configs))
}

fn json_to_flat_map(value: &Value, prefix: &str, result: &mut HashMap<String, Value>) {
    match value {
        Value::Object(obj) => {
            for (key, val) in obj {
                let new_prefix = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{}.{}", prefix, key)
                };
                json_to_flat_map(val, &new_prefix, result);
            }
        }
        Value::Array(arr) => {
            for (index, item) in arr.iter().enumerate() {
                let new_prefix = format!("{}[{}]", prefix, index);
                json_to_flat_map(item, &new_prefix, result);
            }
        }
        primitive => {
            result.insert(prefix.to_string(), primitive.clone());
        }
    }
}

/// 解析键路径，支持数组语法
/// 示例输入："aa[0].bb[1].cc" → ["aa", "0", "bb", "1", "cc"]
fn parse_key_path(key: &str) -> Vec<&str> {
    let re = Regex::new(r"\[(\d+)\]|(?<index>\d+)|(?<word>\w+)").unwrap();
    let mut parts = Vec::new();

    for cap in re.captures_iter(key) {
        if let Some(num) = cap.get(1).or(cap.name("index")) {
            parts.push(num.as_str());
        } else if let Some(word) = cap.name("word") {
            parts.push(word.as_str());
        }
    }

    parts
}

// map转json
fn flat_map_to_json(map: &HashMap<String, Value>) -> Value {
    let mut root = Value::Object(Map::new());

    for (key, value) in map {
        let parts = parse_key_path(key);
        insert_recursive(&mut root, &parts, value.clone());
    }

    root
}
// 递归处理
fn insert_recursive(current: &mut Value, parts: &[&str], value: Value) {
    let (first, rest) = match parts.split_first() {
        Some(p) => p,
        None => return,
    };
    // 判断当前是否是数组索引
    let is_array_index = first.parse::<usize>().is_ok();

    if rest.is_empty() {
        // 叶节点：直接插入值
        if is_array_index {
            panic!("Cannot have array index at leaf node");
        }
        if current.is_object() {
            current
                .as_object_mut()
                .unwrap()
                .insert(first.to_string(), value);
        } else {
            let mut map = Map::new();
            map.insert(first.to_string(), value);
            *current = Value::Object(map);
        }
    } else if is_array_index {
        // 处理数组路径
        let index = first.parse().unwrap();
        if !current.is_array() {
            *current = Value::Array(Vec::new());
        }
        let arr = current.as_array_mut().unwrap();

        // 扩展数组到所需长度
        while arr.len() <= index {
            arr.push(Value::Null);
        }
        // 初始化元素为对象（如果当前位置是Null）
        if arr[index] == Value::Null {
            arr[index] = Value::Object(Map::new());
        }
        insert_recursive(&mut arr[index], rest, value);
    } else {
        // 确保当前是对象
        if !current.is_object() {
            *current = Value::Object(Map::new());
        }
        // 中间节点：递归处理
        let map = current.as_object_mut().unwrap();
        // 获取或创建子节点
        let entry = map
            .entry(first.to_string())
            .or_insert(Value::Object(Map::new()));

        insert_recursive(entry, rest, value);
    }
}

/// 递归处理数组标识
fn post_process_arrays(map: &mut Map<String, Value>) {
    if let Some(Value::Array(arr)) = map.remove("_array") {
        // 将当前对象替换为数组
        *map = Map::new();
        for (i, mut elem) in arr.into_iter().enumerate() {
            if let Value::Object(elem_map) = &mut elem {
                // 递归处理数组元素
                post_process_arrays(elem_map);
            }
            map.insert(i.to_string(), elem);
        }
    } else {
        // 常规递归处理
        for (_, v) in map.iter_mut() {
            if let Value::Object(child) = v {
                post_process_arrays(child);
            }
        }
    }
}

pub fn query_to_map(query: &str) -> HashMap<String, String> {
    form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}
// 保留所有值的版本（返回 Vec<String>）
pub fn query_to_multimap(query: &str) -> HashMap<String, Vec<String>> {
    let mut map = HashMap::new();

    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        map.entry(key.into_owned())
            .or_insert_with(Vec::new)
            .push(value.into_owned());
    }

    map
}
// HashMap<String, String> -> 查询字符串
pub fn map_to_query(map: &HashMap<String, String>) -> String {
    form_urlencoded::Serializer::new(String::new())
        .extend_pairs(map.iter())
        .finish()
}
// HashMap<String, Vec<String>> -> 查询字符串
pub fn multimap_to_query(multimap: &HashMap<String, Vec<String>>) -> String {
    form_urlencoded::Serializer::new(String::new())
        .extend_pairs(
            multimap
                .iter()
                .flat_map(|(k, vs)| vs.iter().map(move |v| (k.as_str(), v.as_str()))),
        )
        .finish()
}
// json body 多级转换
fn merge_subfields(
    map: &HashMap<String, Value>,
    parent_key: &str,
    pairs: &mut HashMap<String, Value>,
) {
    // 处理父键自身的值（单层结构）
    if let Some(value) = map.get(parent_key) {
        pairs.insert(parent_key.to_string(), value.clone());
    }

    if !pairs.is_empty() {
        return;
    }

    // 处理嵌套子字段（多层结构）
    let prefix = format!("{}.", parent_key);
    for (k, v) in map {
        if let Some(sub_key) = k.strip_prefix(&prefix) {
            pairs.insert(sub_key.to_string(), v.clone());
        }
    }
}
// json body 多级转换为字符串
fn json_body_to_string(
    map: &HashMap<String, Value>,
    format: &str, // 格式模板，如 "{key}={value}"
) -> String {
    let mut pairs = Vec::new();

    if map.len() == 1 {
        return map.values().take(1).nth(0).unwrap().as_str().unwrap().to_string();
    }
    for (k, v) in map {
        let value_str = match v {
            Value::String(s) => s.as_str().to_string(),
            _ => v.to_string(),
        };
        let formatted = format
            .replace("{key}", k)
            .replace("{value}", &value_str);
        pairs.push(formatted);
    }
    // 按字母顺序排序保证一致性
    pairs.sort();
    pairs.join("; ")
}
// 获取 method
fn get_method(config: &Option<PathConfig>, method: &Method) -> Method {
    match config {
        Some(config) => match config.request.method_mapping {
            Some(MethodMapping::GetToPost) => Method::POST,
            Some(MethodMapping::PostToGet) => Method::GET,
            None => method.clone(),
        },
        None => method.clone(),
    }
}

fn map_to_json_body(
    res_json_map: &HashMap<String, Value>,
) -> Result<(Option<mime::Mime>, Vec<u8>), (StatusCode, String)> {
    let json_body = serde_json::to_vec(&flat_map_to_json(res_json_map)).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("JSON conversion error: {}", e),
        )
    })?;
    Ok((Some(mime::APPLICATION_JSON), json_body))
}

fn map_to_form_body(
    res_json_map: &HashMap<String, Value>,
) -> Result<(Option<mime::Mime>, Vec<u8>), (StatusCode, String)> {
    let form_str = serde_urlencoded::to_string(res_json_map).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Form conversion error: {}", e),
        )
    })?;
    Ok((
        Some(mime::APPLICATION_WWW_FORM_URLENCODED),
        form_str.into_bytes(),
    ))
}

// 处理转换
fn apply_transformations(value: &str, transformations: &[Transformation]) -> Option<String> {
    let mut result = value.to_string();

    for transform in transformations {
        match transform {
            Transformation::Base64Decode => {
                result = base64::prelude::BASE64_STANDARD
                    .decode(&result)
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok())
                    .unwrap_or_default();
            }
            Transformation::Split { separator, index } => {
                result = result
                    .split(separator)
                    .nth(*index)
                    .unwrap_or_default()
                    .to_string();
            }
            Transformation::Replace { from, to } => {
                result = result.replace(from, to);
            }
            Transformation::Format { format } => {
                result = format!("{}{}", format, result);
            }
            Transformation::Append { value } => {
                result.push_str(value);
            }
            // Test
            Transformation::Extract { regex } => {
                let re = Regex::new(regex).unwrap();
                result = re.find(&result)
                .map(|mat| mat.as_str().to_string())
                .unwrap_or_else(|| result);
            }
        }

        if result.is_empty() {
            return None;
        }
    }

    Some(result)
}

// 重构，获取headervalue
fn get_header_val(
    headers_map: &mut hyper::HeaderMap,
    action: &config::MixAction,
    src: &String,
) -> Option<HeaderValue> {
    let value = match &action {
        MixAction::Move => headers_map.remove(src.as_str()),
        MixAction::Copy => headers_map.get(src.as_str()).cloned(),
        MixAction::AddTarget(value) => Some(value.parse().unwrap()),
        MixAction::DeleteSrc => {
            headers_map.remove(src.as_str());
            None
        }
    };
    value
}

// 重构，获取headervalue
fn get_querymap_val(
    map: &mut HashMap<String, Vec<String>>,
    action: &config::MixAction,
    src: &String,
) -> Option<Vec<String>> {
    let value = match &action {
        MixAction::Move => map.remove(src.as_str()),
        MixAction::Copy => map.get(src.as_str()).cloned(),
        MixAction::AddTarget(value) => Some(vec![value.clone().parse().unwrap()]),
        MixAction::DeleteSrc => {
            map.remove(src.as_str());
            None
        }
    };
    value
}

// 重构，获取headervalue
fn get_bodymap_val(
    map: &mut HashMap<String, String>,
    action: &config::MixAction,
    src: &String,
) -> Option<String> {
    let value = match &action {
        MixAction::Move => map.remove(src.as_str()),
        MixAction::Copy => map.get(src.as_str()).cloned(),
        MixAction::AddTarget(value) => Some(value.clone().parse().unwrap()),
        MixAction::DeleteSrc => {
            map.remove(src.as_str());
            None
        }
    };
    value
}

async fn proxy_handler(
    uri: Uri,
    method: Method,
    headers: header::HeaderMap,
    body: Bytes,
) -> Result<Response, (StatusCode, String)> {
    // if method == Method::CONNECT {
    //     return handle_https_tunnel(uri, *addr).await;
    // }

    let (app_config, path_configs) = load_config().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Config load failed: {}", e),
        )
    })?;
    // 使用模式
    let use_mode = app_config.use_mode.clone();
    event!(Level::INFO, "Use mode: {:?}", use_mode);

    // 记录请求基本信息
    event!(
        Level::INFO,
        "Received {} request to {} | Headers: {:?} | Body size: {} bytes",
        method,
        uri,
        headers
            .iter()
            .map(|(n, v)| format!("{}={}", n, v.to_str().unwrap()))
            .collect::<Vec<_>>(),
        body.len()
    );

    let path = uri.path();
    let query = uri.query();

    event!(Level::DEBUG, "Path: {:?}", path);
    event!(Level::DEBUG, "Query: {:?}", query);

    // 模式
    let (config, base_url) = match use_mode {
        // 代理模式，如果 config 为空， 则执行代理模式
        UseMode::Proxy => {
            let config = path_configs.get(path).clone();
            match config {
                // 命中配置
                Some(config) => (
                    Some(config.clone()),
                    match config.request.target_service {
                        ServiceType::Dify => &app_config.dify_url,
                        ServiceType::SSO | ServiceType::Redirect | ServiceType::SSE(_) => &uri
                            .host()
                            .ok_or((StatusCode::BAD_REQUEST, format!("Host header missing")))?
                            .to_string()
                            .clone(), // 使用原始请求的host
                    },
                ),
                // 未命中配置，判断是否为入栈请求，入栈请求则转发到 dify_url，否则转发到原始请求的host
                None => {
                    if uri.host().is_some()
                        && app_config.dify_host.is_some()
                        && app_config
                            .dify_host
                            .unwrap()
                            .eq(&uri.host().unwrap().to_string())
                    {
                        // 入栈
                        (None, &app_config.dify_url)
                    } else {
                        // 出站
                        (
                            None,
                            &format!(
                                "{}://{}",
                                uri.scheme_str().unwrap_or("https"),
                                uri.host().unwrap().to_string()
                            ),
                        )
                    }
                }
            }
            // 返回结果
        }
        // 正常模式， config 不能为空，否则返回404
        UseMode::Normal => {
            let config = path_configs
                .get(path)
                .ok_or((
                    StatusCode::NOT_FOUND,
                    format!("Path {} not configured", path),
                ))?
                .clone();
            // 返回结果
            (
                Some(config.clone()),
                match config.request.target_service {
                    ServiceType::Dify => &app_config.dify_url,
                    ServiceType::SSO | ServiceType::Redirect | ServiceType::SSE(_) => &app_config
                        .sso_url
                        .ok_or((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            format!("SSO URL not configured"),
                        ))?
                        .clone(),
                },
            )
        }
    };

    event!(Level::DEBUG, "matched config: {:?}", &config);

    // method转换
    let target_method = get_method(&config, &method);

    // query
    let mut query_map = match query {
        Some(query) => query_to_multimap(&query),
        None => HashMap::new(),
    };
    // body
    let mut json_map = HashMap::new();
    // headers
    let mut headers_map = HeaderMap::new();

    let content_type = match method {
        Method::POST | Method::PUT => headers.get(header::CONTENT_TYPE).cloned().ok_or((
            StatusCode::BAD_REQUEST,
            format!("Missing Header: content-type"),
        ))?,
        _ => HeaderValue::from_str("").unwrap(),
    };

    // 根据 content-type 解析 body 数据
    if content_type == mime::APPLICATION_JSON.essence_str() {
        // json
        let json_data: Value = serde_json::from_slice(&body)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("JSON parse error: {}", e)))?;
        json_to_flat_map(&json_data, "", &mut json_map);
    } else if content_type == mime::APPLICATION_WWW_FORM_URLENCODED.essence_str() {
        // form
        let form_data = serde_urlencoded::from_bytes::<HashMap<String, Value>>(&body)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("Form parse error: {}", e)))?;
        json_map = form_data.clone();
    }

    // 未匹配的，添加源header到新header
    headers_map.extend(headers.clone());

    // 处理request.mix_mappings
    if let Some(conf) = &config {
        for mapping in &conf.request.mix_mappings {
            let m = mapping.clone();
            let t = m.target.clone();
            let trans_s = m.transformations.clone();
            match (&m.source, t) {
                // Header to Header
                (MixSource::Header(src), MixTarget::Header(dst)) => {
                    if let Some(mut value) = get_header_val(&mut headers_map, &m.action, src) {
                        if let Some(trans) = trans_s.clone() {
                            if let Some(transformed) =
                            apply_transformations(&value.to_str().unwrap(), &trans)
                            {
                                value = transformed.parse().unwrap();
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        headers_map.insert(obj.as_str(), value.clone());
                    }
                }
                // Header to Body
                (MixSource::Header(src), MixTarget::BodyField(dst)) => {
                    if let Some(mut value) = get_header_val(&mut headers_map, &m.action, src) {
                        if let Some(trans) = trans_s.clone() {
                            if let Some(transformed) =
                            apply_transformations(&value.to_str().unwrap(), &trans)
                            {
                                value = transformed.parse().unwrap();
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        json_map.insert(
                            obj.to_string(),
                            Value::String(value.clone().to_str().unwrap().to_string()),
                        );
                    }
                }
                // Header to Query
                (MixSource::Header(src), MixTarget::Query(dst)) => {
                    if let Some(mut value) = get_header_val(&mut headers_map, &m.action, src) {
                        if let Some(trans) = trans_s.clone() {
                            if let Some(transformed) =
                            apply_transformations(&value.to_str().unwrap(), &trans)
                            {
                                value = transformed.parse().unwrap();
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        query_map.insert(
                            obj.to_string(),
                            vec![value.clone().to_str().unwrap().to_string()],
                        );
                    }
                }
                // Quert to Query
                (MixSource::Query(src), MixTarget::Query(dst)) => {
                    if let Some(mut value) = get_querymap_val(&mut query_map, &m.action, src){
                        if let Some(trans) = trans_s.clone() {
                            if let Some(transformed) =
                            apply_transformations(&value.join(",").as_str(), &trans)
                            {
                                let v:String = transformed.parse().unwrap();
                                value = vec!(v.split(",").collect());
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        query_map.insert(obj.to_string(), value);
                    }
                }
                // Query to Header
                (MixSource::Query(src), MixTarget::Header(dst)) => {
                    if let Some(mut value) = get_querymap_val(&mut query_map, &m.action, src){
                        if let Some(trans) = trans_s.clone() {
                            if let Some(transformed) =
                            apply_transformations(&value.join(",").as_str(), &trans)
                            {
                                let v:String = transformed.parse().unwrap();
                                value = vec!(v.split(",").collect());
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        headers_map.insert(
                            obj.as_str(),
                            HeaderValue::from_str(value.join(",").as_str()).unwrap(),
                        );
                    }
                }
                // Query to Body
                (MixSource::Query(src), MixTarget::BodyField(dst)) => {
                    if let Some(mut value) = get_querymap_val(&mut query_map, &m.action, src){
                        if let Some(trans) = trans_s.clone() {
                            if let Some(transformed) =
                            apply_transformations(&value.join(",").as_str(), &trans)
                            {
                                let v:String = transformed.parse().unwrap();
                                value = vec!(v.split(",").collect());
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        json_map.insert(obj.to_string(), Value::String(value.join(",")));
                    }
                }
                // Body to Body
                // TODO Handle transformations
                (MixSource::BodyField(src), MixTarget::BodyField(dst)) => {
                    let mut res_json = HashMap::<String, Value>::new();
                    merge_subfields(&json_map, &src, &mut res_json);
                    match &m.action {
                        MixAction::Move => {
                            for (k, v) in res_json.iter() {
                                let src_key = format!("{}.{}",src,k);
                                json_map.remove(src_key.as_str()); // delete source
                                json_map.remove(k.as_str()); // delete source
                                json_map.insert(k.clone().replace(src, dst.as_str()), v.clone());
                            }
                        }
                        MixAction::Copy => {
                            for (k, v) in res_json.iter() {
                                json_map.insert(k.clone().replace(src, dst.as_str()), v.clone());
                            }
                        }
                        MixAction::AddTarget(v) => {
                            json_map.insert(src.clone(), Value::String(v.clone()));
                        }
                        MixAction::DeleteSrc => {
                            for (k, _) in res_json.iter() {
                                json_map.remove(k.as_str()); // delete source
                            }
                        }
                    };
                }
                // Body to Query
                // TODO Handle transformations
                (MixSource::BodyField(src), MixTarget::Query(dst)) => {
                    let mut res_json = HashMap::<String, Value>::new();
                    merge_subfields(&json_map, &src, &mut res_json);
                    let value = match &m.action {
                        MixAction::Move => {
                            for (k, _) in res_json.iter() {
                                let src_key = format!("{}.{}",src,k);
                                json_map.remove(src_key.as_str()); // delete source
                                json_map.remove(k.as_str());
                            }
                            Some(json_body_to_string(&res_json, "{key}={value}"))
                        }
                        MixAction::Copy => Some(json_body_to_string(&res_json, "{key}={value}")),
                        MixAction::AddTarget(v) => Some(v.clone()), // Add a static value to the query
                        MixAction::DeleteSrc => {
                            for (k, _) in res_json.iter() {
                                json_map.remove(k.as_str());
                            }
                            None
                        }
                    };
                    if let Some(value) = value {
                        let obj = Box::leak(Box::new(dst));
                        query_map.insert(obj.to_string(), vec![value]);
                    }
                }
                // Body to Header
                // TODO Handle transformations
                (MixSource::BodyField(src), MixTarget::Header(dst)) => {
                    let mut res_json = HashMap::<String, Value>::new();
                    merge_subfields(&json_map, &src, &mut res_json);
                    let value = match &m.action {
                        MixAction::Move => {
                            for (k, _) in res_json.iter() {
                                let src_key = format!("{}.{}",src,k);
                                json_map.remove(src_key.as_str()); // delete source
                                json_map.remove(k.as_str());
                            }
                            Some(json_body_to_string(&res_json, "{key}={value}"))
                        }
                        MixAction::Copy => Some(json_body_to_string(&res_json, "{key}={value}")),
                        MixAction::AddTarget(v) => Some(v.clone()), // Add a static value to the query
                        MixAction::DeleteSrc => {
                            for (k, _) in res_json.iter() {
                                json_map.remove(k.as_str());
                            }
                            None
                        }
                    };
                    if let Some(value) = value {
                        let obj = Box::leak(Box::new(dst));
                        headers_map
                            .insert(obj.as_str(), HeaderValue::from_str(value.as_str()).unwrap());
                    }
                }
            }
        }
    }

    event!(Level::DEBUG, "final body : {:?}", json_map);

    // 目标地址处理 + query参数
    let target_url = if query_map.len() > 0 {
        format!("{}{}?{}", base_url, path, multimap_to_query(&query_map))
    } else {
        format!("{}{}", base_url, path)
    };
    event!(Level::DEBUG, "Target URL: {}", target_url);

    let c = config.clone();

    // redirect处理
    let req_red = match c.clone().unwrap().request.target_service.clone() {
        ServiceType::Redirect => {
            // 处理重定向服务的请求
            let mut h = header::HeaderMap::new();
            h.insert(
                header::LOCATION,
                HeaderValue::from_str(target_url.as_str()).unwrap(),
            );
            let b = Vec::<u8>::new();
            // 返回重定向响应
            Some((StatusCode::FOUND, h, axum::body::Bytes::from(b)))
        }
        _ => None,
    };

    if req_red.is_some() {
        return Ok(req_red.unwrap().into_response());
    }

    let def_json_body = (
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.parse().unwrap()),
        body.to_vec(),
    );
    // 生成真实请求body
    let (content_type, converted_body) = match &config {
        Some(config) => match config.request.body_conversion {
            Some(BodyConversion::FormToJson) => map_to_json_body(&json_map)?,
            Some(BodyConversion::JsonToForm) => {
                let form_str = serde_urlencoded::to_string(&json_map).map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("Form conversion error: {}", e),
                    )
                })?;
                (
                    Some(mime::APPLICATION_WWW_FORM_URLENCODED),
                    form_str.into_bytes(),
                )
            }
            None => {
                if !json_map.is_empty() {
                    map_to_json_body(&json_map)?
                } else {
                    def_json_body
                }
            }
        },
        None => {
            if !json_map.is_empty() {
                map_to_json_body(&json_map)?
            } else {
                def_json_body
            }
        }
    };

    let mut _b = String::new();
    _b = String::from_utf8(converted_body.clone()).unwrap();
    event!(Level::DEBUG, "Request Body: {:?}", &_b);

    // 转换body类型
    if content_type.is_some() {
        // 处理Body转换的header
        headers_map.remove(header::CONTENT_TYPE);
        headers_map.insert(
            header::CONTENT_TYPE,
            content_type.unwrap().to_string().parse().unwrap(),
        );
    }

    // 请求模式需要修改 host头
    if use_mode == UseMode::Normal {
        let to_host = Uri::from_str(base_url).unwrap().host().unwrap().to_string();
        // 处理 host header
        headers_map.remove(header::HOST);
        headers_map.insert(header::HOST, to_host.parse().unwrap()); // 设置目标host
    }

    // 默认更新
    headers_map.insert(
        header::CONTENT_LENGTH,
        converted_body.len().to_string().parse().unwrap(),
    );

    if headers_map.contains_key(header::TRANSFER_ENCODING) {
        let transfer_encoding = headers_map.get(header::TRANSFER_ENCODING);
        if transfer_encoding.is_some()
            && transfer_encoding
                .unwrap()
                .to_str()
                .unwrap()
                .contains("chunked")
        {
            headers_map.remove(header::CONTENT_LENGTH);
        }
    }

    // 移除压缩编码头
    if headers_map.contains_key(header::ACCEPT_ENCODING){
        headers_map.remove(header::ACCEPT_ENCODING);
    }
    event!(Level::DEBUG, "Request Headers: {:?}", headers_map);

    event!(
        Level::INFO,
        "Sending {} request to {} with Headers: {:?} and Body size: {}",
        &target_method,
        &target_url,
        &headers_map,
        &converted_body.len()
    );

    // sse 处理 所有前置处理完成后
    let is_sse_req = match c.unwrap().request.target_service.clone() {
        ServiceType::SSE(source) => {
            // 解析配置字符串（例如 "bodyfield-stream"）
            let (src_type, src_value) = source.split_once('-').expect("Invalid SSE source format");
            match src_type.to_lowercase().as_str() {
                "bodyfield" => json_map.get(src_value).and_then(|v| v.as_bool()).unwrap(),
                "header" => headers_map
                    .get(src_value)
                    .and_then(|hv| hv.to_str().ok())
                    .and_then(|s| s.parse().ok()).unwrap(),
                "query" => query_map
                    .get(src_value)
                    .map(|values| Value::String(values.join(",")))
                    .and_then(|v| v.as_bool()).unwrap(),
                _ => false,
            }
        }
        _ => false,
    };

    event!(Level::DEBUG, "is_sse_req: {:?}", is_sse_req);

    // 发送请求
    let client = ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .no_gzip()
        .build();


    let request_builder = match target_method {
        Method::GET => client.unwrap().get(&target_url.clone()),
        Method::POST => client.unwrap().post(&target_url.clone()).body(converted_body),
        _ => unreachable!(),
    };

    let response = request_builder
        .headers(headers_map)
        .send()
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                format!("Forward request failed: {}", e),
            )
        })?;

    // redirect
    if response.status().is_redirection() {
        let mut red_headers_map = header::HeaderMap::new();
        let location_header = response.headers();
        red_headers_map.extend(location_header.clone());
        let b = Vec::<u8>::new();
        event!(Level::DEBUG, "Redirect Header: {:?}", red_headers_map);
        return Ok((
            response.status(),
            red_headers_map,
            axum::body::Bytes::from(b)
        ).into_response());
    }

    // headers
    let mut res_headers_map = header::HeaderMap::new();

    // response mapping processing
    let res_header = response.headers().clone();

    // 未匹配的，添加response header到新header
    res_headers_map.extend(res_header.clone());

    event!(Level::DEBUG, "Response Headers: {:?}", res_headers_map);

    let res_status = response.status();

    event!(Level::DEBUG, "Response res_status: {:?}", res_status);

    // sse 忽略 response 配置
    if is_sse_req {
        res_headers_map.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        );

        event!(Level::DEBUG, "SSE request, ignore response config");

        let stream = async_stream::stream! {
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                        Ok(c) => c,
                        Err(e) => {
                            let error_msg = format!("Error: {}", e);
                            yield Ok::<Vec<u8>, std::io::Error>(error_msg.into_bytes()); 
                            continue;
                        }
                    };
                event!(Level::DEBUG, "SSE chunk: {:?}", chunk);

                yield Ok(
                    chunk.to_vec()
                );
            };
        };

        return Ok((
            res_status,
            res_headers_map,
            Body::from_stream(stream),
        )
            .into_response());
    }

    // 没有配置response mix_mappings，直接返回response
    if config.clone().is_some() {
        let config = config.as_ref().unwrap();
        if config.response.mix_mappings.is_empty() {
            event!(
                Level::DEBUG,
                "No need process mix, Return response directly"
            );
            return Ok((
                res_status,
                res_headers_map,
                axum::body::Bytes::from(response.bytes().await.unwrap()),
            )
                .into_response());
        }
    }

    let res_body = response.bytes().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Body read failed: {}", e),
        )
    })?;

    event!(
        Level::INFO,
        "Received Response {} from {} | Headers: {:?} | Body size: {} bytes",
        res_status,
        target_url,
        res_header
            .iter()
            .map(|(n, v)| format!("{}={}", n, v.to_str().unwrap()))
            .collect::<Vec<_>>(),
        res_body.len()
    );
    
    let mut _rb = String::new();
    _rb = String::from_utf8(res_body.clone().to_vec()).unwrap();
    event!(Level::DEBUG, "Response origin Body: {:?}", &_rb);

    // body
    let mut res_json_map = HashMap::new();

    let res_content_type = res_headers_map.get(header::CONTENT_TYPE).cloned().ok_or((
        StatusCode::BAD_REQUEST,
        format!("Missing Header: content-type"),
    ))?;

    // 根据 response content-type 解析 res_body 数据
    if res_content_type
        .to_str()
        .unwrap()
        .starts_with(mime::APPLICATION_JSON.essence_str()) || 
        res_content_type
        .to_str()
        .unwrap()
        .starts_with(mime::TEXT_PLAIN.essence_str())
    {
        // json
        let json_data: Value = serde_json::from_slice(&res_body)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("JSON parse error: {}", e)))?;
        json_to_flat_map(&json_data, "", &mut res_json_map);
    } else if res_content_type
        .to_str()
        .unwrap()
        .starts_with(mime::APPLICATION_WWW_FORM_URLENCODED.essence_str())
    {
        // form
        let form_data = serde_urlencoded::from_bytes::<HashMap<String, Value>>(&res_body)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("Form parse error: {}", e)))?;
        res_json_map = form_data.clone();
    }

    // 处理request.mix_mappings
    if let Some(conf) = &config {
        for mapping in &conf.response.mix_mappings {
            let m = mapping.clone();
            let t = m.target.clone();
            let trans_s = m.transformations.clone();
            match (&m.source, t) {
                // Header to Header
                (MixSource::Header(src), MixTarget::Header(dst)) => {
                    if let Some(mut value) = get_header_val(&mut res_headers_map, &m.action, src) {
                        if let Some(trans) = trans_s.clone() {
                            if let Some(transformed) =
                            apply_transformations(&value.to_str().unwrap(), &trans)
                            {
                                value = transformed.parse().unwrap();
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        res_headers_map.insert(obj.as_str(), value.clone());
                    }
                }
                // Header to Body
                (MixSource::Header(src), MixTarget::BodyField(dst)) => {
                    if let Some(mut value) = get_header_val(&mut res_headers_map, &m.action, src){
                        if let Some(trans) = trans_s.clone() {
                            if let Some(transformed) =
                            apply_transformations(&value.to_str().unwrap(), &trans)
                            {
                                value = transformed.parse().unwrap();
                            }
                        }
                        let obj = Box::leak(Box::new(dst));
                        res_json_map.insert(
                            obj.to_string(),
                            Value::String(value.clone().to_str().unwrap().to_string()),
                        );
                    }
                }
                // Body to Body
                (MixSource::BodyField(src), MixTarget::BodyField(dst)) => {
                    let mut res_json = HashMap::<String, Value>::new();
                    merge_subfields(&res_json_map, &src, &mut res_json);
                    match &m.action {
                        MixAction::Move => {
                            for (k, v) in res_json.iter() {
                                let src_key = format!("{}.{}",src,k);
                                res_json_map.remove(src_key.as_str()); // delete source
                                res_json_map
                                    .insert(k.clone().replace(src, dst.as_str()), v.clone());
                            }
                        }
                        MixAction::Copy => {
                            for (k, v) in res_json.iter() {
                                res_json_map
                                    .insert(k.clone().replace(src, dst.as_str()), v.clone());
                            }
                        }
                        MixAction::AddTarget(v) => {
                            res_json_map.insert(src.clone(), Value::String(v.clone()));
                        }
                        MixAction::DeleteSrc => {
                            for (k, _) in res_json.iter() {
                                res_json_map.remove(k.as_str()); // delete source
                            }
                        }
                    };
                }
                // Body to Header
                (MixSource::BodyField(src), MixTarget::Header(dst)) => {
                    let mut res_json = HashMap::<String, Value>::new();
                    merge_subfields(&res_json_map, &src, &mut res_json);
                    let value = match &m.action {
                        MixAction::Move => {
                            for (k, _) in res_json.iter() {
                                let src_key = format!("{}.{}",src,k);
                                res_json_map.remove(src_key.as_str()); // delete source
                                res_json_map.remove(k.as_str());
                            }
                            Some(json_body_to_string(&res_json, "{key}={value}"))
                        }
                        MixAction::Copy => Some(json_body_to_string(&res_json, "{key}={value}")),
                        MixAction::AddTarget(v) => Some(v.clone()), // Add a static value to the query
                        MixAction::DeleteSrc => {
                            for (k, _) in res_json.iter() {
                                json_map.remove(k.as_str());
                            }
                            None
                        }
                    };
                    if let Some(value) = value {
                        let obj = Box::leak(Box::new(dst));
                        res_headers_map
                            .insert(obj.as_str(), HeaderValue::from_str(value.as_str()).unwrap());
                    }
                }
                // 无其他配置
                _ => {}
            }
        }
    }

    let def_res_json_body = (
        res_header
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.parse().unwrap()),
        res_body.to_vec(),
    );

    let (res_content_type, res_converted_body) = match &config {
        Some(config) => match config.response.body_conversion {
            Some(BodyConversion::FormToJson) => map_to_json_body(&res_json_map)?,
            Some(BodyConversion::JsonToForm) => map_to_form_body(&res_json_map)?,
            None => {
                if !res_json_map.is_empty() {
                    map_to_json_body(&res_json_map)?
                } else {
                    def_res_json_body
                }
            }
        },
        None => {
            if !res_json_map.is_empty() {
                // 没有配置 body 转换，但是有其他地方有移动进来的数据，需要转换为 JSON
                map_to_json_body(&res_json_map)?
            } else {
                // 原始数据。没有做修改
                def_res_json_body
            }
        }
    };

    if res_content_type.is_some() {
        // 处理Body转换的header
        res_headers_map.remove(header::CONTENT_TYPE);
        res_headers_map.insert(
            header::CONTENT_TYPE,
            res_content_type.unwrap().to_string().parse().unwrap(),
        );
    }

    // 代理模式不需要处理host
    if use_mode == UseMode::Normal {
        let from_host = app_config.self_host.clone();
        // 处理 host header
        res_headers_map.remove(header::HOST);
        res_headers_map.insert(header::HOST, from_host.parse().unwrap()); // 设置目标host
    }

    if res_headers_map.contains_key(header::TRANSFER_ENCODING) {
        let transfer_encoding = res_headers_map.get(header::TRANSFER_ENCODING);
        if transfer_encoding.is_some()
            && transfer_encoding
                .unwrap()
                .to_str()
                .unwrap()
                .contains("chunked")
        {
            res_headers_map.remove(header::CONTENT_LENGTH);

            event!(Level::DEBUG, "Response status: {:?}", res_status);
            event!(Level::DEBUG, "Response headers: {:?}", res_headers_map);
            let mut _rb = String::new();
            _rb = String::from_utf8(res_converted_body.clone()).unwrap();
            event!(Level::DEBUG, "Response Body: {:?}", &_rb);

            return Ok((
                res_status,
                res_headers_map,
                axum::body::Bytes::from(res_converted_body),
            )
                .into_response());
        }
    }
    let status = res_status;
    let body = res_converted_body.clone();
    // 处理Body转换后的header
    res_headers_map.remove(header::CONTENT_LENGTH);
    res_headers_map.insert(
        header::CONTENT_LENGTH,
        body.len().to_string().parse().unwrap(),
    );

    let headers = res_headers_map;

    event!(Level::DEBUG, "Response status: {:?}", status);
    event!(Level::DEBUG, "Response headers: {:?}", headers);
    let mut _rb = String::new();
    _rb = String::from_utf8(res_converted_body.clone()).unwrap();
    event!(Level::DEBUG, "Response Body: {:?}", &_rb);

    event!(
        Level::INFO,
        "Response {} to {} | Headers: {:?} | Body size: {} bytes",
        status,
        uri,
        headers
            .iter()
            .map(|(n, v)| format!("{}={}", n, v.to_str().unwrap()))
            .collect::<Vec<_>>(),
        body.len()
    );

    Ok((status, headers, axum::body::Bytes::from(body)).into_response())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    let app = Router::new().fallback(any(proxy_handler));
    event!(Level::INFO, "Starting sso_adapter server on port 8080");
    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener,app.into_make_service())
        .await
        .unwrap();
}
