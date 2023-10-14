//! Implementation of a global context mock. Used in testing handlers of all IBC modules.

mod clients;

use alloc::collections::btree_map::BTreeMap;
use alloc::sync::Arc;
use core::cmp::min;
use core::fmt::Debug;
use core::ops::{Add, Sub};
use core::time::Duration;

use derive_more::{From, TryInto};
use ibc_proto::google::protobuf::Any;
use ibc_proto::protobuf::Protobuf;
use parking_lot::Mutex;
use tendermint_testgen::Validator as TestgenValidator;
use tracing::debug;
use typed_builder::TypedBuilder;

use self::clients::TmClientStateConfig;
use super::client_state::{MOCK_CLIENT_STATE_TYPE_URL, MOCK_CLIENT_TYPE};
use super::consensus_state::MOCK_CONSENSUS_STATE_TYPE_URL;
use crate::clients::ics07_tendermint::client_state::{
    ClientState as TmClientState, TENDERMINT_CLIENT_STATE_TYPE_URL,
};
use crate::clients::ics07_tendermint::consensus_state::{
    ConsensusState as TmConsensusState, TENDERMINT_CONSENSUS_STATE_TYPE_URL,
};
use crate::clients::ics07_tendermint::TENDERMINT_CLIENT_TYPE;
use crate::core::events::IbcEvent;
use crate::core::ics02_client::client_state::ClientState;
use crate::core::ics02_client::client_type::ClientType;
use crate::core::ics02_client::consensus_state::ConsensusState;
use crate::core::ics02_client::error::ClientError;
use crate::core::ics03_connection::connection::ConnectionEnd;
use crate::core::ics03_connection::error::ConnectionError;
use crate::core::ics04_channel::channel::ChannelEnd;
use crate::core::ics04_channel::commitment::{AcknowledgementCommitment, PacketCommitment};
use crate::core::ics04_channel::error::{ChannelError, PacketError};
use crate::core::ics04_channel::packet::{Receipt, Sequence};
use crate::core::ics23_commitment::commitment::CommitmentPrefix;
use crate::core::ics24_host::identifier::{ChainId, ChannelId, ClientId, ConnectionId, PortId};
use crate::core::ics24_host::path::{
    AckPath, ChannelEndPath, ClientConnectionPath, ClientConsensusStatePath, CommitmentPath,
    ConnectionPath, ReceiptPath, SeqAckPath, SeqRecvPath, SeqSendPath,
};
use crate::core::router::Router;
use crate::core::timestamp::Timestamp;
use crate::core::{dispatch, ContextError, ExecutionContext, MsgEnvelope, ValidationContext};
use crate::mock::client_state::{client_type as mock_client_type, MockClientState};
use crate::mock::consensus_state::MockConsensusState;
use crate::mock::header::MockHeader;
use crate::mock::host::{HostBlock, HostType};
use crate::mock::ics18_relayer::context::RelayerContext;
use crate::mock::ics18_relayer::error::RelayerError;
use crate::prelude::*;
use crate::signer::Signer;
use crate::Height;

pub const DEFAULT_BLOCK_TIME_SECS: u64 = 3;

#[derive(Debug, Clone, From, PartialEq, ClientState)]
#[generics(ClientValidationContext = MockContext,
           ClientExecutionContext = MockContext)
]
#[mock]
pub enum AnyClientState {
    Tendermint(TmClientState),
    Mock(MockClientState),
}

impl Protobuf<Any> for AnyClientState {}

impl TryFrom<Any> for AnyClientState {
    type Error = ClientError;

    fn try_from(raw: Any) -> Result<Self, Self::Error> {
        if raw.type_url == TENDERMINT_CLIENT_STATE_TYPE_URL {
            TmClientState::try_from(raw).map(Into::into)
        } else if raw.type_url == MOCK_CLIENT_STATE_TYPE_URL {
            MockClientState::try_from(raw).map(Into::into)
        } else {
            Err(ClientError::Other {
                description: "failed to deserialize message".to_string(),
            })
        }
    }
}

impl From<AnyClientState> for Any {
    fn from(host_client_state: AnyClientState) -> Self {
        match host_client_state {
            AnyClientState::Tendermint(cs) => cs.into(),
            AnyClientState::Mock(cs) => cs.into(),
        }
    }
}

#[derive(Debug, Clone, From, TryInto, PartialEq, ConsensusState)]
pub enum AnyConsensusState {
    Tendermint(TmConsensusState),
    Mock(MockConsensusState),
}

impl Protobuf<Any> for AnyConsensusState {}

impl TryFrom<Any> for AnyConsensusState {
    type Error = ClientError;

    fn try_from(raw: Any) -> Result<Self, Self::Error> {
        if raw.type_url == TENDERMINT_CONSENSUS_STATE_TYPE_URL {
            TmConsensusState::try_from(raw).map(Into::into)
        } else if raw.type_url == MOCK_CONSENSUS_STATE_TYPE_URL {
            MockConsensusState::try_from(raw).map(Into::into)
        } else {
            Err(ClientError::Other {
                description: "failed to deserialize message".to_string(),
            })
        }
    }
}

impl From<AnyConsensusState> for Any {
    fn from(host_consensus_state: AnyConsensusState) -> Self {
        match host_consensus_state {
            AnyConsensusState::Tendermint(cs) => cs.into(),
            AnyConsensusState::Mock(cs) => cs.into(),
        }
    }
}

/// A mock of an IBC client record as it is stored in a mock context.
/// For testing ICS02 handlers mostly, cf. `MockClientContext`.
#[derive(Clone, Debug)]
pub struct MockClientRecord {
    /// The client state (representing only the latest height at the moment).
    pub client_state: Option<AnyClientState>,

    /// Mapping of heights to consensus states for this client.
    pub consensus_states: BTreeMap<Height, AnyConsensusState>,
}

/// An object that stores all IBC related data.
#[derive(Clone, Debug, Default)]
pub struct MockIbcStore {
    /// The set of all clients, indexed by their id.
    pub clients: BTreeMap<ClientId, MockClientRecord>,

    /// Tracks the processed time for clients header updates
    pub client_processed_times: BTreeMap<(ClientId, Height), Timestamp>,

    /// Tracks the processed height for the clients
    pub client_processed_heights: BTreeMap<(ClientId, Height), Height>,

    /// Counter for the client identifiers, necessary for `increase_client_counter` and the
    /// `client_counter` methods.
    pub client_ids_counter: u64,

    /// Association between client ids and connection ids.
    pub client_connections: BTreeMap<ClientId, ConnectionId>,

    /// All the connections in the store.
    pub connections: BTreeMap<ConnectionId, ConnectionEnd>,

    /// Counter for connection identifiers (see `increase_connection_counter`).
    pub connection_ids_counter: u64,

    /// Association between connection ids and channel ids.
    pub connection_channels: BTreeMap<ConnectionId, Vec<(PortId, ChannelId)>>,

    /// Counter for channel identifiers (see `increase_channel_counter`).
    pub channel_ids_counter: u64,

    /// All the channels in the store. TODO Make new key PortId X ChannelId
    pub channels: PortChannelIdMap<ChannelEnd>,

    /// Tracks the sequence number for the next packet to be sent.
    pub next_sequence_send: PortChannelIdMap<Sequence>,

    /// Tracks the sequence number for the next packet to be received.
    pub next_sequence_recv: PortChannelIdMap<Sequence>,

