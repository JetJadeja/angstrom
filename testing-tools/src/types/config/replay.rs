use crate::types::{GlobalTestingConfig, initial_state::InitialStateConfig};

#[derive(Debug, Clone)]
pub struct ReplayConfig {
    /// A testnet replay will be replayed on a local chain prepared with the
    /// testnet configuration files associated with our runner.  If this is
    /// false, this is a "live" replay that weill be replayed on a specified
    /// chain forked to a local anvil
    initial_state:   InitialStateConfig,
    testnet_replay:  bool,
    eth_fork_url:    String,
    seed:            u16,
    leader_rpc_port: u16
}

impl ReplayConfig {
    pub fn new(
        initial_state: InitialStateConfig,
        testnet_replay: bool,
        eth_fork_url: String
    ) -> Self {
        let seed = rand::random();
        let leader_rpc_port = rand::random();
        Self { initial_state, testnet_replay, eth_fork_url, seed, leader_rpc_port }
    }

    pub fn testnet_replay(&self) -> bool {
        self.testnet_replay
    }
}

impl GlobalTestingConfig for ReplayConfig {
    fn is_leader(&self, node_id: u64) -> bool {
        true
    }

    fn anvil_rpc_endpoint(&self, _: u64) -> String {
        format!("/tmp/testnet_anvil_{}.ipc", self.seed)
    }

    fn base_angstrom_rpc_port(&self) -> u16 {
        unreachable!()
    }

    fn node_count(&self) -> u64 {
        1
    }

    fn leader_eth_rpc_port(&self) -> u16 {
        self.leader_rpc_port
    }

    fn config_type(&self) -> super::TestingConfigKind {
        super::TestingConfigKind::Replay
    }

    fn initial_state_config(&self) -> InitialStateConfig {
        self.initial_state.clone()
    }

    fn eth_ws_url(&self) -> String {
        self.eth_fork_url.clone()
    }

    fn fork_config(&self) -> Option<(u64, String)> {
        None
    }
}
