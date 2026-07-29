//! Node identity, peer, and witness-list endpoints (split from `http.rs`, P05).

use serde_json::{json, Value};

/// `POST /wallet/getnodeinfo` — static node identity/config (subset).
pub fn get_node_info(network: &str, p2p_port: u16) -> Value {
    json!({
        "configNodeInfo": {
            "codeVersion": env!("CARGO_PKG_VERSION"),
            "p2pVersion": network,
            "listenPort": p2p_port,
        },
        "solidityBlock": "",
        "activeConnectCount": 0,
    })
}

/// `POST /wallet/listnodes` — peers known to the discovery service. Each entry is
/// `{ "address": { "host": <ip>, "port": <tcp port> } }` (java-tron `NodeList`
/// shape). Empty until the discovery service populates the peer table.
pub fn list_nodes(nodes: &[(String, u16)]) -> Value {
    let nodes: Vec<Value> = nodes
        .iter()
        .map(|(host, port)| json!({ "address": { "host": host, "port": port } }))
        .collect();
    json!({ "nodes": nodes })
}

/// `POST /wallet/listwitnesses` — the witness (SR) list (empty until the witness
/// store is enumerated).
pub fn list_witnesses() -> Value {
    json!({ "witnesses": [] })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_info_and_list_nodes_shape() {
        let info = get_node_info("nile", 18888);
        assert_eq!(info["configNodeInfo"]["listenPort"], 18888);
        assert!(info["configNodeInfo"]["codeVersion"].is_string());
        assert_eq!(list_nodes(&[])["nodes"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn list_nodes_reports_discovered_peers() {
        let peers = vec![("1.2.3.4".to_string(), 18888u16), ("5.6.7.8".to_string(), 18889u16)];
        let v = list_nodes(&peers);
        let arr = v["nodes"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["address"]["host"], "1.2.3.4");
        assert_eq!(arr[0]["address"]["port"], 18888);
        assert_eq!(arr[1]["address"]["port"], 18889);
    }
}