    /// Tracks the sequence number for the next packet to be acknowledged.
    pub next_sequence_ack: PortChannelIdMap<Sequence>,

    pub packet_acknowledgement: PortChannelIdMap<BTreeMap<Sequence, AcknowledgementCommitment>>,

    /// Constant-size commitments to packets data fields
    pub packet_commitment: PortChannelIdMap<BTreeMap<Sequence, PacketCommitment>>,

    // Used by unordered channel
    pub packet_receipt: PortChannelIdMap<BTreeMap<Sequence, Receipt>>,
}

/// Config builder for MockContext.
#[derive(Debug, TypedBuilder)]
#[builder(build_method(into = MockContext))]
pub struct MockContextConfig {
    #[builder(default = HostType::Mock)]
    host_type: HostType,

    host_id: ChainId,

    #[builder(default = Duration::from_secs(DEFAULT_BLOCK_TIME_SECS))]
    block_time: Duration,

    #[builder(default = 5)]
    max_history_size: usize,

    latest_height: Height,

    #[builder(default = Timestamp::now())]
    latest_timestamp: Timestamp,
}

/// Config builder for MockClientConfig.
#[derive(Debug, TypedBuilder)]
pub struct MockClientConfig {
    client_chain_id: ChainId,
    client_id: ClientId,
    #[builder(default = mock_client_type())]
    client_type: ClientType,
    client_state_height: Height,
    #[builder(default)]
    consensus_state_heights: Vec<Height>,
    #[builder(default = Timestamp::now())]
    latest_timestamp: Timestamp,
}

/// A context implementing the dependencies necessary for testing any IBC module.
#[derive(Debug)]
pub struct MockContext {
    /// The type of host chain underlying this mock context.
    host_chain_type: HostType,

    /// Host chain identifier.
    host_chain_id: ChainId,

    /// Maximum size for the history of the host chain. Any block older than this is pruned.
    max_history_size: usize,

    /// The chain of blocks underlying this context. A vector of size up to `max_history_size`
    /// blocks, ascending order by their height (latest block is on the last position).
    history: Vec<HostBlock>,

    /// Average time duration between blocks
    block_time: Duration,

    /// An object that stores all IBC related data.
    pub ibc_store: Arc<Mutex<MockIbcStore>>,

    pub events: Vec<IbcEvent>,

    pub logs: Vec<String>,
}

/// Returns a MockContext with bare minimum initialization: no clients, no connections and no channels are
/// present, and the chain has Height(5). This should be used sparingly, mostly for testing the
/// creation of new domain objects.
impl Default for MockContext {
    fn default() -> Self {
        Self::new(
            ChainId::new("mockgaia", 0).expect("Never fails"),
            HostType::Mock,
            5,
            Height::new(0, 5).expect("Never fails"),
        )
    }
}

/// A manual clone impl is provided because the tests are oblivious to the fact that the `ibc_store`
/// is a shared ptr.
impl Clone for MockContext {
    fn clone(&self) -> Self {
        let ibc_store = {
            let ibc_store = self.ibc_store.lock().clone();
            Arc::new(Mutex::new(ibc_store))
        };

        Self {
            host_chain_type: self.host_chain_type,
            host_chain_id: self.host_chain_id.clone(),
            max_history_size: self.max_history_size,
            history: self.history.clone(),
            block_time: self.block_time,
            ibc_store,
            events: self.events.clone(),
            logs: self.logs.clone(),
        }
    }
}

impl From<MockContextConfig> for MockContext {
    fn from(params: MockContextConfig) -> Self {
        assert_ne!(
            params.max_history_size, 0,
            "The chain must have a non-zero max_history_size"
        );

        assert_ne!(
            params.latest_height.revision_height(),
            0,
            "The chain must have a non-zero revision_height"
        );

        // Compute the number of blocks to store.
        let n = min(
            params.max_history_size as u64,
            params.latest_height.revision_height(),
        );

        assert_eq!(
            params.host_id.revision_number(),
            params.latest_height.revision_number(),
            "The version in the chain identifier must match the version in the latest height"
        );

        let next_block_timestamp = params
            .latest_timestamp
            .add(params.block_time)
            .expect("Never fails");

        MockContext {
            host_chain_type: params.host_type,
            host_chain_id: params.host_id.clone(),
            max_history_size: params.max_history_size,
            history: (0..n)
                .rev()
                .map(|i| {
                    // generate blocks with timestamps -> N, N - BT, N - 2BT, ...
                    // where N = now(), BT = block_time
                    HostBlock::generate_block(
                        params.host_id.clone(),
                        params.host_type,
                        params
                            .latest_height
                            .sub(i)
                            .expect("Never fails")
                            .revision_height(),
                        next_block_timestamp
                            .sub(params.block_time * ((i + 1) as u32))
                            .expect("Never fails"),
                    )
                })
                .collect(),
            block_time: params.block_time,
            ibc_store: Arc::new(Mutex::new(MockIbcStore::default())),
            events: Vec::new(),
            logs: Vec::new(),
        }
    }
}

/// Implementation of internal interface for use in testing. The methods in this interface should
/// _not_ be accessible to any Ics handler.
impl MockContext {
    /// Creates a mock context. Parameter `max_history_size` determines how many blocks will
    /// the chain maintain in its history, which also determines the pruning window. Parameter
    /// `latest_height` determines the current height of the chain. This context
    /// has support to emulate two type of underlying chains: Mock or SyntheticTendermint.
    pub fn new(
        host_id: ChainId,
        host_type: HostType,
        max_history_size: usize,
        latest_height: Height,
    ) -> Self {
        assert_ne!(
            max_history_size, 0,
            "The chain must have a non-zero max_history_size"
        );

        assert_ne!(
            latest_height.revision_height(),
            0,
            "The chain must have a non-zero revision_height"
        );

        // Compute the number of blocks to store.
        let n = min(max_history_size as u64, latest_height.revision_height());

        assert_eq!(
            host_id.revision_number(),
            latest_height.revision_number(),
            "The version in the chain identifier must match the version in the latest height"
        );

        let block_time = Duration::from_secs(DEFAULT_BLOCK_TIME_SECS);
        let next_block_timestamp = Timestamp::now().add(block_time).expect("Never fails");
        MockContext {
            host_chain_type: host_type,
            host_chain_id: host_id.clone(),
            max_history_size,
            history: (0..n)
                .rev()
                .map(|i| {
                    // generate blocks with timestamps -> N, N - BT, N - 2BT, ...
                    // where N = now(), BT = block_time
                    HostBlock::generate_block(
                        host_id.clone(),
                        host_type,
                        latest_height.sub(i).expect("Never fails").revision_height(),
                        next_block_timestamp
                            .sub(Duration::from_secs(DEFAULT_BLOCK_TIME_SECS * (i + 1)))
                            .expect("Never fails"),
                    )
                })
                .collect(),
            block_time,
            ibc_store: Arc::new(Mutex::new(MockIbcStore::default())),
            events: Vec::new(),
            logs: Vec::new(),
        }
    }

