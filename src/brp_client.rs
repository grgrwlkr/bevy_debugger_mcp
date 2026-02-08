#![allow(clippy::result_large_err)]

use futures_util::{SinkExt, StreamExt};
use regex::Regex;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, RwLock};
use tokio::time::{interval, Instant};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, warn};
use url::Url;

use crate::brp_command_handler::{BrpCommandHandler, CommandHandlerRegistry, CoreBrpHandler};
use crate::brp_messages::{
    BrpError, BrpErrorCode, BrpRequest, BrpResponse, BrpResult, ComponentFilter, ComponentTypeId,
    ComponentTypeInfo, ComponentValue, EntityData, FilterOp, QueryFilter,
};
use crate::config::Config;
use crate::debug_command_processor::DebugCommandRouter;
use crate::error::{Error, Result};
use crate::resource_manager::ResourceManager;

/// Batched request for efficient processing with proper cleanup
#[derive(Debug)]
struct BatchedRequest {
    request: BrpRequest,
    timestamp: Instant,
    response_tx: mpsc::Sender<Result<BrpResponse>>,
}

impl BatchedRequest {
    /// Send response and handle channel cleanup
    #[allow(dead_code)]
    async fn send_response(self, response: Result<BrpResponse>) {
        // Attempt to send response, ignoring receiver disconnect errors
        // as this is normal when the receiver is dropped
        let _ = self.response_tx.send(response).await;
    }

    /// Check if request has expired based on timeout
    #[allow(dead_code)]
    fn is_expired(&self, timeout: Duration) -> bool {
        self.timestamp.elapsed() > timeout
    }
}

impl Clone for BatchedRequest {
    fn clone(&self) -> Self {
        Self {
            request: self.request.clone(),
            timestamp: self.timestamp,
            response_tx: self.response_tx.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrpTransport {
    WebSocket,
    Http,
}

impl BrpTransport {
    fn from_url(url: &str) -> Result<Self> {
        let parsed =
            Url::parse(url).map_err(|e| Error::Connection(format!("Invalid BRP URL: {e}")))?;
        match parsed.scheme() {
            "ws" | "wss" => Ok(Self::WebSocket),
            "http" | "https" => Ok(Self::Http),
            scheme => Err(Error::Connection(format!(
                "Unsupported BRP URL scheme: {scheme}"
            ))),
        }
    }
}

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    method: String,
    id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcResponse {
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
    #[serde(default)]
    data: Option<Value>,
}

struct JsonRpcPayload {
    method: String,
    params: Option<Value>,
}

/// BRP client with extensible command handler support
pub struct BrpClient {
    config: Config,
    ws_stream: Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    http_client: Option<HttpClient>,
    connected: bool,
    retry_count: u32,
    resource_manager: Option<Arc<RwLock<ResourceManager>>>,
    request_queue: Arc<RwLock<VecDeque<BatchedRequest>>>,
    batch_processor_handle: Option<tokio::task::JoinHandle<()>>,
    command_registry: Arc<CommandHandlerRegistry>,
    debug_router: Option<Arc<DebugCommandRouter>>,
    component_type_cache: Option<Vec<String>>,
    next_request_id: u64,
}

impl std::fmt::Debug for BrpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrpClient")
            .field("config", &self.config)
            .field("connected", &self.connected)
            .field("retry_count", &self.retry_count)
            .field("has_resource_manager", &self.resource_manager.is_some())
            .field("has_debug_router", &self.debug_router.is_some())
            .finish()
    }
}

impl BrpClient {
    pub fn new(config: &Config) -> Self {
        let command_registry = Arc::new(CommandHandlerRegistry::new());

        BrpClient {
            config: config.clone(),
            ws_stream: None,
            http_client: None,
            connected: false,
            retry_count: 0,
            resource_manager: None,
            request_queue: Arc::new(RwLock::new(VecDeque::new())),
            batch_processor_handle: None,
            command_registry,
            debug_router: None,
            component_type_cache: None,
            next_request_id: 1,
        }
    }

    /// Initialize the client asynchronously with default handlers
    pub async fn init(&self) -> Result<()> {
        // Register core handler - safe async initialization
        let core_handler = Arc::new(CoreBrpHandler);
        self.command_registry.register(core_handler).await;
        Ok(())
    }

