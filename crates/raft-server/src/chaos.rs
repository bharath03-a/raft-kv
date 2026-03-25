/// Fault-injection configuration for the transport layer.
///
/// When `ChaosConfig::none()` is used (the production default), all drop
/// probabilities are 0.0 and the hot path in the transport never calls
/// `rand::random`, keeping the overhead at a single float comparison.
#[derive(Debug, Clone)]
pub struct ChaosConfig {
    /// Probability [0.0, 1.0] that any outbound peer Raft message is silently
    /// dropped before it reaches the TCP send buffer.
    pub outbound_drop_rate: f64,
    /// Probability [0.0, 1.0] that any inbound peer Raft message is silently
    /// dropped after being decoded but before being forwarded to the node.
    pub inbound_drop_rate: f64,
}

impl ChaosConfig {
    /// No message drops — the production default.
    pub const fn none() -> Self {
        Self {
            outbound_drop_rate: 0.0,
            inbound_drop_rate: 0.0,
        }
    }

    /// 20 % outbound and inbound message drops.
    ///
    /// Used by the `chaos_message_loss_cluster_converges` integration test to
    /// verify that the cluster still elects a leader and commits all writes
    /// despite sustained message loss.
    pub fn twenty_percent() -> Self {
        Self {
            outbound_drop_rate: 0.20,
            inbound_drop_rate: 0.20,
        }
    }

    /// Returns `true` if this outbound message should be dropped.
    ///
    /// Uses a thread-local RNG. Safe to call from any tokio worker thread.
    #[inline]
    pub fn should_drop_outbound(&self) -> bool {
        self.outbound_drop_rate > 0.0 && rand::random::<f64>() < self.outbound_drop_rate
    }

    /// Returns `true` if this inbound message should be dropped.
    #[inline]
    pub fn should_drop_inbound(&self) -> bool {
        self.inbound_drop_rate > 0.0 && rand::random::<f64>() < self.inbound_drop_rate
    }
}

impl Default for ChaosConfig {
    fn default() -> Self {
        Self::none()
    }
}