    /// Same as [Self::new] but with custom validator sets for each block.
    /// Note: the validator history is used accordingly for current validator set and next validator set.
    /// `validator_history[i]` and `validator_history[i+1]` is i'th block's current and next validator set.
    /// The number of blocks will be `validator_history.len() - 1` due to the above.
    pub fn new_with_validator_history(
        host_id: ChainId,
        host_type: HostType,
        validator_history: &[Vec<TestgenValidator>],
        latest_height: Height,
    ) -> Self {
        let max_history_size = validator_history.len() - 1;

        assert_ne!(
            max_history_size, 0,
            "The chain must have a non-zero max_history_size"
        );

        assert_ne!(
            latest_height.revision_height(),
            0,
            "The chain must have a non-zero revision_height"
        );

        assert!(
            max_history_size as u64 <= latest_height.revision_height(),
            "The number of blocks must be greater than the number of validator set histories"
        );

        assert_eq!(
            host_id.revision_number(),
            latest_height.revision_number(),
            "The version in the chain identifier must match the version in the latest height"
        );

        let block_time = Duration::from_secs(DEFAULT_BLOCK_TIME_SECS);
        let next_block_timestamp = Timestamp::now().add(block_time).expect("Never fails");

        let history = (0..max_history_size)
            .rev()
            .map(|i| {
                // generate blocks with timestamps -> N, N - BT, N - 2BT, ...
                // where N = now(), BT = block_time
                HostBlock::generate_block_with_validators(
                    host_id.clone(),
                    host_type,
                    latest_height
                        .sub(i as u64)
                        .expect("Never fails")
                        .revision_height(),
                    next_block_timestamp
                        .sub(Duration::from_secs(
                            DEFAULT_BLOCK_TIME_SECS * (i as u64 + 1),
                        ))
                        .expect("Never fails"),
                    &validator_history[max_history_size - 1 - i],
                    &validator_history[max_history_size - i],
                )
            })
            .collect();

        MockContext {
            host_chain_type: host_type,
            host_chain_id: host_id.clone(),
            max_history_size,
            history,
            block_time,
            ibc_store: Arc::new(Mutex::new(MockIbcStore::default())),
            events: Vec::new(),
            logs: Vec::new(),
        }
    }

    /// Associates a client record to this context.
    /// Given a client id and a height, registers a new client in the context and also associates
    /// to this client a mock client state and a mock consensus state for height `height`. The type
    /// of this client is implicitly assumed to be Mock.
    pub fn with_client(self, client_id: &ClientId, height: Height) -> Self {
        self.with_client_parametrized(client_id, height, Some(mock_client_type()), Some(height))
    }

    /// Similar to `with_client`, this function associates a client record to this context, but
    /// additionally permits to parametrize two details of the client. If `client_type` is None,
    /// then the client will have type Mock, otherwise the specified type. If
    /// `consensus_state_height` is None, then the client will be initialized with a consensus
    /// state matching the same height as the client state (`client_state_height`).
    pub fn with_client_parametrized(
        self,
        client_id: &ClientId,
        client_state_height: Height,
        client_type: Option<ClientType>,
        consensus_state_height: Option<Height>,
    ) -> Self {
        // NOTE: this is wrong; the client chain ID is supposed to represent
        // the chain ID of the counterparty chain. But at this point this is
        // too ingrained in our tests; `with_client()` is called everywhere,
        // which delegates to this.
        let client_chain_id = self.host_chain_id.clone();

        self.with_client_parametrized_with_chain_id(
            client_chain_id,
            client_id,
            client_state_height,
            client_type,
            consensus_state_height,
        )
    }

    pub fn with_client_parametrized_with_chain_id(
        self,
        client_chain_id: ChainId,
        client_id: &ClientId,
        client_state_height: Height,
        client_type: Option<ClientType>,
        consensus_state_height: Option<Height>,
    ) -> Self {
        let cs_height = consensus_state_height.unwrap_or(client_state_height);

        let client_type = client_type.unwrap_or_else(mock_client_type);
        let (client_state, consensus_state) = if client_type.as_str() == MOCK_CLIENT_TYPE {
            (
                Some(MockClientState::new(MockHeader::new(client_state_height)).into()),
                MockConsensusState::new(MockHeader::new(cs_height)).into(),
            )
        } else if client_type.as_str() == TENDERMINT_CLIENT_TYPE {
            let light_block = HostBlock::generate_tm_block(
                client_chain_id,
                cs_height.revision_height(),
                Timestamp::now(),
            );

            let client_state =
                TmClientState::new_dummy_from_header(light_block.header().clone()).into();

            // Return the tuple.
            (Some(client_state), light_block.into())
        } else {
            panic!("unknown client type")
        };
        // If it's a mock client, create the corresponding mock states.
        let consensus_states = vec![(cs_height, consensus_state)].into_iter().collect();

        debug!("consensus states: {:?}", consensus_states);

        let client_record = MockClientRecord {
            client_state,
            consensus_states,
        };
        self.ibc_store
            .lock()
            .clients
            .insert(client_id.clone(), client_record);
        self
    }

    pub fn with_client_parametrized_history(
        self,
        client_id: &ClientId,
        client_state_height: Height,
        client_type: Option<ClientType>,
        consensus_state_height: Option<Height>,
    ) -> Self {
        let client_chain_id = self.host_chain_id.clone();
        self.with_client_parametrized_history_with_chain_id(
            client_chain_id,
            client_id,
            client_state_height,
            client_type,
            consensus_state_height,
        )
    }

    pub(crate) fn with_client_parametrized_history_with_chain_id(
        self,
        client_chain_id: ChainId,
        client_id: &ClientId,
        client_state_height: Height,
        client_type: Option<ClientType>,
        consensus_state_height: Option<Height>,
    ) -> Self {
        let cs_height = consensus_state_height.unwrap_or(client_state_height);
        let prev_cs_height = cs_height.clone().sub(1).unwrap_or(client_state_height);

        let client_type = client_type.unwrap_or_else(mock_client_type);
        let now = Timestamp::now();

        let (client_state, consensus_state): (Option<AnyClientState>, AnyConsensusState) =
            if client_type.as_str() == MOCK_CLIENT_TYPE {
                // If it's a mock client, create the corresponding mock states.
                (
                    Some(MockClientState::new(MockHeader::new(client_state_height)).into()),
                    MockConsensusState::new(MockHeader::new(cs_height)).into(),
                )
            } else if client_type.as_str() == TENDERMINT_CLIENT_TYPE {
                // If it's a Tendermint client, we need TM states.
                let light_block =
                    HostBlock::generate_tm_block(client_chain_id, cs_height.revision_height(), now);

                let client_state =
                    TmClientState::new_dummy_from_header(light_block.header().clone()).into();

                // Return the tuple.
                (Some(client_state), light_block.into())
            } else {
                panic!("Unknown client type")
            };

        let prev_consensus_state = if client_type.as_str() == MOCK_CLIENT_TYPE {
            MockConsensusState::new(MockHeader::new(prev_cs_height)).into()
        } else if client_type.as_str() == TENDERMINT_CLIENT_TYPE {
            let light_block = HostBlock::generate_tm_block(
                self.host_chain_id.clone(),
                prev_cs_height.revision_height(),
                now.sub(self.block_time).expect("Never fails"),
            );
            light_block.into()
        } else {
            panic!("Unknown client type")
        };

        let consensus_states = vec![
            (prev_cs_height, prev_consensus_state),
            (cs_height, consensus_state),
        ]
        .into_iter()
        .collect();

        debug!("consensus states: {:?}", consensus_states);

        let client_record = MockClientRecord {
            client_state,
            consensus_states,
        };

        self.ibc_store
            .lock()
            .clients
            .insert(client_id.clone(), client_record);
        self
    }

