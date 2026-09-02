use iii_sdk::errors::Error;
use iii_sdk::{IIIClient, RegisterFunction, protocol::TriggerRequest, register_worker};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

mod types;

use types::{
    ChainRequest, FindByCapabilityRequest, HierarchyNode, SetHierarchyRequest, TreeNode,
    TreeRequest,
};

fn scope(realm_id: &str) -> String {
    format!("realm:{realm_id}:hierarchy")
}

async fn set_node(iii: &IIIClient, req: SetHierarchyRequest) -> Result<Value, Error> {
    let node = HierarchyNode {
        agent_id: req.agent_id.clone(),
        realm_id: req.realm_id.clone(),
        reports_to: req.reports_to,
        title: req.title,
        capabilities: req.capabilities.unwrap_or_default(),
        rank: req.rank.unwrap_or(0),
    };

    if let Some(ref parent) = node.reports_to {
        if parent == &node.agent_id {
            return Err(Error::Handler("agent cannot report to itself".into()));
        }

        let all_nodes = load_all(iii, &req.realm_id).await?;
        if would_create_cycle(&all_nodes, &node.agent_id, parent) {
            return Err(Error::Handler("cycle detected in hierarchy".into()));
        }
    }

    let value = serde_json::to_value(&node).map_err(|e| Error::Handler(e.to_string()))?;

    iii.trigger(TriggerRequest {
        function_id: "state::set".to_string(),
        payload: json!({
            "scope": scope(&req.realm_id),
            "key": node.agent_id,
            "value": value,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    Ok(serde_json::to_value(&node).unwrap())
}

fn would_create_cycle(nodes: &[HierarchyNode], agent_id: &str, new_parent: &str) -> bool {
    let parent_map: HashMap<&str, &str> = nodes
        .iter()
        .filter_map(|n| n.reports_to.as_deref().map(|p| (n.agent_id.as_str(), p)))
        .collect();

    let mut visited = HashSet::new();
    let mut current = new_parent;

    loop {
        if current == agent_id {
            return true;
        }
        if !visited.insert(current) {
            return true;
        }
        match parent_map.get(current) {
            Some(&parent) => current = parent,
            None => return false,
        }
    }
}

async fn load_all(iii: &IIIClient, realm_id: &str) -> Result<Vec<HierarchyNode>, Error> {
    let result = iii
        .trigger(TriggerRequest {
            function_id: "state::list".to_string(),
            payload: json!({ "scope": scope(realm_id) }),
            action: None,
            timeout_ms: None,
        })
        .await
        .map_err(|e| Error::Handler(e.to_string()))?;

    let nodes: Vec<HierarchyNode> = if let Some(arr) = result.as_array() {
        arr.iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect()
    } else {
        vec![]
    };

    Ok(nodes)
}

async fn get_tree(iii: &IIIClient, req: TreeRequest) -> Result<Value, Error> {
    let nodes = load_all(iii, &req.realm_id).await?;

    let children_map: HashMap<Option<&str>, Vec<&HierarchyNode>> = {
        let mut m: HashMap<Option<&str>, Vec<&HierarchyNode>> = HashMap::new();
        for n in &nodes {
            m.entry(n.reports_to.as_deref()).or_default().push(n);
        }
        m
    };

    fn build_tree<'a>(
        agent_id: &str,
        nodes: &'a [HierarchyNode],
        children_map: &HashMap<Option<&str>, Vec<&'a HierarchyNode>>,
        visited: &mut HashSet<String>,
    ) -> TreeNode {
        let node = nodes.iter().find(|n| n.agent_id == agent_id);
        let title = node.and_then(|n| n.title.clone());
        let caps = node.map(|n| n.capabilities.clone()).unwrap_or_default();
        let rank = node.map(|n| n.rank).unwrap_or(0);

        let reports = if visited.insert(agent_id.to_string()) {
            children_map
                .get(&Some(agent_id))
                .map(|children| {
                    children
                        .iter()
                        .map(|c| build_tree(&c.agent_id, nodes, children_map, visited))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            vec![]
        };

        TreeNode {
            agent_id: agent_id.to_string(),
            title,
            capabilities: caps,
            rank,
            reports,
        }
    }

    if let Some(root_id) = &req.root_agent_id {
        let mut visited = HashSet::new();
        let tree = build_tree(root_id, &nodes, &children_map, &mut visited);
        Ok(serde_json::to_value(tree).unwrap())
    } else {
        let roots: Vec<TreeNode> = children_map
            .get(&None)
            .map(|root_nodes| {
                let mut visited = HashSet::new();
                root_nodes
                    .iter()
                    .map(|n| build_tree(&n.agent_id, &nodes, &children_map, &mut visited))
                    .collect()
            })
            .unwrap_or_default();

        Ok(json!({ "roots": roots }))
    }
}

async fn find_by_capability(iii: &IIIClient, req: FindByCapabilityRequest) -> Result<Value, Error> {
    let nodes = load_all(iii, &req.realm_id).await?;
    let matches: Vec<&HierarchyNode> = nodes
        .iter()
        .filter(|n| {
            n.capabilities
                .iter()
                .any(|c| c.eq_ignore_ascii_case(&req.capability))
        })
        .collect();

    Ok(json!({
        "matches": matches,
        "count": matches.len(),
    }))
}

async fn get_chain(iii: &IIIClient, req: ChainRequest) -> Result<Value, Error> {
    let nodes = load_all(iii, &req.realm_id).await?;
    let node_map: HashMap<&str, &HierarchyNode> =
        nodes.iter().map(|n| (n.agent_id.as_str(), n)).collect();

    let mut chain = vec![];
    let mut current = req.agent_id.as_str();
    let mut visited = HashSet::new();

    loop {
        if !visited.insert(current) {
            break;
        }
        if let Some(node) = node_map.get(current) {
            chain.push(serde_json::to_value(*node).unwrap());
            match &node.reports_to {
                Some(parent) => current = parent.as_str(),
                None => break,
            }
        } else {
            break;
        }
    }

    Ok(json!({ "chain": chain }))
}

async fn remove_node(iii: &IIIClient, realm_id: &str, agent_id: &str) -> Result<Value, Error> {
    iii.trigger(TriggerRequest {
        function_id: "state::delete".to_string(),
        payload: json!({
            "scope": scope(realm_id),
            "key": agent_id,
        }),
        action: None,
        timeout_ms: None,
    })
    .await
    .map_err(|e| Error::Handler(e.to_string()))?;

    Ok(json!({ "removed": true }))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let ws_url = std::env::var("III_URL").unwrap_or_else(|_| "ws://localhost:49134".to_string());
    let iii = register_worker(&ws_url, agentos_bus_auth::init_options());

    let iii_clone = iii.clone();
    iii.register_function(
        "hierarchy::set",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: SetHierarchyRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                set_node(&iii, req).await
            }
        })
        .description("Set agent position in hierarchy"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "hierarchy::tree",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: TreeRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                get_tree(&iii, req).await
            }
        })
        .description("Get full org tree for a realm"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "hierarchy::find",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: FindByCapabilityRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                find_by_capability(&iii, req).await
            }
        })
        .description("Find agents by capability"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "hierarchy::chain",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let req: ChainRequest =
                    serde_json::from_value(input).map_err(|e| Error::Handler(e.to_string()))?;
                get_chain(&iii, req).await
            }
        })
        .description("Get chain of command for an agent"),
    );

    let iii_clone = iii.clone();
    iii.register_function(
        "hierarchy::remove",
        RegisterFunction::new_async(move |input: Value| {
            let iii = iii_clone.clone();
            async move {
                let realm_id = input["realmId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing realmId".into()))?;
                let agent_id = input["agentId"]
                    .as_str()
                    .ok_or_else(|| Error::Handler("missing agentId".into()))?;
                remove_node(&iii, realm_id, agent_id).await
            }
        })
        .description("Remove agent from hierarchy"),
    );

    agentos_http_adapter::register_http_trigger(
        &iii,
        "hierarchy::set".to_string(),
        json!({ "http_method": "POST", "api_path": "api/hierarchy" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "hierarchy::tree".to_string(),
        json!({ "http_method": "GET", "api_path": "api/hierarchy/:realmId/tree" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "hierarchy::find".to_string(),
        json!({ "http_method": "GET", "api_path": "api/hierarchy/:realmId/find" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "hierarchy::chain".to_string(),
        json!({ "http_method": "GET", "api_path": "api/hierarchy/:realmId/chain/:agentId" }),
        None,
    )?;
    agentos_http_adapter::register_http_trigger(
        &iii,
        "hierarchy::remove".to_string(),
        json!({ "http_method": "DELETE", "api_path": "api/hierarchy/:realmId/:agentId" }),
        None,
    )?;

    tracing::info!("hierarchy worker started");
    tokio::signal::ctrl_c().await?;
    iii.shutdown_async().await;
    Ok(())
}