    pub fn with_resource_manager(mut self, resource_manager: Arc<RwLock<ResourceManager>>) -> Self {
        self.resource_manager = Some(resource_manager);
        self
    }

    /// Set the debug command router for handling debug commands
    pub fn with_debug_router(mut self, router: Arc<DebugCommandRouter>) -> Self {
        self.debug_router = Some(router);
        self
    }

    /// Register a custom command handler
    pub async fn register_handler(&self, handler: Arc<dyn BrpCommandHandler>) {
        self.command_registry.register(handler).await;
    }

    /// Get the command registry for external access
    pub fn command_registry(&self) -> Arc<CommandHandlerRegistry> {
        self.command_registry.clone()
    }

    pub async fn connect_with_retry(&mut self) -> Result<()> {
        const MAX_RETRIES: u32 = 5;
        const BASE_DELAY: Duration = Duration::from_millis(1000);

        while self.retry_count < MAX_RETRIES {
            match self.connect().await {
                Ok(()) => {
                    info!("Successfully connected to BRP at {}", self.config.brp_url());
                    self.retry_count = 0;
                    return Ok(());
                }
                Err(e) => {
                    self.retry_count += 1;
                    let delay = BASE_DELAY * 2_u32.pow(self.retry_count.min(5));
                    warn!(
                        "Failed to connect to BRP (attempt {}/{}): {}. Retrying in {:?}",
                        self.retry_count, MAX_RETRIES, e, delay
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }

        Err(Error::Connection(format!(
            "Failed to connect to BRP after {MAX_RETRIES} attempts"
        )))
    }

    async fn connect(&mut self) -> Result<()> {
        let url_str = self.config.brp_url();
        match BrpTransport::from_url(&url_str)? {
            BrpTransport::WebSocket => self.connect_websocket(&url_str).await,
            BrpTransport::Http => self.connect_http().await,
        }
    }

    async fn connect_websocket(&mut self, url_str: &str) -> Result<()> {
        let url =
            Url::parse(url_str).map_err(|e| Error::Connection(format!("Invalid BRP URL: {e}")))?;

        debug!("Attempting to connect to {}", url);
        let (ws_stream, _) = connect_async(url_str)
            .await
            .map_err(|e| Error::WebSocket(Box::new(e)))?;

        self.ws_stream = Some(ws_stream);
        self.http_client = None;
        self.connected = true;

        Ok(())
    }

    async fn connect_http(&mut self) -> Result<()> {
        let url_str = self.config.brp_url();
        debug!("Attempting to connect to {}", url_str);

        self.ws_stream = None;
        let response = self.send_http_jsonrpc("rpc.discover", None).await?;
        if let Some(error) = response.error {
            return Err(Error::Brp(format!(
                "BRP JSON-RPC error {}: {}",
                error.code, error.message
            )));
        }

        self.connected = true;
        Ok(())
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Send a BRP request and return the response (with resource management)
    pub async fn send_request(&mut self, request: &BrpRequest) -> Result<BrpResponse> {
        // Check rate limiting if resource manager is available
        if let Some(ref rm) = self.resource_manager {
            let resource_manager = rm.read().await;
            if !resource_manager.check_brp_rate_limit().await {
                return Err(Error::Validation(
                    "BRP request rate limit exceeded".to_string(),
                ));
            }

            // Acquire operation permit
            let _permit = resource_manager.acquire_operation_permit().await?;

            // Check if we should sample this request
            if !resource_manager.should_sample().await {
                debug!("Skipping BRP request due to adaptive sampling");
                // Return a mock response or cached result here if needed
                return Err(Error::Validation(
                    "Request skipped due to adaptive sampling".to_string(),
                ));
            }
        }

        let start_time = Instant::now();
        let result = self.send_request_internal(request).await;
        let duration = start_time.elapsed();

        // Record success/failure for circuit breaker
        if let Some(ref rm) = self.resource_manager {
            let resource_manager = rm.read().await;
            match &result {
                Ok(_) => {
                    resource_manager.record_operation_success().await;
                    debug!("Request completed in {:?}", duration);
                }
                Err(_) => {
                    resource_manager.record_operation_failure().await;
                    debug!("Request failed after {:?}", duration);
                }
            }
        }

        result
    }

    /// Internal send request without resource management
    async fn send_request_internal(&mut self, request: &BrpRequest) -> Result<BrpResponse> {
        match BrpTransport::from_url(&self.config.brp_url())? {
            BrpTransport::WebSocket => self.send_request_websocket(request).await,
            BrpTransport::Http => self.send_request_http(request).await,
        }
    }

    async fn send_request_websocket(&mut self, request: &BrpRequest) -> Result<BrpResponse> {
        let request_json = serde_json::to_string(request)?;
        self.send_message(&request_json).await?;

        // Wait for response with timeout
        let response = tokio::time::timeout(Duration::from_secs(5), self.receive_message())
            .await
            .map_err(|_| Error::Connection("Request timeout".to_string()))?;

        match response? {
            Some(response_text) => serde_json::from_str(&response_text).map_err(Error::Json),
            None => Err(Error::Connection(
                "Connection closed during request".to_string(),
            )),
        }
    }

    async fn send_request_http(&mut self, request: &BrpRequest) -> Result<BrpResponse> {
        let payload = self.build_jsonrpc_payload(request).await?;
        let response = self
            .send_http_jsonrpc(&payload.method, payload.params)
            .await?;

        if let Some(error) = response.error {
            return Ok(self.map_jsonrpc_error(error));
        }

        let result = response
            .result
            .ok_or_else(|| Error::Brp("Missing JSON-RPC result".to_string()))?;
        let brp_response = self.convert_jsonrpc_result(request, result).await?;
        self.connected = true;
        Ok(brp_response)
    }

    async fn build_jsonrpc_payload(&mut self, request: &BrpRequest) -> Result<JsonRpcPayload> {
        match request {
            BrpRequest::Query { filter, strict, .. } => {
                let resolved_filter = self.resolve_query_filter(filter.as_ref()).await?;
                let mut params = serde_json::Map::new();
                params.insert(
                    "data".to_string(),
                    json!({
                        "components": [],
                        "option": "all",
                        "has": []
                    }),
                );
                if let Some(filter_value) = resolved_filter.as_ref().and_then(Self::filter_to_json)
                {
                    params.insert("filter".to_string(), filter_value);
                }
                params.insert("strict".to_string(), json!(strict.unwrap_or(false)));

                Ok(JsonRpcPayload {
                    method: "world.query".to_string(),
                    params: Some(Value::Object(params)),
                })
            }
            BrpRequest::ListEntities { filter } => {
                let resolved_filter = self.resolve_query_filter(filter.as_ref()).await?;
                let mut params = serde_json::Map::new();
                params.insert(
                    "data".to_string(),
                    json!({
                        "components": [],
                        "option": [],
                        "has": []
                    }),
                );
                if let Some(filter_value) = resolved_filter.as_ref().and_then(Self::filter_to_json)
                {
                    params.insert("filter".to_string(), filter_value);
                }
                params.insert("strict".to_string(), json!(false));

                Ok(JsonRpcPayload {
                    method: "world.query".to_string(),
                    params: Some(Value::Object(params)),
                })
            }
            BrpRequest::Get { entity, components } => {
                let component_list = if let Some(list) = components {
                    self.resolve_component_types(list).await?
                } else {
                    self.get_component_types().await?
                };

                Ok(JsonRpcPayload {
                    method: "world.get_components".to_string(),
                    params: Some(json!({
                        "entity": entity,
                        "components": component_list,
                        "strict": false
                    })),
                })
            }
            BrpRequest::QueryEntity { entity_id } => {
                let component_list = self.get_component_types().await?;
                Ok(JsonRpcPayload {
                    method: "world.get_components".to_string(),
                    params: Some(json!({
                        "entity": entity_id,
                        "components": component_list,
                        "strict": false
                    })),
                })
            }
            BrpRequest::ListComponents => Ok(JsonRpcPayload {
                method: "world.list_components".to_string(),
                params: None,
            }),
            BrpRequest::Set { entity, components } | BrpRequest::Insert { entity, components } => {
                let resolved_components = self.resolve_component_map(components).await?;
                Ok(JsonRpcPayload {
                    method: "world.insert_components".to_string(),
                    params: Some(json!({
                        "entity": entity,
                        "components": resolved_components
                    })),
                })
            }
            BrpRequest::Remove { entity, components } => {
                let resolved_components = self.resolve_component_types(components).await?;
                Ok(JsonRpcPayload {
                    method: "world.remove_components".to_string(),
                    params: Some(json!({
                        "entity": entity,
                        "components": resolved_components
                    })),
                })
            }
            BrpRequest::Reparent { entity, parent } => Ok(JsonRpcPayload {
                method: "world.reparent_entities".to_string(),
                params: Some(json!({
                    "entities": [entity],
                    "parent": parent
                })),
            }),
            BrpRequest::Spawn { components } => {
                let resolved_components = self.resolve_component_map(components).await?;
                Ok(JsonRpcPayload {
                    method: "world.spawn_entity".to_string(),
                    params: Some(json!({
                        "components": resolved_components
                    })),
                })
            }
            BrpRequest::SpawnEntity { components } => {
                let mut raw_components = HashMap::new();
                for (component, value) in components {
                    raw_components.insert(component.clone(), value.clone());
                }
                let resolved_components = self.resolve_component_map(&raw_components).await?;
                Ok(JsonRpcPayload {
                    method: "world.spawn_entity".to_string(),
                    params: Some(json!({
                        "components": resolved_components
                    })),
                })
            }
            BrpRequest::ModifyEntity {
                entity_id,
                components,
            } => {
                let mut raw_components = HashMap::new();
                for (component, value) in components {
                    raw_components.insert(component.clone(), value.clone());
                }
                let resolved_components = self.resolve_component_map(&raw_components).await?;
                Ok(JsonRpcPayload {
                    method: "world.insert_components".to_string(),
                    params: Some(json!({
                        "entity": entity_id,
                        "components": resolved_components
                    })),
                })
            }
            BrpRequest::Destroy { entity } | BrpRequest::DeleteEntity { entity_id: entity } => {
                Ok(JsonRpcPayload {
                    method: "world.despawn_entity".to_string(),
                    params: Some(json!({ "entity": entity })),
                })
            }
            BrpRequest::Screenshot { .. } => Err(Error::Brp(
                "Screenshot requires a custom game handler".to_string(),
            )),
            BrpRequest::Debug { .. } => Err(Error::Brp(
                "Debug commands are handled by local processors".to_string(),
            )),
        }
    }

    async fn send_http_jsonrpc(
        &mut self,
        method: &str,
        params: Option<Value>,
    ) -> Result<JsonRpcResponse> {
        let url = self.config.brp_url();
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            method: method.to_string(),
            id: self.next_request_id(),
            params,
        };
        let body = serde_json::to_string(&request)?;
        let client = self.http_client.get_or_insert_with(HttpClient::new);

        let response = tokio::time::timeout(
            self.config.resilience.request_timeout,
            client
                .post(&url)
                .header("content-type", "application/json")
                .body(body)
                .send(),
        )
        .await
        .map_err(|_| Error::Connection("HTTP request timeout".to_string()))?
        .map_err(|e| {
            self.connected = false;
            Error::Connection(format!("HTTP request failed: {e}"))
        })?;

        let status = response.status();
        let text = response.text().await.map_err(|e| {
            self.connected = false;
            Error::Connection(format!("Failed to read HTTP response: {e}"))
        })?;

        if !status.is_success() {
            self.connected = false;
            return Err(Error::Connection(format!("HTTP {}: {}", status, text)));
        }

        serde_json::from_str(&text).map_err(Error::Json)
    }

    fn map_jsonrpc_error(&self, error: JsonRpcError) -> BrpResponse {
        let code = match error.code {
            -32602..=-32600 => BrpErrorCode::InvalidQuery,
            _ => BrpErrorCode::InternalError,
        };

        BrpResponse::Error(BrpError {
            code,
            message: error.message,
            details: error.data,
        })
    }

    async fn convert_jsonrpc_result(
        &mut self,
        request: &BrpRequest,
        result: Value,
    ) -> Result<BrpResponse> {
        match request {
            BrpRequest::Query { filter, limit, .. } => {
                let resolved_filter = self.resolve_query_filter(filter.as_ref()).await?;
                let mut entities = self.parse_query_result(&result)?;

                if let Some(filter) = resolved_filter {
                    if let Some(where_clause) = &filter.where_clause {
                        entities = self.apply_where_clause(entities, where_clause);
                    }
                }

                if let Some(limit) = limit {
                    entities.truncate(*limit);
                }

                Ok(BrpResponse::Success(Box::new(BrpResult::Entities(
                    entities,
                ))))
            }
            BrpRequest::ListEntities { filter } => {
                let resolved_filter = self.resolve_query_filter(filter.as_ref()).await?;
                let mut entities = self.parse_query_result(&result)?;

                if let Some(filter) = resolved_filter {
                    if let Some(where_clause) = &filter.where_clause {
                        entities = self.apply_where_clause(entities, where_clause);
                    }
                }

                Ok(BrpResponse::Success(Box::new(BrpResult::Entities(
                    entities,
                ))))
            }
            BrpRequest::Get { entity, .. } => {
                let components = self.parse_components_from_get(&result)?;
                Ok(BrpResponse::Success(Box::new(BrpResult::Entity(
                    EntityData {
                        id: *entity,
                        components,
                    },
                ))))
            }
            BrpRequest::QueryEntity { entity_id } => {
                let components = self.parse_components_from_get(&result)?;
                Ok(BrpResponse::Success(Box::new(BrpResult::Entity(
                    EntityData {
                        id: *entity_id,
                        components,
                    },
                ))))
            }
            BrpRequest::ListComponents => {
                let component_types = self.parse_component_list(&result)?;
                self.component_type_cache = Some(component_types.clone());

                let component_infos = component_types
                    .into_iter()
                    .map(|type_name| ComponentTypeInfo {
                        id: type_name.clone(),
                        name: Self::short_component_name(&type_name),
                        schema: None,
                    })
                    .collect::<Vec<_>>();

                Ok(BrpResponse::Success(Box::new(BrpResult::ComponentTypes(
                    component_infos,
                ))))
            }
            BrpRequest::Spawn { .. } | BrpRequest::SpawnEntity { .. } => {
                let entity_id = self.parse_spawn_entity_id(&result)?;
                Ok(BrpResponse::Success(Box::new(BrpResult::EntitySpawned(
                    entity_id,
                ))))
            }
            BrpRequest::ModifyEntity { .. } => {
                Ok(BrpResponse::Success(Box::new(BrpResult::EntityModified)))
            }
            BrpRequest::Destroy { .. } | BrpRequest::DeleteEntity { .. } => {
                Ok(BrpResponse::Success(Box::new(BrpResult::EntityDeleted)))
            }
            BrpRequest::Set { .. }
            | BrpRequest::Insert { .. }
            | BrpRequest::Remove { .. }
            | BrpRequest::Reparent { .. } => Ok(BrpResponse::Success(Box::new(BrpResult::Success))),
            BrpRequest::Screenshot { .. } | BrpRequest::Debug { .. } => {
                Err(Error::Brp("Unsupported BRP response mapping".to_string()))
            }
        }
    }

    async fn get_component_types(&mut self) -> Result<Vec<String>> {
        if let Some(cached) = &self.component_type_cache {
            return Ok(cached.clone());
        }

        let response = self
            .send_http_jsonrpc("world.list_components", None)
            .await?;
        if let Some(error) = response.error {
            return Err(Error::Brp(format!(
                "Failed to list components {}: {}",
                error.code, error.message
            )));
        }

        let result = response
            .result
            .ok_or_else(|| Error::Brp("Missing component list result".to_string()))?;
        let component_types = self.parse_component_list(&result)?;
        self.component_type_cache = Some(component_types.clone());
        Ok(component_types)
    }

    async fn resolve_component_types(&mut self, components: &[String]) -> Result<Vec<String>> {
        if components.is_empty() {
            return Ok(Vec::new());
        }

        let known_types = match self.get_component_types().await {
            Ok(types) => types,
            Err(err) => {
                warn!("Component registry lookup failed: {}", err);
                return Ok(components.to_vec());
            }
        };

        let mut resolved = Vec::with_capacity(components.len());
        for component in components {
            if known_types.iter().any(|known| known == component) {
                resolved.push(component.clone());
                continue;
            }

            let suffix = format!("::{}", component);
            let matches = known_types
                .iter()
                .filter(|known| known.ends_with(&suffix) || known.ends_with(component))
                .collect::<Vec<_>>();

            if matches.is_empty() {
                resolved.push(component.clone());
            } else {
                if matches.len() > 1 {
                    warn!(
                        "Ambiguous component type '{}', using '{}'",
                        component, matches[0]
                    );
                }
                resolved.push(matches[0].to_string());
            }
        }

        Ok(resolved)
    }

    async fn resolve_component_map(
        &mut self,
        components: &HashMap<ComponentTypeId, ComponentValue>,
    ) -> Result<HashMap<ComponentTypeId, ComponentValue>> {
        if components.is_empty() {
            return Ok(HashMap::new());
        }

        let keys = components.keys().cloned().collect::<Vec<_>>();
        let resolved_keys = self.resolve_component_types(&keys).await?;

        let mut resolved = HashMap::with_capacity(components.len());
        for (original, resolved_key) in keys.iter().zip(resolved_keys.iter()) {
            if let Some(value) = components.get(original) {
                resolved.insert(resolved_key.clone(), value.clone());
            }
        }

        Ok(resolved)
    }

    async fn resolve_query_filter(
        &mut self,
        filter: Option<&QueryFilter>,
    ) -> Result<Option<QueryFilter>> {
        let Some(filter) = filter else {
            return Ok(None);
        };

        let mut resolved = filter.clone();
        if let Some(with) = &filter.with {
            resolved.with = Some(self.resolve_component_types(with).await?);
        }
        if let Some(without) = &filter.without {
            resolved.without = Some(self.resolve_component_types(without).await?);
        }
        if let Some(where_clause) = &filter.where_clause {
            let mut resolved_filters = Vec::with_capacity(where_clause.len());
            for clause in where_clause {
                let resolved_component = self
                    .resolve_component_types(std::slice::from_ref(&clause.component))
                    .await?;
                let mut resolved_clause = clause.clone();
                if let Some(component) = resolved_component.first() {
                    resolved_clause.component = component.clone();
                }
                resolved_filters.push(resolved_clause);
            }
            resolved.where_clause = Some(resolved_filters);
        }

        Ok(Some(resolved))
    }

    fn filter_to_json(filter: &QueryFilter) -> Option<Value> {
        let with = filter.with.clone().unwrap_or_default();
        let without = filter.without.clone().unwrap_or_default();

        if with.is_empty() && without.is_empty() {
            None
        } else {
            Some(json!({ "with": with, "without": without }))
        }
    }

    fn parse_component_list(&self, result: &Value) -> Result<Vec<String>> {
        let items = result
            .as_array()
            .ok_or_else(|| Error::Brp("Expected component list to be an array".to_string()))?;

        let mut types = Vec::with_capacity(items.len());
        for item in items {
            let component = item
                .as_str()
                .ok_or_else(|| Error::Brp("Component list entry must be a string".to_string()))?;
            types.push(component.to_string());
        }

        Ok(types)
    }

    fn parse_query_result(&self, result: &Value) -> Result<Vec<EntityData>> {
        let items = result
            .as_array()
            .ok_or_else(|| Error::Brp("Expected query result to be an array".to_string()))?;

        let mut entities = Vec::with_capacity(items.len());
        for item in items {
            let entity_value = item
                .get("entity")
                .ok_or_else(|| Error::Brp("Missing entity id in result".to_string()))?;
            let entity_id = self.parse_entity_id(entity_value)?;

            let components = item
                .get("components")
                .and_then(|value| value.as_object())
                .map(|map| {
                    map.iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect::<HashMap<_, _>>()
                })
                .unwrap_or_default();

            entities.push(EntityData {
                id: entity_id,
                components,
            });
        }

        Ok(entities)
    }

    fn parse_entity_id(&self, value: &Value) -> Result<u64> {
        if let Some(id) = value.as_u64() {
            return Ok(id);
        }
        if let Some(id) = value.as_i64() {
            return Ok(id as u64);
        }
        if let Some(text) = value.as_str() {
            return text
                .parse::<u64>()
                .map_err(|_| Error::Brp("Invalid entity id string".to_string()));
        }
        if let Some(obj) = value.as_object() {
            let index = obj.get("index").and_then(Value::as_u64);
            let generation = obj.get("generation").and_then(Value::as_u64);
            if let (Some(index), Some(generation)) = (index, generation) {
                return Ok((generation << 32) | index);
            }
        }

        Err(Error::Brp("Unsupported entity id format".to_string()))
    }

    fn parse_components_from_get(
        &self,
        result: &Value,
    ) -> Result<HashMap<ComponentTypeId, ComponentValue>> {
        let obj = result.as_object().ok_or_else(|| {
            Error::Brp("Expected get_components result to be an object".to_string())
        })?;

        if let Some(components_value) = obj.get("components") {
            if let Some(map) = components_value.as_object() {
                return Ok(map
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect());
            }
        }

        Ok(obj
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect())
    }

    fn parse_spawn_entity_id(&self, result: &Value) -> Result<u64> {
        if let Some(obj) = result.as_object() {
            if let Some(entity_value) = obj.get("entity").or_else(|| obj.get("entity_id")) {
                return self.parse_entity_id(entity_value);
            }
        }

        self.parse_entity_id(result)
    }

    fn short_component_name(full: &str) -> String {
        full.rsplit("::").next().unwrap_or(full).to_string()
    }

    fn apply_where_clause(
        &self,
        entities: Vec<EntityData>,
        filters: &[ComponentFilter],
    ) -> Vec<EntityData> {
        if filters.is_empty() {
            return entities;
        }

        entities
            .into_iter()
            .filter(|entity| self.entity_matches_filters(entity, filters))
            .collect()
    }

    fn entity_matches_filters(&self, entity: &EntityData, filters: &[ComponentFilter]) -> bool {
        filters.iter().all(|filter| {
            let component_value = match entity.components.get(&filter.component) {
                Some(value) => value,
                None => return false,
            };
            let target_value = match &filter.field {
                Some(path) => self.extract_path(component_value, path),
                None => Some(component_value),
            };
            let target_value = match target_value {
                Some(value) => value,
                None => return false,
            };
            self.compare_values(&filter.op, target_value, &filter.value)
        })
    }

    fn extract_path<'a>(&self, value: &'a Value, path: &str) -> Option<&'a Value> {
        let mut current = value;
        for segment in path.split('.') {
            if let Some(map) = current.as_object() {
                current = map.get(segment)?;
                continue;
            }
            if let Some(list) = current.as_array() {
                let index = segment.parse::<usize>().ok()?;
                current = list.get(index)?;
                continue;
            }
            return None;
        }
        Some(current)
    }

    fn compare_values(&self, op: &FilterOp, left: &Value, right: &Value) -> bool {
        match op {
            FilterOp::Equal => left == right,
            FilterOp::NotEqual => left != right,
            FilterOp::GreaterThan => match (left.as_f64(), right.as_f64()) {
                (Some(l), Some(r)) => l > r,
                _ => false,
            },
            FilterOp::GreaterThanOrEqual => match (left.as_f64(), right.as_f64()) {
                (Some(l), Some(r)) => l >= r,
                _ => false,
            },
            FilterOp::LessThan => match (left.as_f64(), right.as_f64()) {
                (Some(l), Some(r)) => l < r,
                _ => false,
            },
            FilterOp::LessThanOrEqual => match (left.as_f64(), right.as_f64()) {
                (Some(l), Some(r)) => l <= r,
                _ => false,
            },
            FilterOp::Contains => match left {
                Value::String(text) => right
                    .as_str()
                    .map(|needle| text.contains(needle))
                    .unwrap_or(false),
                Value::Array(values) => values.iter().any(|value| value == right),
                Value::Object(map) => right
                    .as_str()
                    .map(|needle| map.contains_key(needle))
                    .unwrap_or(false),
                _ => false,
            },
            FilterOp::Regex => match (left.as_str(), right.as_str()) {
                (Some(text), Some(pattern)) => Regex::new(pattern)
                    .map(|regex| regex.is_match(text))
                    .unwrap_or(false),
                _ => false,
            },
        }
    }

    fn next_request_id(&mut self) -> u64 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        id
    }