    pub fn with_client_config(self, client: MockClientConfig) -> Self {
        let cs_heights = if client.consensus_state_heights.is_empty() {
            vec![client.client_state_height]
        } else {
            client.consensus_state_heights
        };

        let (client_state, consensus_states) = match client.client_type.as_str() {
            MOCK_CLIENT_TYPE => {
                let blocks: Vec<_> = cs_heights
                    .into_iter()
                    .map(|cs_height| (cs_height, MockHeader::new(cs_height)))
                    .collect();

                let client_state = MockClientState::new(blocks.last().expect("never fails").1);

                let cs_states = blocks
                    .into_iter()
                    .map(|(height, block)| (height, MockConsensusState::new(block).into()))
                    .collect();

                (client_state.into(), cs_states)
            }
            TENDERMINT_CLIENT_TYPE => {
                let n_blocks = cs_heights.len();
                let blocks: Vec<_> = cs_heights
                    .into_iter()
                    .enumerate()
                    .map(|(i, cs_height)| {
                        (
                            cs_height,
                            HostBlock::generate_tm_block(
                                client.client_chain_id.clone(),
                                cs_height.revision_height(),
                                client
                                    .latest_timestamp
                                    .sub(self.block_time * ((n_blocks - 1 - i) as u32))
                                    .expect("never fails"),
                            ),
                        )
                    })
                    .collect();

                // TODO(rano): parametrize the other params
                let client_state: TmClientState = TmClientStateConfig::builder()
                    .chain_id(client.client_chain_id)
                    .latest_height(client.client_state_height)
                    .build()
                    .try_into()
                    .expect("never fails");

                client_state.validate().expect("never fails");

                let cs_states = blocks
                    .into_iter()
                    .map(|(height, block)| (height, block.into()))
                    .collect();

                (client_state.into(), cs_states)
            }
            _ => todo!(),
        };

        let client_record = MockClientRecord {
            client_state: Some(client_state),
            consensus_states,
        };

        self.ibc_store
            .lock()
            .clients
            .insert(client.client_id.clone(), client_record);
        self
    }

    /// Associates a connection to this context.
    pub fn with_connection(
        self,
        connection_id: ConnectionId,
        connection_end: ConnectionEnd,
    ) -> Self {
        self.ibc_store
            .lock()
            .connections
            .insert(connection_id, connection_end);
        self
    }

    /// Associates a channel (in an arbitrary state) to this context.
    pub fn with_channel(
        self,
        port_id: PortId,
        chan_id: ChannelId,
        channel_end: ChannelEnd,
    ) -> Self {
        let mut channels = self.ibc_store.lock().channels.clone();
        channels
            .entry(port_id)
            .or_default()
            .insert(chan_id, channel_end);
        self.ibc_store.lock().channels = channels;
        self
    }

    pub fn with_send_sequence(
        self,
        port_id: PortId,
        chan_id: ChannelId,
        seq_number: Sequence,
    ) -> Self {
        let mut next_sequence_send = self.ibc_store.lock().next_sequence_send.clone();
        next_sequence_send
            .entry(port_id)
            .or_default()
            .insert(chan_id, seq_number);
        self.ibc_store.lock().next_sequence_send = next_sequence_send;
        self
    }

    pub fn with_recv_sequence(
        self,
        port_id: PortId,
        chan_id: ChannelId,
        seq_number: Sequence,
    ) -> Self {
        let mut next_sequence_recv = self.ibc_store.lock().next_sequence_recv.clone();
        next_sequence_recv
            .entry(port_id)
            .or_default()
            .insert(chan_id, seq_number);
        self.ibc_store.lock().next_sequence_recv = next_sequence_recv;
        self
    }

    pub fn with_ack_sequence(
        self,
        port_id: PortId,
        chan_id: ChannelId,
        seq_number: Sequence,
    ) -> Self {
        let mut next_sequence_ack = self.ibc_store.lock().next_sequence_send.clone();
        next_sequence_ack
            .entry(port_id)
            .or_default()
            .insert(chan_id, seq_number);
        self.ibc_store.lock().next_sequence_ack = next_sequence_ack;
        self
    }

    pub fn with_height(self, target_height: Height) -> Self {
        let latest_height = self.latest_height();
        if target_height.revision_number() > latest_height.revision_number() {
            unimplemented!()
        } else if target_height.revision_number() < latest_height.revision_number() {
            panic!("Cannot rewind history of the chain to a smaller revision number!")
        } else if target_height.revision_height() < latest_height.revision_height() {
            panic!("Cannot rewind history of the chain to a smaller revision height!")
        } else if target_height.revision_height() > latest_height.revision_height() {
            // Repeatedly advance the host chain height till we hit the desired height
            let mut ctx = MockContext { ..self };
            while ctx.latest_height().revision_height() < target_height.revision_height() {
                ctx.advance_host_chain_height()
            }
            ctx
        } else {
            // Both the revision number and height match
            self
        }
    }

    pub fn with_packet_commitment(
        self,
        port_id: PortId,
        chan_id: ChannelId,
        seq: Sequence,
        data: PacketCommitment,
    ) -> Self {
        let mut packet_commitment = self.ibc_store.lock().packet_commitment.clone();
        packet_commitment
            .entry(port_id)
            .or_default()
            .entry(chan_id)
            .or_default()
            .insert(seq, data);
        self.ibc_store.lock().packet_commitment = packet_commitment;
        self
    }

    /// Accessor for a block of the local (host) chain from this context.
    /// Returns `None` if the block at the requested height does not exist.
    pub fn host_block(&self, target_height: &Height) -> Option<&HostBlock> {
        let target = target_height.revision_height() as usize;
        let latest = self.latest_height().revision_height() as usize;

        // Check that the block is not too advanced, nor has it been pruned.
        if (target > latest) || (target <= latest - self.history.len()) {
            None // Block for requested height does not exist in history.
        } else {
            Some(&self.history[self.history.len() + target - latest - 1])
        }
    }

    /// Triggers the advancing of the host chain, by extending the history of blocks (or headers).
    pub fn advance_host_chain_height(&mut self) {
        self.advance_host_chain_height_with_timestamp(self.host_timestamp().expect("Never fails"))
    }

    /// Triggers the advancing of the host chain, by extending the history of blocks (or headers).
    pub fn advance_host_chain_height_with_timestamp(&mut self, timestamp: Timestamp) {
        let latest_block = self.history.last().expect("history cannot be empty");
        let new_block = HostBlock::generate_block(
            self.host_chain_id.clone(),
            self.host_chain_type,
            latest_block.height().increment().revision_height(),
            timestamp,
        );

        // Append the new header at the tip of the history.
        if self.history.len() >= self.max_history_size {
            // History is full, we rotate and replace the tip with the new header.
            self.history.rotate_left(1);
            self.history[self.max_history_size - 1] = new_block;
        } else {
            // History is not full yet.
            self.history.push(new_block);
        }
    }

    /// A datagram passes from the relayer to the IBC module (on host chain).
    /// Alternative method to `Ics18Context::send` that does not exercise any serialization.
    /// Used in testing the Ics18 algorithms, hence this may return a Ics18Error.
    pub fn deliver(
        &mut self,
        router: &mut impl Router,
        msg: MsgEnvelope,
    ) -> Result<(), RelayerError> {
        dispatch(self, router, msg).map_err(RelayerError::TransactionFailed)?;
        // Create a new block.
        self.advance_host_chain_height();
        Ok(())
    }

