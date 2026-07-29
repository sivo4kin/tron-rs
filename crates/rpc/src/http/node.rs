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

/// `POST /wallet/listnodes` — discovered peers (empty until the discovery table
/// is populated by the live channel).
pub fn list_nodes() -> Value {
    json!({ "nodes": [] })
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
        assert_eq!(list_nodes()["nodes"].as_array().unwrap().len(), 0);
    }
}