    /// Send a batched request (queued for batch processing)
    pub async fn send_batched_request(&mut self, request: BrpRequest) -> Result<BrpResponse> {
        let (response_tx, mut response_rx) = mpsc::channel(1);

        let batched_request = BatchedRequest {
            request,
            timestamp: Instant::now(),
            response_tx,
        };

        // Add to queue
        {
            let mut queue = self.request_queue.write().await;
            queue.push_back(batched_request);
        }

        // Wait for response
        response_rx
            .recv()
            .await
            .ok_or_else(|| Error::Connection("Batch response channel closed".to_string()))?
    }

    /// Start batch processing
    pub async fn start_batch_processing(&mut self) -> Result<()> {
        if self.batch_processor_handle.is_some() {
            return Ok(()); // Already running
        }

        let queue = self.request_queue.clone();
        let resource_manager = self.resource_manager.clone();

        let handle = tokio::spawn(async move {
            let mut batch_interval = interval(Duration::from_millis(50)); // Batch every 50ms

            loop {
                batch_interval.tick().await;

                // Process batched requests
                let requests = {
                    let mut queue_guard = queue.write().await;
                    let batch_size = std::cmp::min(queue_guard.len(), 10); // Max 10 per batch
                    queue_guard.drain(..batch_size).collect::<Vec<_>>()
                };

                if requests.is_empty() {
                    continue;
                }

                // Check resource limits before processing batch
                if let Some(ref rm) = resource_manager {
                    let rm_guard = rm.read().await;
                    if !rm_guard.check_brp_rate_limit().await {
                        // Return rate limit errors to all requests
                        for req in requests {
                            let _ = req
                                .response_tx
                                .send(Err(Error::Validation(
                                    "BRP rate limit exceeded".to_string(),
                                )))
                                .await;
                        }
                        continue;
                    }
                }

                info!("Processing batch of {} BRP requests", requests.len());

                // Process each request in the batch
                // For better efficiency, we process them individually but with shared resources
                for batched_request in requests {
                    // Simulate batch processing by adding a small delay and processing
                    let result = if let Some(ref rm) = resource_manager {
                        let rm_guard = rm.read().await;
                        if rm_guard.should_sample().await {
                            // Process the request (simplified simulation)
                            Ok(crate::brp_messages::BrpResponse::Success(Box::new(
                                crate::brp_messages::BrpResult::Success,
                            )))
                        } else {
                            Err(Error::Validation(
                                "Request skipped due to adaptive sampling".to_string(),
                            ))
                        }
                    } else {
                        // Fallback processing without resource management
                        Ok(crate::brp_messages::BrpResponse::Success(Box::new(
                            crate::brp_messages::BrpResult::Success,
                        )))
                    };

                    let _ = batched_request.response_tx.send(result).await;
                }
            }
        });

        self.batch_processor_handle = Some(handle);
        info!("Batch processing started");
        Ok(())
    }