    /// Validates this context. Should be called after the context is mutated by a test.
    pub fn validate(&self) -> Result<(), String> {
        // Check that the number of entries is not higher than window size.
        if self.history.len() > self.max_history_size {
            return Err("too many entries".to_string());
        }

        // Check the content of the history.
        if !self.history.is_empty() {
            // Get the highest block.
            let lh = &self.history[self.history.len() - 1];
            // Check latest is properly updated with highest header height.
            if lh.height() != self.latest_height() {
                return Err("latest height is not updated".to_string());
            }
        }

        // Check that headers in the history are in sequential order.
        for i in 1..self.history.len() {
            let ph = &self.history[i - 1];
            let h = &self.history[i];
            if ph.height().increment() != h.height() {
                return Err("headers in history not sequential".to_string());
            }
        }
        Ok(())
    }

    pub fn latest_client_states(&self, client_id: &ClientId) -> AnyClientState {
        self.ibc_store.lock().clients[client_id]
            .client_state
            .as_ref()
            .expect("Never fails")
            .clone()
    }

    pub fn latest_consensus_states(
        &self,
        client_id: &ClientId,
        height: &Height,
    ) -> AnyConsensusState {
        self.ibc_store.lock().clients[client_id]
            .consensus_states
            .get(height)
            .expect("Never fails")
            .clone()
    }

    pub fn latest_height(&self) -> Height {
        self.history
            .last()
            .expect("history cannot be empty")
            .height()
    }

    pub fn ibc_store_share(&self) -> Arc<Mutex<MockIbcStore>> {
        self.ibc_store.clone()
    }

    pub fn query_latest_header(&self) -> Option<HostBlock> {
        let block_ref = self.host_block(&self.host_height().expect("Never fails"));
        block_ref.cloned()
    }
}

type PortChannelIdMap<V> = BTreeMap<PortId, BTreeMap<ChannelId, V>>;

impl RelayerContext for MockContext {
    fn query_latest_height(&self) -> Result<Height, ContextError> {
        ValidationContext::host_height(self)
    }

    fn query_client_full_state(&self, client_id: &ClientId) -> Option<AnyClientState> {
        // Forward call to Ics2.
        self.client_state(client_id).ok()
    }

    fn signer(&self) -> Signer {
        "0CDA3F47EF3C4906693B170EF650EB968C5F4B2C"
            .to_string()
            .into()
    }
}

impl ValidationContext for MockContext {
    type V = Self;
    type E = Self;
    type AnyConsensusState = AnyConsensusState;
    type AnyClientState = AnyClientState;

    fn client_state(&self, client_id: &ClientId) -> Result<Self::AnyClientState, ContextError> {
        match self.ibc_store.lock().clients.get(client_id) {
            Some(client_record) => {
                client_record
                    .client_state
                    .clone()
                    .ok_or_else(|| ClientError::ClientStateNotFound {
                        client_id: client_id.clone(),
                    })
            }
            None => Err(ClientError::ClientStateNotFound {
                client_id: client_id.clone(),
            }),
        }
        .map_err(ContextError::ClientError)
    }

    fn decode_client_state(&self, client_state: Any) -> Result<Self::AnyClientState, ContextError> {
        if let Ok(client_state) = TmClientState::try_from(client_state.clone()) {
            client_state.validate().map_err(ClientError::from)?;
            Ok(client_state.into())
        } else if let Ok(client_state) = MockClientState::try_from(client_state.clone()) {
            Ok(client_state.into())
        } else {
            Err(ClientError::UnknownClientStateType {
                client_state_type: client_state.type_url,
            })
        }
        .map_err(ContextError::ClientError)
    }

    fn consensus_state(
        &self,
        client_cons_state_path: &ClientConsensusStatePath,
    ) -> Result<AnyConsensusState, ContextError> {
        let client_id = &client_cons_state_path.client_id;
        let height = Height::new(client_cons_state_path.epoch, client_cons_state_path.height)?;
        match self.ibc_store.lock().clients.get(client_id) {
            Some(client_record) => match client_record.consensus_states.get(&height) {
                Some(consensus_state) => Ok(consensus_state.clone()),
                None => Err(ClientError::ConsensusStateNotFound {
                    client_id: client_id.clone(),
                    height,
                }),
            },
            None => Err(ClientError::ConsensusStateNotFound {
                client_id: client_id.clone(),
                height,
            }),
        }
        .map_err(ContextError::ClientError)
    }

    fn host_height(&self) -> Result<Height, ContextError> {
        Ok(self.latest_height())
    }

    fn host_timestamp(&self) -> Result<Timestamp, ContextError> {
        Ok(self
            .history
            .last()
            .expect("history cannot be empty")
            .timestamp()
            .add(self.block_time)
            .expect("Never fails"))
    }

    fn host_consensus_state(&self, height: &Height) -> Result<AnyConsensusState, ContextError> {
        match self.host_block(height) {
            Some(block_ref) => Ok(block_ref.clone().into()),
            None => Err(ClientError::MissingLocalConsensusState { height: *height }),
        }
        .map_err(ConnectionError::Client)
        .map_err(ContextError::ConnectionError)
    }

    fn client_counter(&self) -> Result<u64, ContextError> {
        Ok(self.ibc_store.lock().client_ids_counter)
    }

    fn connection_end(&self, cid: &ConnectionId) -> Result<ConnectionEnd, ContextError> {
        match self.ibc_store.lock().connections.get(cid) {
            Some(connection_end) => Ok(connection_end.clone()),
            None => Err(ConnectionError::ConnectionNotFound {
                connection_id: cid.clone(),
            }),
        }
        .map_err(ContextError::ConnectionError)
    }

    fn validate_self_client(
        &self,
        client_state_of_host_on_counterparty: Any,
    ) -> Result<(), ContextError> {
        let mock_client_state = MockClientState::try_from(client_state_of_host_on_counterparty)
            .map_err(|_| ConnectionError::InvalidClientState {
                reason: "client must be a mock client".to_string(),
            })
            .map_err(ContextError::ConnectionError)?;

        if mock_client_state.is_frozen() {
            return Err(ClientError::ClientFrozen {
                description: String::new(),
            }
            .into());
        }

        let self_chain_id = &self.host_chain_id;
        let self_revision_number = self_chain_id.revision_number();
        if self_revision_number != mock_client_state.latest_height().revision_number() {
            return Err(ConnectionError::InvalidClientState {
                reason: format!(
                    "client is not in the same revision as the chain. expected: {}, got: {}",
                    self_revision_number,
                    mock_client_state.latest_height().revision_number()
                ),
            })
            .map_err(ContextError::ConnectionError);
        }

        let host_current_height = self.latest_height().increment();
        if mock_client_state.latest_height() >= host_current_height {
            return Err(ConnectionError::InvalidClientState {
                reason: format!(
                    "client has latest height {} greater than or equal to chain height {}",
                    mock_client_state.latest_height(),
                    host_current_height
                ),
            })
            .map_err(ContextError::ConnectionError);
        }

        Ok(())
    }

    fn commitment_prefix(&self) -> CommitmentPrefix {
        CommitmentPrefix::try_from(b"mock".to_vec()).expect("Never fails")
    }

