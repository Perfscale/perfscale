//! The `webrtc:` config gate, pro side of the line: once a handler for
//! `pro/webrtc-connect@v1` is registered (what the closed pro module does at
//! startup), a config declaring the block must parse cleanly.
//!
//! This is an integration test because the action registry is
//! process-global: the unit tests in `yaml.rs` cover the unregistered (OSS)
//! case, and one binary cannot test both states.

use std::sync::Arc;

use perfscale_core::step::actions::{register_action, ActionFuture, ActionHandler, ActionOutput};
use perfscale_core::step::context::Context;
use perfscale_core::yaml::parse_config_file;
use serde_json::Value;

struct StubWebrtc;

impl ActionHandler for StubWebrtc {
    fn matches(&self, action_id: &str) -> bool {
        action_id.starts_with("pro/webrtc-")
    }

    fn call<'a>(
        &'a self,
        _action_id: &'a str,
        _params: &'a Value,
        _ctx: &'a Context,
        _step_name: &'a str,
    ) -> ActionFuture<'a> {
        Box::pin(async {
            ActionOutput {
                value: Value::Null,
                logs: Vec::new(),
                success: true,
                http_sample: None,
            }
        })
    }
}

#[test]
fn webrtc_block_parses_once_the_pro_module_registers() {
    register_action(Arc::new(StubWebrtc));

    let yaml = r#"
vus: 4
webrtc:
  ice_servers:
    - urls: ["turn:turn.example.com:3478"]
      username: ${TURN_USER}
      credential: ${TURN_PASS}
  max_peer_connections: 500
"#;
    let cfg = parse_config_file(yaml).expect("registered pro module unlocks the block");
    let webrtc = cfg.webrtc.expect("webrtc section parsed");
    assert_eq!(webrtc.max_peer_connections, Some(500));
    let servers = webrtc.ice_servers.unwrap();
    assert_eq!(servers[0].urls, ["turn:turn.example.com:3478"]);
    assert_eq!(servers[0].username.as_deref(), Some("${TURN_USER}"));

    // A config without the block is unaffected either way.
    parse_config_file("vus: 1\n").unwrap();
}