    /// Stop batch processing
    pub async fn stop_batch_processing(&mut self) {
        if let Some(handle) = self.batch_processor_handle.take() {
            handle.abort();
            info!("Batch processing stopped");
        }
    }

    pub async fn send_message(&mut self, message: &str) -> Result<()> {
        if matches!(
            BrpTransport::from_url(&self.config.brp_url())?,
            BrpTransport::Http
        ) {
            return Err(Error::Connection(
                "HTTP transport does not support raw messaging".to_string(),
            ));
        }

        if let Some(ws_stream) = &mut self.ws_stream {
            ws_stream
                .send(Message::Text(message.to_string()))
                .await
                .map_err(|e| Error::WebSocket(Box::new(e)))?;
            debug!("Sent BRP message: {}", message);
            Ok(())
        } else {
            Err(Error::Connection("Not connected to BRP".to_string()))
        }
    }

    pub async fn receive_message(&mut self) -> Result<Option<String>> {
        if matches!(
            BrpTransport::from_url(&self.config.brp_url())?,
            BrpTransport::Http
        ) {
            return Err(Error::Connection(
                "HTTP transport does not support raw messaging".to_string(),
            ));
        }

        if let Some(ws_stream) = &mut self.ws_stream {
            match ws_stream.next().await {
                Some(Ok(Message::Text(text))) => {
                    debug!("Received BRP message: {}", text);
                    Ok(Some(text))
                }
                Some(Ok(Message::Close(_))) => {
                    warn!("BRP connection closed");
                    self.connected = false;
                    self.ws_stream = None;
                    Ok(None)
                }
                Some(Err(e)) => {
                    error!("BRP WebSocket error: {}", e);
                    self.connected = false;
                    self.ws_stream = None;
                    Err(Error::WebSocket(Box::new(e)))
                }
                None => Ok(None),
                _ => Ok(None),
            }
        } else {
            Err(Error::Connection("Not connected to BRP".to_string()))
        }
    }

    pub async fn disconnect(&mut self) {
        if matches!(
            BrpTransport::from_url(&self.config.brp_url()),
            Ok(BrpTransport::Http)
        ) {
            self.http_client = None;
            self.connected = false;
            info!("Disconnected from BRP");
            return;
        }

        if let Some(mut ws_stream) = self.ws_stream.take() {
            let _ = ws_stream.close(None).await;
        }
        self.connected = false;
        info!("Disconnected from BRP");
    }
}