    fn connection_counter(&self) -> Result<u64, ContextError> {
        Ok(self.ibc_store.lock().connection_ids_counter)
    }

    fn channel_end(&self, chan_end_path: &ChannelEndPath) -> Result<ChannelEnd, ContextError> {
        let port_id = &chan_end_path.0;
        let channel_id = &chan_end_path.1;

        match self
            .ibc_store
            .lock()
            .channels
            .get(port_id)
            .and_then(|map| map.get(channel_id))
        {
            Some(channel_end) => Ok(channel_end.clone()),
            None => Err(ChannelError::ChannelNotFound {
                port_id: port_id.clone(),
                channel_id: channel_id.clone(),
            }),
        }
        .map_err(ContextError::ChannelError)
    }

    fn get_next_sequence_send(
        &self,
        seq_send_path: &SeqSendPath,
    ) -> Result<Sequence, ContextError> {
        let port_id = &seq_send_path.0;
        let channel_id = &seq_send_path.1;

        match self
            .ibc_store
            .lock()
            .next_sequence_send
            .get(port_id)
            .and_then(|map| map.get(channel_id))
        {
            Some(sequence) => Ok(*sequence),
            None => Err(PacketError::MissingNextSendSeq {
                port_id: port_id.clone(),
                channel_id: channel_id.clone(),
            }),
        }
        .map_err(ContextError::PacketError)
    }

    fn get_next_sequence_recv(
        &self,
        seq_recv_path: &SeqRecvPath,
    ) -> Result<Sequence, ContextError> {
        let port_id = &seq_recv_path.0;
        let channel_id = &seq_recv_path.1;

        match self
            .ibc_store
            .lock()
            .next_sequence_recv
            .get(port_id)
            .and_then(|map| map.get(channel_id))
        {
            Some(sequence) => Ok(*sequence),
            None => Err(PacketError::MissingNextRecvSeq {
                port_id: port_id.clone(),
                channel_id: channel_id.clone(),
            }),
        }
        .map_err(ContextError::PacketError)
    }

    fn get_next_sequence_ack(&self, seq_ack_path: &SeqAckPath) -> Result<Sequence, ContextError> {
        let port_id = &seq_ack_path.0;
        let channel_id = &seq_ack_path.1;

        match self
            .ibc_store
            .lock()
            .next_sequence_ack
            .get(port_id)
            .and_then(|map| map.get(channel_id))
        {
            Some(sequence) => Ok(*sequence),
            None => Err(PacketError::MissingNextAckSeq {
                port_id: port_id.clone(),
                channel_id: channel_id.clone(),
            }),
        }
        .map_err(ContextError::PacketError)
    }

    fn get_packet_commitment(
        &self,
        commitment_path: &CommitmentPath,
    ) -> Result<PacketCommitment, ContextError> {
        let port_id = &commitment_path.port_id;
        let channel_id = &commitment_path.channel_id;
        let seq = &commitment_path.sequence;

        match self
            .ibc_store
            .lock()
            .packet_commitment
            .get(port_id)
            .and_then(|map| map.get(channel_id))
            .and_then(|map| map.get(seq))
        {
            Some(commitment) => Ok(commitment.clone()),
            None => Err(PacketError::PacketCommitmentNotFound { sequence: *seq }),
        }
        .map_err(ContextError::PacketError)
    }

    fn get_packet_receipt(&self, receipt_path: &ReceiptPath) -> Result<Receipt, ContextError> {
        let port_id = &receipt_path.port_id;
        let channel_id = &receipt_path.channel_id;
        let seq = &receipt_path.sequence;

        match self
            .ibc_store
            .lock()
            .packet_receipt
            .get(port_id)
            .and_then(|map| map.get(channel_id))
            .and_then(|map| map.get(seq))
        {
            Some(receipt) => Ok(receipt.clone()),
            None => Err(PacketError::PacketReceiptNotFound { sequence: *seq }),
        }
        .map_err(ContextError::PacketError)
    }

    fn get_packet_acknowledgement(
        &self,
        ack_path: &AckPath,
    ) -> Result<AcknowledgementCommitment, ContextError> {
        let port_id = &ack_path.port_id;
        let channel_id = &ack_path.channel_id;
        let seq = &ack_path.sequence;

        match self
            .ibc_store
            .lock()
            .packet_acknowledgement
            .get(port_id)
            .and_then(|map| map.get(channel_id))
            .and_then(|map| map.get(seq))
        {
            Some(ack) => Ok(ack.clone()),
            None => Err(PacketError::PacketAcknowledgementNotFound { sequence: *seq }),
        }
        .map_err(ContextError::PacketError)
    }

    fn channel_counter(&self) -> Result<u64, ContextError> {
        Ok(self.ibc_store.lock().channel_ids_counter)
    }

    fn max_expected_time_per_block(&self) -> Duration {
        self.block_time
    }

    fn validate_message_signer(&self, _signer: &Signer) -> Result<(), ContextError> {
        Ok(())
    }

    fn get_client_validation_context(&self) -> &Self::V {
        self
    }
}

impl ExecutionContext for MockContext {
    fn get_client_execution_context(&mut self) -> &mut Self::E {
        self
    }

    fn increase_client_counter(&mut self) -> Result<(), ContextError> {
        let mut ibc_store = self.ibc_store.lock();

        ibc_store.client_ids_counter = ibc_store
            .client_ids_counter
            .checked_add(1)
            .ok_or(ClientError::CounterOverflow)?;

        Ok(())
    }

    fn store_connection(
        &mut self,
        connection_path: &ConnectionPath,
        connection_end: ConnectionEnd,
    ) -> Result<(), ContextError> {
        let connection_id = connection_path.0.clone();
        self.ibc_store
            .lock()
            .connections
            .insert(connection_id, connection_end);
        Ok(())
    }

    fn store_connection_to_client(
        &mut self,
        client_connection_path: &ClientConnectionPath,
        conn_id: ConnectionId,
    ) -> Result<(), ContextError> {
        let client_id = client_connection_path.0.clone();
        self.ibc_store
            .lock()
            .client_connections
            .insert(client_id, conn_id);
        Ok(())
    }

    fn increase_connection_counter(&mut self) -> Result<(), ContextError> {
        let mut ibc_store = self.ibc_store.lock();

        ibc_store.connection_ids_counter = ibc_store
            .connection_ids_counter
            .checked_add(1)
            .ok_or(ClientError::CounterOverflow)?;

        Ok(())
    }

    fn store_packet_commitment(
        &mut self,
        commitment_path: &CommitmentPath,
        commitment: PacketCommitment,
    ) -> Result<(), ContextError> {
        self.ibc_store
            .lock()
            .packet_commitment
            .entry(commitment_path.port_id.clone())
            .or_default()
            .entry(commitment_path.channel_id.clone())
            .or_default()
            .insert(commitment_path.sequence, commitment);
        Ok(())
    }

    fn delete_packet_commitment(
        &mut self,
        commitment_path: &CommitmentPath,
    ) -> Result<(), ContextError> {
        self.ibc_store
            .lock()
            .packet_commitment
            .get_mut(&commitment_path.port_id)
            .and_then(|map| map.get_mut(&commitment_path.channel_id))
            .and_then(|map| map.remove(&commitment_path.sequence));
        Ok(())
    }

    fn store_packet_receipt(
        &mut self,
        path: &ReceiptPath,
        receipt: Receipt,
    ) -> Result<(), ContextError> {
        self.ibc_store
            .lock()
            .packet_receipt
            .entry(path.port_id.clone())
            .or_default()
            .entry(path.channel_id.clone())
            .or_default()
            .insert(path.sequence, receipt);
        Ok(())
    }

    fn store_packet_acknowledgement(
        &mut self,
        ack_path: &AckPath,
        ack_commitment: AcknowledgementCommitment,
    ) -> Result<(), ContextError> {
        let port_id = ack_path.port_id.clone();
        let channel_id = ack_path.channel_id.clone();
        let seq = ack_path.sequence;

        self.ibc_store
            .lock()
            .packet_acknowledgement
            .entry(port_id)
            .or_default()
            .entry(channel_id)
            .or_default()
            .insert(seq, ack_commitment);
        Ok(())
    }

    fn delete_packet_acknowledgement(&mut self, ack_path: &AckPath) -> Result<(), ContextError> {
        let port_id = ack_path.port_id.clone();
        let channel_id = ack_path.channel_id.clone();
        let sequence = ack_path.sequence;

        self.ibc_store
            .lock()
            .packet_acknowledgement
            .get_mut(&port_id)
            .and_then(|map| map.get_mut(&channel_id))
            .and_then(|map| map.remove(&sequence));
        Ok(())
    }

    fn store_channel(
        &mut self,
        channel_end_path: &ChannelEndPath,
        channel_end: ChannelEnd,
    ) -> Result<(), ContextError> {
        let port_id = channel_end_path.0.clone();
        let channel_id = channel_end_path.1.clone();

        self.ibc_store
            .lock()
            .channels
            .entry(port_id)
            .or_default()
            .insert(channel_id, channel_end);
        Ok(())
    }

    fn store_next_sequence_send(
        &mut self,
        seq_send_path: &SeqSendPath,
        seq: Sequence,
    ) -> Result<(), ContextError> {
        let port_id = seq_send_path.0.clone();
        let channel_id = seq_send_path.1.clone();

        self.ibc_store
            .lock()
            .next_sequence_send
            .entry(port_id)
            .or_default()
            .insert(channel_id, seq);
        Ok(())
    }

    fn store_next_sequence_recv(
        &mut self,
        seq_recv_path: &SeqRecvPath,
        seq: Sequence,
    ) -> Result<(), ContextError> {
        let port_id = seq_recv_path.0.clone();
        let channel_id = seq_recv_path.1.clone();

        self.ibc_store
            .lock()
            .next_sequence_recv
            .entry(port_id)
            .or_default()
            .insert(channel_id, seq);
        Ok(())
    }

    fn store_next_sequence_ack(
        &mut self,
        seq_ack_path: &SeqAckPath,
        seq: Sequence,
    ) -> Result<(), ContextError> {
        let port_id = seq_ack_path.0.clone();
        let channel_id = seq_ack_path.1.clone();

        self.ibc_store
            .lock()
            .next_sequence_ack
            .entry(port_id)
            .or_default()
            .insert(channel_id, seq);
        Ok(())
    }

    fn increase_channel_counter(&mut self) -> Result<(), ContextError> {
        let mut ibc_store = self.ibc_store.lock();

        ibc_store.channel_ids_counter = ibc_store
            .channel_ids_counter
            .checked_add(1)
            .ok_or(ClientError::CounterOverflow)?;

        Ok(())
    }

    fn emit_ibc_event(&mut self, event: IbcEvent) -> Result<(), ContextError> {
        self.events.push(event);
        Ok(())
    }

    fn log_message(&mut self, message: String) -> Result<(), ContextError> {
        self.logs.push(message);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use test_log::test;

    use super::*;
    use crate::core::ics04_channel::acknowledgement::Acknowledgement;
    use crate::core::ics04_channel::channel::{Counterparty, Order};
    use crate::core::ics04_channel::error::ChannelError;
    use crate::core::ics04_channel::packet::Packet;
    use crate::core::ics04_channel::Version;
    use crate::core::ics24_host::identifier::{ChainId, ChannelId, ConnectionId, PortId};
    use crate::core::router::{Module, ModuleExtras, ModuleId};
    use crate::mock::context::MockContext;
    use crate::mock::host::HostType;
    use crate::mock::router::MockRouter;
    use crate::signer::Signer;
    use crate::test_utils::get_dummy_bech32_account;
    use crate::Height;

    #[test]
    fn test_history_manipulation() {
        pub struct Test {
            name: String,
            ctx: MockContext,
        }
        let cv = 1; // The version to use for all chains.

        let mock_chain_id = ChainId::new("mockgaia", cv).unwrap();

        let tests: Vec<Test> = vec![
            Test {
                name: "Empty history, small pruning window".to_string(),
                ctx: MockContext::new(
                    mock_chain_id.clone(),
                    HostType::Mock,
                    2,
                    Height::new(cv, 1).expect("Never fails"),
                ),
            },
            Test {
                name: "[Synthetic TM host] Empty history, small pruning window".to_string(),
                ctx: MockContext::new(
                    mock_chain_id.clone(),
                    HostType::SyntheticTendermint,
                    2,
                    Height::new(cv, 1).expect("Never fails"),
                ),
            },
            Test {
                name: "Large pruning window".to_string(),
                ctx: MockContext::new(
                    mock_chain_id.clone(),
                    HostType::Mock,
                    30,
                    Height::new(cv, 2).expect("Never fails"),
                ),
            },
            Test {
                name: "[Synthetic TM host] Large pruning window".to_string(),
                ctx: MockContext::new(
                    mock_chain_id.clone(),
                    HostType::SyntheticTendermint,
                    30,
                    Height::new(cv, 2).expect("Never fails"),
                ),
            },
            Test {
                name: "Small pruning window".to_string(),
                ctx: MockContext::new(
                    mock_chain_id.clone(),
                    HostType::Mock,
                    3,
                    Height::new(cv, 30).expect("Never fails"),
                ),
            },
            Test {
                name: "[Synthetic TM host] Small pruning window".to_string(),
                ctx: MockContext::new(
                    mock_chain_id.clone(),
                    HostType::SyntheticTendermint,
                    3,
                    Height::new(cv, 30).expect("Never fails"),
                ),
            },
            Test {
                name: "Small pruning window, small starting height".to_string(),
                ctx: MockContext::new(
                    mock_chain_id.clone(),
                    HostType::Mock,
                    3,
                    Height::new(cv, 2).expect("Never fails"),
                ),
            },
            Test {
                name: "[Synthetic TM host] Small pruning window, small starting height".to_string(),
                ctx: MockContext::new(
                    mock_chain_id.clone(),
                    HostType::SyntheticTendermint,
                    3,
                    Height::new(cv, 2).expect("Never fails"),
                ),
            },
            Test {
                name: "Large pruning window, large starting height".to_string(),
                ctx: MockContext::new(
                    mock_chain_id.clone(),
                    HostType::Mock,
                    50,
                    Height::new(cv, 2000).expect("Never fails"),
                ),
            },
            Test {
                name: "[Synthetic TM host] Large pruning window, large starting height".to_string(),
                ctx: MockContext::new(
                    mock_chain_id,
                    HostType::SyntheticTendermint,
                    50,
                    Height::new(cv, 2000).expect("Never fails"),
                ),
            },
        ];

        for mut test in tests {
            // All tests should yield a valid context after initialization.
            assert!(
                test.ctx.validate().is_ok(),
                "failed in test {} while validating context {:?}",
                test.name,
                test.ctx
            );

            let current_height = test.ctx.latest_height();

            // After advancing the chain's height, the context should still be valid.
            test.ctx.advance_host_chain_height();
            assert!(
                test.ctx.validate().is_ok(),
                "failed in test {} while validating context {:?}",
                test.name,
                test.ctx
            );

            let next_height = current_height.increment();
            assert_eq!(
                test.ctx.latest_height(),
                next_height,
                "failed while increasing height for context {:?}",
                test.ctx
            );

            assert_eq!(
                test.ctx
                    .host_block(&current_height)
                    .expect("Never fails")
                    .height(),
                current_height,
                "failed while fetching height {:?} of context {:?}",
                current_height,
                test.ctx
            );
        }
    }

    #[test]
    fn test_router() {
        #[derive(Debug, Default)]
        struct FooModule {
            counter: usize,
        }

        impl Module for FooModule {
            fn on_chan_open_init_validate(
                &self,
                _order: Order,
                _connection_hops: &[ConnectionId],
                _port_id: &PortId,
                _channel_id: &ChannelId,
                _counterparty: &Counterparty,
                version: &Version,
            ) -> Result<Version, ChannelError> {
                Ok(version.clone())
            }

            fn on_chan_open_init_execute(
                &mut self,
                _order: Order,
                _connection_hops: &[ConnectionId],
                _port_id: &PortId,
                _channel_id: &ChannelId,
                _counterparty: &Counterparty,
                version: &Version,
            ) -> Result<(ModuleExtras, Version), ChannelError> {
                Ok((ModuleExtras::empty(), version.clone()))
            }

            fn on_chan_open_try_validate(
                &self,
                _order: Order,
                _connection_hops: &[ConnectionId],
                _port_id: &PortId,
                _channel_id: &ChannelId,
                _counterparty: &Counterparty,
                counterparty_version: &Version,
            ) -> Result<Version, ChannelError> {
                Ok(counterparty_version.clone())
            }

            fn on_chan_open_try_execute(
                &mut self,
                _order: Order,
                _connection_hops: &[ConnectionId],
                _port_id: &PortId,
                _channel_id: &ChannelId,
                _counterparty: &Counterparty,
                counterparty_version: &Version,
            ) -> Result<(ModuleExtras, Version), ChannelError> {
                Ok((ModuleExtras::empty(), counterparty_version.clone()))
            }

            fn on_recv_packet_execute(
                &mut self,
                _packet: &Packet,
                _relayer: &Signer,
            ) -> (ModuleExtras, Acknowledgement) {
                self.counter += 1;

                (
                    ModuleExtras::empty(),
                    Acknowledgement::try_from(vec![1u8]).expect("Never fails"),
                )
            }

            fn on_timeout_packet_validate(
                &self,
                _packet: &Packet,
                _relayer: &Signer,
            ) -> Result<(), PacketError> {
                Ok(())
            }

            fn on_timeout_packet_execute(
                &mut self,
                _packet: &Packet,
                _relayer: &Signer,
            ) -> (ModuleExtras, Result<(), PacketError>) {
                (ModuleExtras::empty(), Ok(()))
            }

            fn on_acknowledgement_packet_validate(
                &self,
                _packet: &Packet,
                _acknowledgement: &Acknowledgement,
                _relayer: &Signer,
            ) -> Result<(), PacketError> {
                Ok(())
            }

            fn on_acknowledgement_packet_execute(
                &mut self,
                _packet: &Packet,
                _acknowledgement: &Acknowledgement,
                _relayer: &Signer,
            ) -> (ModuleExtras, Result<(), PacketError>) {
                (ModuleExtras::empty(), Ok(()))
            }
        }

        #[derive(Debug, Default)]
        struct BarModule;

        impl Module for BarModule {
            fn on_chan_open_init_validate(
                &self,
                _order: Order,
                _connection_hops: &[ConnectionId],
                _port_id: &PortId,
                _channel_id: &ChannelId,
                _counterparty: &Counterparty,
                version: &Version,
            ) -> Result<Version, ChannelError> {
                Ok(version.clone())
            }

            fn on_chan_open_init_execute(
                &mut self,
                _order: Order,
                _connection_hops: &[ConnectionId],
                _port_id: &PortId,
                _channel_id: &ChannelId,
                _counterparty: &Counterparty,
                version: &Version,
            ) -> Result<(ModuleExtras, Version), ChannelError> {
                Ok((ModuleExtras::empty(), version.clone()))
            }

            fn on_chan_open_try_validate(
                &self,
                _order: Order,
                _connection_hops: &[ConnectionId],
                _port_id: &PortId,
                _channel_id: &ChannelId,
                _counterparty: &Counterparty,
                counterparty_version: &Version,
            ) -> Result<Version, ChannelError> {
                Ok(counterparty_version.clone())
            }

            fn on_chan_open_try_execute(
                &mut self,
                _order: Order,
                _connection_hops: &[ConnectionId],
                _port_id: &PortId,
                _channel_id: &ChannelId,
                _counterparty: &Counterparty,
                counterparty_version: &Version,
            ) -> Result<(ModuleExtras, Version), ChannelError> {
                Ok((ModuleExtras::empty(), counterparty_version.clone()))
            }

            fn on_recv_packet_execute(
                &mut self,
                _packet: &Packet,
                _relayer: &Signer,
            ) -> (ModuleExtras, Acknowledgement) {
                (
                    ModuleExtras::empty(),
                    Acknowledgement::try_from(vec![1u8]).expect("Never fails"),
                )
            }

            fn on_timeout_packet_validate(
                &self,
                _packet: &Packet,
                _relayer: &Signer,
            ) -> Result<(), PacketError> {
                Ok(())
            }

            fn on_timeout_packet_execute(
                &mut self,
                _packet: &Packet,
                _relayer: &Signer,
            ) -> (ModuleExtras, Result<(), PacketError>) {
                (ModuleExtras::empty(), Ok(()))
            }

            fn on_acknowledgement_packet_validate(
                &self,
                _packet: &Packet,
                _acknowledgement: &Acknowledgement,
                _relayer: &Signer,
            ) -> Result<(), PacketError> {
                Ok(())
            }

            fn on_acknowledgement_packet_execute(
                &mut self,
                _packet: &Packet,
                _acknowledgement: &Acknowledgement,
                _relayer: &Signer,
            ) -> (ModuleExtras, Result<(), PacketError>) {
                (ModuleExtras::empty(), Ok(()))
            }
        }

        let mut router = MockRouter::default();
        router
            .add_route(ModuleId::new("foomodule".to_string()), FooModule::default())
            .expect("Never fails");
        router
            .add_route(ModuleId::new("barmodule".to_string()), BarModule)
            .expect("Never fails");

        let mut on_recv_packet_result = |module_id: &'static str| {
            let module_id = ModuleId::new(module_id.to_string());
            let m = router.get_route_mut(&module_id).expect("Never fails");
            let result =
                m.on_recv_packet_execute(&Packet::default(), &get_dummy_bech32_account().into());
            (module_id, result)
        };

        let _results = vec![
            on_recv_packet_result("foomodule"),
            on_recv_packet_result("barmodule"),
        ];
    }
}
