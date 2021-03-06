// Copyright 2015-2020 Parity Technologies (UK) Ltd.
// This file is part of Parity Secret Store.

// Parity Secret Store is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity Secret Store is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity Secret Store.  If not, see <http://www.gnu.org/licenses/>.

use std::sync::Arc;
use std::collections::BTreeSet;
use parity_crypto::publickey::{Public, Signature, Random, Generator};
use ethereum_types::{Address, H256};
use log::trace;
use primitives::acl_storage::AclStorage;
use primitives::key_storage::KeyStorage;
use primitives::key_server_key_pair::KeyServerKeyPair;
use primitives::service::{
	ServiceTasksListener,
	ServiceTasksListenerRegistrar,
};
use crate::network::{ConnectionProvider, ConnectionManager};
use crate::key_server_cluster::{Error, NodeId, SessionId, Requester};
use crate::key_server_cluster::cluster_sessions::{WaitableSession, ClusterSession, AdminSession, ClusterSessions,
	SessionIdWithSubSession, ClusterSessionsContainer, SERVERS_SET_CHANGE_SESSION_ID, create_cluster_view,
	AdminSessionCreationData, ClusterSessionsListener};
use crate::key_server_cluster::cluster_sessions_creator::ClusterSessionCreator;
use crate::key_server_cluster::cluster_message_processor::MessageProcessor;
use crate::key_server_cluster::message::Message;
use crate::key_server_cluster::generation_session::{SessionImpl as GenerationSession};
use crate::key_server_cluster::decryption_session::{SessionImpl as DecryptionSession};
use crate::key_server_cluster::encryption_session::{SessionImpl as EncryptionSession};
use crate::key_server_cluster::cluster_message_processor::SessionsMessageProcessor;
use crate::key_server_cluster::signing_session_ecdsa::{SessionImpl as EcdsaSigningSession};
use crate::key_server_cluster::signing_session_schnorr::{SessionImpl as SchnorrSigningSession};
use crate::key_server_cluster::key_version_negotiation_session::{SessionImpl as KeyVersionNegotiationSession,
	IsolatedSessionTransport as KeyVersionNegotiationSessionTransport, ContinueAction, FailedContinueAction};
use crate::key_server_cluster::connection_trigger::ServersSetChangeSessionCreatorConnector;

/// Cluster interface for external clients.
pub trait ClusterClient: Send + Sync {
	/// Start new generation session.
	fn new_generation_session(
		&self,
		session_id: SessionId,
		origin: Option<Address>,
		author: Address,
		threshold: usize,
	) -> Result<WaitableSession<GenerationSession>, Error>;
	/// Start new encryption session.
	fn new_encryption_session(
		&self,
		session_id: SessionId,
		author: Requester,
		common_point: Public,
		encrypted_point: Public,
	) -> Result<WaitableSession<EncryptionSession>, Error>;
	/// Start new decryption session.
	fn new_decryption_session(
		&self,
		session_id: SessionId,
		origin: Option<Address>,
		requester: Requester,
		version: Option<H256>,
		is_shadow_decryption: bool,
		is_broadcast_decryption: bool,
	) -> Result<WaitableSession<DecryptionSession>, Error>;
	/// Start new Schnorr signing session.
	fn new_schnorr_signing_session(
		&self,
		session_id: SessionId,
		requester: Requester,
		version: Option<H256>,
		message_hash: H256,
	) -> Result<WaitableSession<SchnorrSigningSession>, Error>;
	/// Start new ECDSA session.
	fn new_ecdsa_signing_session(
		&self,
		session_id: SessionId,
		requester: Requester,
		version: Option<H256>,
		message_hash: H256,
	) -> Result<WaitableSession<EcdsaSigningSession>, Error>;
	/// Start new key version negotiation session.
	fn new_key_version_negotiation_session(
		&self,
		session_id: SessionId,
	) -> Result<WaitableSession<KeyVersionNegotiationSession<KeyVersionNegotiationSessionTransport>>, Error>;
	/// Start new servers set change session.
	fn new_servers_set_change_session(
		&self,
		session_id: Option<SessionId>,
		migration_id: Option<H256>,
		new_nodes_set: BTreeSet<NodeId>,
		old_set_signature: Signature,
		new_set_signature: Signature,
	) -> Result<WaitableSession<AdminSession>, Error>;

	/// Return cluster session listener registrar.
	fn session_listener_registrar(&self) -> Arc<dyn ServiceTasksListenerRegistrar>;

	/// Ask node to make 'faulty' generation sessions.
	#[cfg(test)]
	fn make_faulty_generation_sessions(&self);
	/// Get active generation session with given id.
	#[cfg(test)]
	fn generation_session(&self, session_id: &SessionId) -> Option<Arc<GenerationSession>>;

	/// Are we connected to every required server?
	fn is_fully_connected(&self) -> bool;
	/// Try connect to disconnected nodes.
	fn connect(&self);
	/// True if node has active sessions.
	fn has_active_sessions(&self) -> bool;
}

/// Cluster access for single session participant.
pub trait Cluster: Send + Sync {
	/// Broadcast message to all other nodes.
	fn broadcast(&self, message: Message) -> Result<(), Error>;
	/// Send message to given node.
	fn send(&self, to: &NodeId, message: Message) -> Result<(), Error>;
	/// Is connected to given node?
	fn is_connected(&self, node: &NodeId) -> bool;
	/// Get a set of connected nodes.
	fn nodes(&self) -> BTreeSet<NodeId>;
	/// Get total count of configured key server nodes (valid at the time of ClusterView creation).
	fn configured_nodes_count(&self) -> usize;
	/// Get total count of connected key server nodes (valid at the time of ClusterView creation).
	fn connected_nodes_count(&self) -> usize;
}

/// Network cluster implementation.
pub struct ClusterCore<C: ConnectionManager> {
	/// Cluster data.
	data: Arc<ClusterData<C>>,
}

/// Network cluster client interface implementation.
pub struct ClusterClientImpl<C: ConnectionManager> {
	/// Cluster data.
	data: Arc<ClusterData<C>>,
}

/// Session listener registrar.
struct ClusterSessionListenerRegistrar<C: ConnectionManager> {
	/// Cluster data.
	data: Arc<ClusterData<C>>,
}

/// Network cluster view. It is a communication channel, required in single session.
pub struct ClusterView {
	configured_nodes_count: usize,
	connected_nodes: BTreeSet<NodeId>,
	connections: Arc<dyn ConnectionProvider>,
	self_key_pair: Arc<dyn KeyServerKeyPair>,
}

/// Cross-thread shareable cluster data.
pub struct ClusterData<C: ConnectionManager> {
	/// KeyPair this node holds.
	pub self_key_pair: Arc<dyn KeyServerKeyPair>,
	/// Reference to key storage
	pub key_storage: Arc<dyn KeyStorage>,
	/// Reference to ACL storage
	pub acl_storage: Arc<dyn AclStorage>,
	/// Administrator public key.
	pub admin_address: Option<Address>,
	/// Connections data.
	pub connections: Arc<C>,
	/// Active sessions data.
	pub sessions: Arc<ClusterSessions>,
	// Messages processor.
	pub message_processor: Arc<dyn MessageProcessor>,
	/// Link between servers set chnage session and the connections manager.
	pub servers_set_change_creator_connector: Arc<dyn ServersSetChangeSessionCreatorConnector>,
}

/// Create cluster.
pub fn create_cluster<CM: ConnectionManager>(
	self_key_pair: Arc<dyn KeyServerKeyPair>,
	admin_address: Option<Address>,
	key_storage: Arc<dyn KeyStorage>,
	acl_storage: Arc<dyn AclStorage>,
	servers_set_change_creator_connector: Arc<dyn ServersSetChangeSessionCreatorConnector>,
	connection_provider: Arc<dyn ConnectionProvider>,
	make_connections_manager: impl FnOnce(Arc<dyn MessageProcessor>) -> Result<Arc<CM>, Error>,
) -> Result<Arc<ClusterCore<CM>>, Error> {
	let sessions = Arc::new(ClusterSessions::new(
		self_key_pair.address(),
		admin_address,
		key_storage.clone(),
		acl_storage.clone(),
		servers_set_change_creator_connector.clone(),
	));
	let message_processor = Arc::new(SessionsMessageProcessor::new(
		self_key_pair.clone(),
		servers_set_change_creator_connector.clone(),
		sessions.clone(),
		connection_provider,
	));
	
	let connections_manager = make_connections_manager(message_processor.clone())?;
	let cluster = Arc::new(ClusterCore {
		data: Arc::new(ClusterData {
			self_key_pair,
			connections: connections_manager,
			sessions,
			key_storage,
			acl_storage,
			admin_address,
			message_processor,
			servers_set_change_creator_connector
		}),
	});

	cluster.run()?;

	Ok(cluster)
}

impl<C: ConnectionManager> ClusterCore<C> {
	/// Create new client interface.
	pub fn client(&self) -> Arc<dyn ClusterClient> {
		Arc::new(ClusterClientImpl::new(self.data.clone()))
	}

	/// Run cluster.
	pub fn run(&self) -> Result<(), Error> {
		self.data.connections.connect();
		Ok(())
	}

	#[cfg(test)]
	pub fn view(&self) -> Result<Arc<dyn Cluster>, Error> {
		let connections = self.data.connections.provider();
		let mut connected_nodes = connections.connected_nodes()?;
		let disconnected_nodes = connections.disconnected_nodes();
		connected_nodes.insert(self.data.self_key_pair.address());

		let connected_nodes_count = connected_nodes.len();
		let disconnected_nodes_count = disconnected_nodes.len();
		Ok(Arc::new(ClusterView::new(
			self.data.self_key_pair.clone(),
			connections,
			connected_nodes,
			connected_nodes_count + disconnected_nodes_count)))
	}
}

impl ClusterView {
	pub fn new(
		self_key_pair: Arc<dyn KeyServerKeyPair>,
		connections: Arc<dyn ConnectionProvider>,
		nodes: BTreeSet<NodeId>,
		configured_nodes_count: usize
	) -> Self {
		ClusterView {
			configured_nodes_count: configured_nodes_count,
			connected_nodes: nodes,
			connections,
			self_key_pair,
		}
	}
}

impl Cluster for ClusterView {
	fn broadcast(&self, message: Message) -> Result<(), Error> {
		for node in self.connected_nodes.iter().filter(|n| **n != self.self_key_pair.address()) {
			trace!(target: "secretstore_net", "{}: sent message {} to {}", self.self_key_pair.address(), message, node);
			let connection = self.connections.connection(node).ok_or(Error::NodeDisconnected)?;
			connection.send_message(message.clone());
		}
		Ok(())
	}

	fn send(&self, to: &NodeId, message: Message) -> Result<(), Error> {
		trace!(target: "secretstore_net", "{}: sent message {} to {}", self.self_key_pair.address(), message, to);
		let connection = self.connections.connection(to).ok_or(Error::NodeDisconnected)?;
		connection.send_message(message);
		Ok(())
	}

	fn is_connected(&self, node: &NodeId) -> bool {
		self.connected_nodes.contains(node)
	}

	fn nodes(&self) -> BTreeSet<NodeId> {
		self.connected_nodes.clone()
	}

	fn configured_nodes_count(&self) -> usize {
		self.configured_nodes_count
	}

	fn connected_nodes_count(&self) -> usize {
		self.connected_nodes.len()
	}
}

impl<C: ConnectionManager> ClusterClientImpl<C> {
	pub fn new(data: Arc<ClusterData<C>>) -> Self {
		ClusterClientImpl {
			data: data,
		}
	}

	fn create_key_version_negotiation_session(
		&self,
		session_id: SessionId,
	) -> Result<WaitableSession<KeyVersionNegotiationSession<KeyVersionNegotiationSessionTransport>>, Error> {
		let mut connected_nodes = self.data.connections.provider().connected_nodes()?;
		connected_nodes.insert(self.data.self_key_pair.address());

		let access_key = Random.generate().secret().clone();
		let session_id = SessionIdWithSubSession::new(session_id, access_key);
		let cluster = create_cluster_view(self.data.self_key_pair.clone(), self.data.connections.provider(), false)?;
		let session = self.data.sessions.negotiation_sessions.insert(cluster, self.data.self_key_pair.address(), session_id.clone(), None, false, None)?;
		match session.session.initialize(connected_nodes) {
			Ok(()) => Ok(session),
			Err(error) => {
				self.data.sessions.negotiation_sessions.remove(&session.session.id());
				Err(error)
			}
		}
	}
}

impl<C: ConnectionManager> ClusterClient for ClusterClientImpl<C> {
	fn new_generation_session(
		&self,
		session_id: SessionId,
		origin: Option<Address>,
		author: Address,
		threshold: usize,
	) -> Result<WaitableSession<GenerationSession>, Error> {
		let mut connected_nodes = self.data.connections.provider().connected_nodes()?;
		connected_nodes.insert(self.data.self_key_pair.address());

		let cluster = create_cluster_view(self.data.self_key_pair.clone(), self.data.connections.provider(), true)?;
		let session = self.data.sessions.generation_sessions.insert(cluster, self.data.self_key_pair.address().clone(), session_id, None, false, None)?;
		process_initialization_result(
			session.session.initialize(origin, author, false, threshold, connected_nodes.into()),
			session, &self.data.sessions.generation_sessions)
	}

	fn new_encryption_session(
		&self,
		session_id: SessionId,
		requester: Requester,
		common_point: Public,
		encrypted_point: Public,
	) -> Result<WaitableSession<EncryptionSession>, Error> {
		let mut connected_nodes = self.data.connections.provider().connected_nodes()?;
		connected_nodes.insert(self.data.self_key_pair.address().clone());

		let cluster = create_cluster_view(self.data.self_key_pair.clone(), self.data.connections.provider(), true)?;
		let session = self.data.sessions.encryption_sessions.insert(cluster, self.data.self_key_pair.address(), session_id, None, false, None)?;
		process_initialization_result(
			session.session.initialize(requester, common_point, encrypted_point),
			session, &self.data.sessions.encryption_sessions)
	}

	fn new_decryption_session(
		&self,
		session_id: SessionId,
		origin: Option<Address>,
		requester: Requester,
		version: Option<H256>,
		is_shadow_decryption: bool,
		is_broadcast_decryption: bool,
	) -> Result<WaitableSession<DecryptionSession>, Error> {
		let mut connected_nodes = self.data.connections.provider().connected_nodes()?;
		connected_nodes.insert(self.data.self_key_pair.address().clone());

		let access_key = Random.generate().secret().clone();
		let session_id = SessionIdWithSubSession::new(session_id, access_key);
		let cluster = create_cluster_view(self.data.self_key_pair.clone(), self.data.connections.provider(), false)?;
		let session = self.data.sessions.decryption_sessions.insert(cluster, self.data.self_key_pair.address().clone(),
			session_id.clone(), None, false, Some(requester))?;

		let initialization_result = match version {
			Some(version) => session.session.initialize(origin, version, is_shadow_decryption, is_broadcast_decryption),
			None => {
				self.create_key_version_negotiation_session(session_id.id.clone())
					.map(|version_session| {
						let continue_action = ContinueAction::Decrypt(
							session.session.clone(),
							origin,
							is_shadow_decryption,
							is_broadcast_decryption,
						);
						version_session.session.set_continue_action(continue_action);
						self.data.message_processor.try_continue_session(Some(version_session.session));
					})
			},
		};

		process_initialization_result(
			initialization_result,
			session, &self.data.sessions.decryption_sessions)
	}

	fn new_schnorr_signing_session(
		&self,
		session_id: SessionId,
		requester: Requester,
		version: Option<H256>,
		message_hash: H256,
	) -> Result<WaitableSession<SchnorrSigningSession>, Error> {
		let mut connected_nodes = self.data.connections.provider().connected_nodes()?;
		connected_nodes.insert(self.data.self_key_pair.address().clone());

		let access_key = Random.generate().secret().clone();
		let session_id = SessionIdWithSubSession::new(session_id, access_key);
		let cluster = create_cluster_view(self.data.self_key_pair.clone(), self.data.connections.provider(), false)?;
		let session = self.data.sessions.schnorr_signing_sessions.insert(cluster, self.data.self_key_pair.address(), session_id.clone(), None, false, Some(requester))?;

		let initialization_result = match version {
			Some(version) => session.session.initialize(version, message_hash),
			None => {
				self.create_key_version_negotiation_session(session_id.id.clone())
					.map(|version_session| {
						let continue_action = ContinueAction::SchnorrSign(session.session.clone(), message_hash);
						version_session.session.set_continue_action(continue_action);
						self.data.message_processor.try_continue_session(Some(version_session.session));
					})
			},
		};

		process_initialization_result(
			initialization_result,
			session, &self.data.sessions.schnorr_signing_sessions)
	}

	fn new_ecdsa_signing_session(
		&self,
		session_id: SessionId,
		requester: Requester,
		version: Option<H256>,
		message_hash: H256,
	) -> Result<WaitableSession<EcdsaSigningSession>, Error> {
		let mut connected_nodes = self.data.connections.provider().connected_nodes()?;
		connected_nodes.insert(self.data.self_key_pair.address());

		let access_key = Random.generate().secret().clone();
		let session_id = SessionIdWithSubSession::new(session_id, access_key);
		let cluster = create_cluster_view(self.data.self_key_pair.clone(), self.data.connections.provider(), false)?;
		let session = self.data.sessions.ecdsa_signing_sessions.insert(cluster, self.data.self_key_pair.address(), session_id.clone(), None, false, Some(requester))?;

		let initialization_result = match version {
			Some(version) => session.session.initialize(version, message_hash),
			None => {
				self.create_key_version_negotiation_session(session_id.id.clone())
					.map(|version_session| {
						let continue_action = ContinueAction::EcdsaSign(session.session.clone(), message_hash);
						version_session.session.set_continue_action(continue_action);
						self.data.message_processor.try_continue_session(Some(version_session.session));
					})
			},
		};

		process_initialization_result(
			initialization_result,
			session, &self.data.sessions.ecdsa_signing_sessions)
	}

	fn new_key_version_negotiation_session(
		&self,
		session_id: SessionId,
	) -> Result<WaitableSession<KeyVersionNegotiationSession<KeyVersionNegotiationSessionTransport>>, Error> {
		self.create_key_version_negotiation_session(session_id)
	}

	fn new_servers_set_change_session(
		&self,
		session_id: Option<SessionId>,
		migration_id: Option<H256>,
		new_nodes_set: BTreeSet<NodeId>,
		old_set_signature: Signature,
		new_set_signature: Signature,
	) -> Result<WaitableSession<AdminSession>, Error> {
		new_servers_set_change_session(
			self.data.self_key_pair.clone(),
			&self.data.sessions,
			self.data.connections.provider(),
			self.data.servers_set_change_creator_connector.clone(),
			ServersSetChangeParams {
				session_id,
				migration_id,
				new_nodes_set,
				old_set_signature,
				new_set_signature,
			})
	}

	/// Return cluster session listener registrar.
	fn session_listener_registrar(&self) -> Arc<dyn ServiceTasksListenerRegistrar> {
		Arc::new(ClusterSessionListenerRegistrar {
			data: self.data.clone(),
		})
	}

	#[cfg(test)]
	fn make_faulty_generation_sessions(&self) {
		self.data.sessions.make_faulty_generation_sessions();
	}

	#[cfg(test)]
	fn generation_session(&self, session_id: &SessionId) -> Option<Arc<GenerationSession>> {
		self.data.sessions.generation_sessions.get(session_id, false)
	}

	fn is_fully_connected(&self) -> bool {
		self.data.connections.provider().disconnected_nodes().is_empty()
	}

	fn connect(&self) {
		self.data.connections.connect()
	}

	fn has_active_sessions(&self) -> bool {
		self.data.sessions.has_active_sessions()
	}
}

impl<C: ConnectionManager> ServiceTasksListenerRegistrar for ClusterSessionListenerRegistrar<C> {
	fn register_listener(&self, listener: Arc<dyn ServiceTasksListener>) {
		use primitives::key_server::{
			ServerKeyGenerationResult,
			ServerKeyGenerationParams,
			ServerKeyGenerationArtifacts,
			DocumentKeyShadowRetrievalResult,
			DocumentKeyShadowRetrievalParams,
			DocumentKeyShadowRetrievalArtifacts,
		};
	
		struct ListenerWrapper(Arc<dyn ServiceTasksListener>);

		impl ClusterSessionsListener<GenerationSession> for ListenerWrapper {
			fn on_session_removed(&self, session: Arc<GenerationSession>) {
				// by this time sesion must already be completed - either successfully, or not
				assert!(session.is_finished());

				let key_id = session.id();
				if let Some(session_result) = session.result() {
					self.0.server_key_generated(ServerKeyGenerationResult {
						origin: session.origin(),
						params: ServerKeyGenerationParams {
							key_id,
						},
						result: session_result.map(|key| ServerKeyGenerationArtifacts { key }),
					})
				}
			}
		}

		impl ClusterSessionsListener<DecryptionSession> for ListenerWrapper {
			fn on_session_removed(&self, session: Arc<DecryptionSession>) {
				// by this time sesion must already be completed - either successfully, or not
				assert!(session.is_finished());

				let session_id = session.id();
				let key_id = session_id.id;
				if let Some(session_result) = session.result() {
					let session_side_result = (
						session.is_shadow_decryption_requested(),
						session.requester(),
						session.broadcast_shadows(),
					);
					
					if let (Some(true), Some(requester), Some(participants_coefficients)) = session_side_result {
						self.0.document_key_shadow_retrieved(DocumentKeyShadowRetrievalResult {
							origin: session.origin(),
							params: DocumentKeyShadowRetrievalParams {
								key_id,
								requester,
							},
							result: session_result.map(|result| DocumentKeyShadowRetrievalArtifacts {
								common_point: result.common_point.expect("shadow decryption is requested; qed"),
								threshold: session.threshold(),
								encrypted_document_key: result.decrypted_secret,
								participants_coefficients,
							}),
						});
					}
				}
			}
		}

		impl ClusterSessionsListener<KeyVersionNegotiationSession<KeyVersionNegotiationSessionTransport>> for ListenerWrapper {
			fn on_session_removed(&self, session: Arc<KeyVersionNegotiationSession<KeyVersionNegotiationSessionTransport>>) {
				// by this time sesion must already be completed - either successfully, or not
				assert!(session.is_finished());

				// we're interested in:
				// 1) sessions failed with fatal error
				// 2) with decryption continue action
				let error = match session.result() {
					Some(Err(ref error)) if !error.is_non_fatal() => error.clone(),
					_ => return,
				};

				let (origin, requester) = match session.take_failed_continue_action() {
					Some(FailedContinueAction::Decrypt(origin, requester)) => (origin, requester),
					_ => return,
				};

				let meta = session.meta();
				let key_id = meta.id;
				self.0.document_key_shadow_retrieved(DocumentKeyShadowRetrievalResult {
					origin,
					params: DocumentKeyShadowRetrievalParams {
						key_id,
						requester: requester.into(),
					},
					result: Err(error),
				});
			}
		}

		self.data.sessions.generation_sessions.add_listener(Arc::new(ListenerWrapper(listener.clone())));
		self.data.sessions.decryption_sessions.add_listener(Arc::new(ListenerWrapper(listener.clone())));
		self.data.sessions.negotiation_sessions.add_listener(Arc::new(ListenerWrapper(listener)));	
	}
}

pub struct ServersSetChangeParams {
	pub session_id: Option<SessionId>,
	pub migration_id: Option<H256>,
	pub new_nodes_set: BTreeSet<NodeId>,
	pub old_set_signature: Signature,
	pub new_set_signature: Signature,
}

pub fn new_servers_set_change_session(
	self_key_pair: Arc<dyn KeyServerKeyPair>,
	sessions: &ClusterSessions,
	connections: Arc<dyn ConnectionProvider>,
	servers_set_change_creator_connector: Arc<dyn ServersSetChangeSessionCreatorConnector>,
	params: ServersSetChangeParams,
) -> Result<WaitableSession<AdminSession>, Error> {
	let session_id = match params.session_id {
		Some(session_id) if session_id == *SERVERS_SET_CHANGE_SESSION_ID => session_id,
		Some(_) => return Err(Error::InvalidMessage),
		None => *SERVERS_SET_CHANGE_SESSION_ID,
	};

	let cluster = create_cluster_view(self_key_pair.clone(), connections, true)?;
	let creation_data = AdminSessionCreationData::ServersSetChange(params.migration_id, params.new_nodes_set.clone());
	let session = sessions.admin_sessions
		.insert(cluster, self_key_pair.address(), session_id, None, true, Some(creation_data))?;
	let initialization_result = session.session.as_servers_set_change().expect("servers set change session is created; qed")
		.initialize(params.new_nodes_set, params.old_set_signature, params.new_set_signature);

	if initialization_result.is_ok() {
		servers_set_change_creator_connector.set_key_servers_set_change_session(session.session.clone());
	}

	process_initialization_result(
		initialization_result,
		session, &sessions.admin_sessions)
}

fn process_initialization_result<S, SC>(
	result: Result<(), Error>,
	session: WaitableSession<S>,
	sessions: &ClusterSessionsContainer<S, SC>
) -> Result<WaitableSession<S>, Error>
	where
		S: ClusterSession,
		SC: ClusterSessionCreator<S>
{
	match result {
		Ok(()) if session.session.is_finished() => {
			sessions.remove(&session.session.id());
			Ok(session)
		},
		Ok(()) => Ok(session),
		Err(error) => {
			sessions.remove(&session.session.id());
			Err(error)
		},
	}
}

#[cfg(test)]
pub mod tests {
	use std::sync::Arc;
	use std::sync::atomic::{AtomicUsize, Ordering};
	use std::collections::{BTreeMap, BTreeSet, VecDeque};
	use futures::Future;
	use parking_lot::{Mutex, RwLock};
	use ethereum_types::{Address, H256};
	use parity_crypto::publickey::{Random, Generator, Public, Signature, sign};
	use primitives::acl_storage::{AclStorage, InMemoryPermissiveAclStorage};
	use primitives::key_server_set::{KeyServerSet, InMemoryKeyServerSet};
	use primitives::key_storage::{KeyStorage, InMemoryKeyStorage};
	use primitives::key_server_key_pair::InMemoryKeyServerKeyPair;
	use primitives::key_server_key_pair::KeyServerKeyPair;
	use primitives::service::ServiceTasksListenerRegistrar;
	use crate::network::ConnectionManager;
	use crate::network::in_memory::{InMemoryMessagesQueue, InMemoryConnectionsManager, new_in_memory_connections};
	use crate::key_server_cluster::{NodeId, SessionId, Requester, Error};
	use crate::key_server_cluster::message::Message;
	use crate::key_server_cluster::cluster::{Cluster, ClusterCore, ClusterClient, create_cluster};
	use crate::key_server_cluster::cluster_sessions::{WaitableSession, ClusterSession, ClusterSessions, AdminSession};
	use crate::key_server_cluster::generation_session::{SessionImpl as GenerationSession,
		SessionState as GenerationSessionState};
	use crate::key_server_cluster::decryption_session::{SessionImpl as DecryptionSession};
	use crate::key_server_cluster::encryption_session::{SessionImpl as EncryptionSession};
	use crate::key_server_cluster::signing_session_ecdsa::{SessionImpl as EcdsaSigningSession};
	use crate::key_server_cluster::signing_session_schnorr::{SessionImpl as SchnorrSigningSession};
	use crate::key_server_cluster::key_version_negotiation_session::{SessionImpl as KeyVersionNegotiationSession,
		IsolatedSessionTransport as KeyVersionNegotiationSessionTransport};

	/// Create new in-memory backed cluster.
	pub fn new_test_cluster(
		messages: InMemoryMessagesQueue,
		key_server_set: Arc<dyn KeyServerSet<NetworkAddress=std::net::SocketAddr>>,
		self_key_pair: Arc<dyn KeyServerKeyPair>,
		key_storage: Arc<dyn KeyStorage>,
		acl_storage: Arc<dyn AclStorage>,
		preserve_sessions: bool,
	) -> Result<Arc<ClusterCore<InMemoryConnectionsManager>>, Error> {
		use crate::key_server_cluster::{
			connection_trigger::{ConnectionTrigger, SimpleConnectionTrigger},
		};

		let nodes = key_server_set.snapshot().current_set;
		let connections = Arc::new(new_in_memory_connections(messages, self_key_pair.address(), nodes.keys().cloned().collect()));
		let connections_manager = connections.manager();
		let connection_trigger = Box::new(SimpleConnectionTrigger::new(key_server_set, None));
		let servers_set_change_creator_connector = connection_trigger.servers_set_change_creator_connector();
		let cluster = create_cluster(
			self_key_pair,
			None,
			key_storage,
			acl_storage,
			servers_set_change_creator_connector.clone(),
			connections_manager.provider(),
			move |_message_processor| Ok(connections_manager),
		)?;

		if preserve_sessions {
			cluster.data.sessions.preserve_sessions();
		}

		Ok(cluster)
	}

	#[derive(Default)]
	pub struct DummyClusterClient {
		pub generation_requests_count: AtomicUsize,
	}

	#[derive(Debug)]
	pub struct DummyCluster {
		id: NodeId,
		data: RwLock<DummyClusterData>,
	}

	#[derive(Debug, Default)]
	struct DummyClusterData {
		nodes: BTreeSet<NodeId>,
		messages: VecDeque<(NodeId, Message)>,
	}

	impl ClusterClient for DummyClusterClient {
		fn new_generation_session(
			&self,
			_session_id: SessionId,
			_origin: Option<Address>,
			_author: Address,
			_threshold: usize,
		) -> Result<WaitableSession<GenerationSession>, Error> {
			self.generation_requests_count.fetch_add(1, Ordering::Relaxed);
			Err(Error::Internal("test-error".into()))
		}
		fn new_encryption_session(
			&self,
			_session_id: SessionId,
			_requester: Requester,
			_common_point: Public,
			_encrypted_point: Public,
		) -> Result<WaitableSession<EncryptionSession>, Error> {
			unimplemented!("test-only")
		}
		fn new_decryption_session(
			&self,
			_session_id: SessionId,
			_origin: Option<Address>,
			_requester: Requester,
			_version: Option<H256>,
			_is_shadow_decryption: bool,
			_is_broadcast_session: bool,
		) -> Result<WaitableSession<DecryptionSession>, Error> {
			unimplemented!("test-only")
		}
		fn new_schnorr_signing_session(
			&self,
			_session_id: SessionId,
			_requester: Requester,
			_version: Option<H256>,
			_message_hash: H256,
		) -> Result<WaitableSession<SchnorrSigningSession>, Error> {
			unimplemented!("test-only")
		}
		fn new_ecdsa_signing_session(
			&self,
			_session_id: SessionId,
			_requester: Requester,
			_version: Option<H256>,
			_message_hash: H256,
		) -> Result<WaitableSession<EcdsaSigningSession>, Error> {
			unimplemented!("test-only")
		}

		fn new_key_version_negotiation_session(
			&self,
			_session_id: SessionId,
		) -> Result<WaitableSession<KeyVersionNegotiationSession<KeyVersionNegotiationSessionTransport>>, Error> {
			unimplemented!("test-only")
		}
		fn new_servers_set_change_session(
			&self,
			_session_id: Option<SessionId>,
			_migration_id: Option<H256>,
			_new_nodes_set: BTreeSet<NodeId>,
			_old_set_signature: Signature,
			_new_set_signature: Signature,
		) -> Result<WaitableSession<AdminSession>, Error> {
			unimplemented!("test-only")
		}

		fn session_listener_registrar(&self) -> Arc<dyn ServiceTasksListenerRegistrar> {
			unimplemented!("test-only")
		}

		fn make_faulty_generation_sessions(&self) { unimplemented!("test-only") }
		fn generation_session(&self, _session_id: &SessionId) -> Option<Arc<GenerationSession>> { unimplemented!("test-only") }
		fn is_fully_connected(&self) -> bool { true }
		fn connect(&self) {}
		fn has_active_sessions(&self) -> bool { false }
	}

	impl DummyCluster {
		pub fn new(id: NodeId) -> Self {
			DummyCluster {
				id: id,
				data: RwLock::new(DummyClusterData::default())
			}
		}

		pub fn node(&self) -> NodeId {
			self.id.clone()
		}

		pub fn add_node(&self, node: NodeId) {
			self.data.write().nodes.insert(node);
		}

		pub fn add_nodes<I: Iterator<Item=NodeId>>(&self, nodes: I) {
			self.data.write().nodes.extend(nodes)
		}

		pub fn remove_node(&self, node: &NodeId) {
			self.data.write().nodes.remove(node);
		}

		pub fn take_message(&self) -> Option<(NodeId, Message)> {
			self.data.write().messages.pop_front()
		}
	}

	impl Cluster for DummyCluster {
		fn broadcast(&self, message: Message) -> Result<(), Error> {
			let mut data = self.data.write();
			let all_nodes: Vec<_> = data.nodes.iter().cloned().filter(|n| n != &self.id).collect();
			for node in all_nodes {
				data.messages.push_back((node, message.clone()));
			}
			Ok(())
		}

		fn send(&self, to: &NodeId, message: Message) -> Result<(), Error> {
			debug_assert!(&self.id != to);
			self.data.write().messages.push_back((to.clone(), message));
			Ok(())
		}

		fn is_connected(&self, node: &NodeId) -> bool {
			let data = self.data.read();
			&self.id == node || data.nodes.contains(node)
		}

		fn nodes(&self) -> BTreeSet<NodeId> {
			self.data.read().nodes.iter().cloned().collect()
		}

		fn configured_nodes_count(&self) -> usize {
			self.data.read().nodes.len()
		}

		fn connected_nodes_count(&self) -> usize {
			self.data.read().nodes.len()
		}
	}

	/// Test message loop.
	pub struct MessageLoop {
		messages: InMemoryMessagesQueue,
		preserve_sessions: bool,
		key_pairs_map: BTreeMap<NodeId, Arc<InMemoryKeyServerKeyPair>>,
		acl_storages_map: BTreeMap<NodeId, Arc<InMemoryPermissiveAclStorage>>,
		key_storages_map: BTreeMap<NodeId, Arc<InMemoryKeyStorage>>,
		clusters_map: BTreeMap<NodeId, Arc<ClusterCore<InMemoryConnectionsManager>>>,
	}

	impl ::std::fmt::Debug for MessageLoop {
		fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
			write!(f, "MessageLoop({})", self.clusters_map.len())
		}
	}

	impl MessageLoop {
		/// Returns set of all nodes ids.
		pub fn nodes(&self) -> BTreeSet<NodeId> {
			self.clusters_map.keys().cloned().collect()
		}

		/// Returns nodes id by its index.
		pub fn node(&self, idx: usize) -> NodeId {
			*self.clusters_map.keys().nth(idx).unwrap()
		}

		/// Returns key pair of the node by its idx.
		pub fn node_key_pair(&self, idx: usize) -> &Arc<InMemoryKeyServerKeyPair> {
			self.key_pairs_map.values().nth(idx).unwrap()
		}

		/// Get cluster reference by its index.
		pub fn cluster(&self, idx: usize) -> &Arc<ClusterCore<InMemoryConnectionsManager>> {
			self.clusters_map.values().nth(idx).unwrap()
		}

		/// Get keys storage reference by its index.
		pub fn key_storage(&self, idx: usize) -> &Arc<InMemoryKeyStorage> {
			self.key_storages_map.values().nth(idx).unwrap()
		}

		/// Get keys storage reference by node id.
		pub fn key_storage_of(&self, node: &NodeId) -> &Arc<InMemoryKeyStorage> {
			&self.key_storages_map[node]
		}

		/// Replace key storage of the node by its id.
		pub fn replace_key_storage_of(&mut self, node: &NodeId, key_storage: Arc<InMemoryKeyStorage>) {
			*self.key_storages_map.get_mut(node).unwrap() = key_storage;
		}

		/// Get ACL storage reference by its index.
		pub fn acl_storage(&self, idx: usize) -> &Arc<InMemoryPermissiveAclStorage> {
			self.acl_storages_map.values().nth(idx).unwrap()
		}

		/// Get sessions container reference by its index.
		pub fn sessions(&self, idx: usize) -> &Arc<ClusterSessions> {
			&self.cluster(idx).data.sessions
		}

		/// Get sessions container reference by node id.
		pub fn sessions_of(&self, node: &NodeId) -> &Arc<ClusterSessions> {
			&self.clusters_map[node].data.sessions
		}

		/// Isolate node from others.
		pub fn isolate(&self, idx: usize) {
			let node = self.node(idx);
			for (i, cluster) in self.clusters_map.values().enumerate() {
				if i == idx {
					cluster.data.connections.isolate();
				} else {
					cluster.data.connections.disconnect(node);
				}
			}
		}

		/// Exclude node from cluster.
		pub fn exclude(&mut self, idx: usize) {
			let node = self.node(idx);
			for (i, cluster) in self.clusters_map.values().enumerate() {
				if i != idx {
					cluster.data.connections.exclude(node);
				}
			}
			self.key_storages_map.remove(&node);
			self.acl_storages_map.remove(&node);
			self.key_pairs_map.remove(&node);
			self.clusters_map.remove(&node);
		}

		/// Include new node to the cluster.
		pub fn include(&mut self, node_key_pair: Arc<InMemoryKeyServerKeyPair>) -> usize {
			let key_storage = Arc::new(InMemoryKeyStorage::default());
			let acl_storage = Arc::new(InMemoryPermissiveAclStorage::default());
			let cluster = new_test_cluster(
				self.messages.clone(),
				Arc::new(InMemoryKeyServerSet::new(
					false,
					node_key_pair.address(),
					self.nodes().iter()
						.chain(::std::iter::once(&node_key_pair.address()))
						.map(|n| (*n, format!("127.0.0.1:{}", 13).parse().unwrap()))
						.collect())
				),
				node_key_pair.clone(),
				key_storage.clone(),
				acl_storage.clone(),
				self.preserve_sessions,
			).unwrap();

			for cluster in self.clusters_map.values(){
				cluster.data.connections.include(node_key_pair.address());
			}
			self.acl_storages_map.insert(node_key_pair.address(), acl_storage);
			self.key_storages_map.insert(node_key_pair.address(), key_storage);
			self.clusters_map.insert(node_key_pair.address(), cluster);
			self.key_pairs_map.insert(node_key_pair.address(), node_key_pair.clone());
			self.clusters_map.keys().position(|k| *k == node_key_pair.address()).unwrap()
		}

		/// Is empty message queue?
		pub fn is_empty(&self) -> bool {
			self.messages.lock().is_empty()
		}

		/// Takes next message from the queue.
		pub fn take_message(&self) -> Option<(NodeId, NodeId, Message)> {
			self.messages.lock().pop_front()
		}

		/// Process single message.
		pub fn process_message(&self, from: NodeId, to: NodeId, message: Message) {
			let cluster_data = &self.clusters_map[&to].data;
			let connection = cluster_data.connections.provider().connection(&from).unwrap();
			cluster_data.message_processor.process_connection_message(connection, message);
		}

		/// Take next message and process it.
		pub fn take_and_process_message(&self) -> bool {
			let maybe_message = self.take_message();
			let (from, to, message) = match maybe_message {
				Some((from, to, message)) => (from, to, message),
				None => return false,
			};

			self.process_message(from, to, message);
			true
		}

		/// Loops until `predicate` returns `true` or there are no messages in the queue.
		pub fn loop_until<F>(&self, predicate: F) where F: Fn() -> bool {
			while !predicate() {
				if !self.take_and_process_message() {
					panic!("message queue is empty but goal is not achieved");
				}
			}
		}

		/// Loops until there are no messages in the queue.
		pub fn loop_until_future_completed<T: Send + 'static>(
			&self,
			fut: impl std::future::Future<Output=T> + Send + 'static,
		) -> T {
			use futures03::FutureExt;
			
			let (sender, mut receiver) = futures03::channel::oneshot::channel();
			let pool = futures03::executor::ThreadPool::new().unwrap();
			pool.spawn_ok(fut.map(|result| { let _ = sender.send(result); }));
			loop {
				if !self.take_and_process_message() {
					if let Some(result) = receiver.try_recv().unwrap() {
						return result;
					}
				}
			}
		}
	}

	pub fn make_clusters(num_nodes: usize) -> MessageLoop {
		do_make_clusters(num_nodes, false)
	}

	pub fn make_clusters_and_preserve_sessions(num_nodes: usize) -> MessageLoop {
		do_make_clusters(num_nodes, true)
	}

	fn do_make_clusters(num_nodes: usize, preserve_sessions: bool) -> MessageLoop {
		let ports_begin = 0;
		let messages = Arc::new(Mutex::new(VecDeque::new()));
		let key_pairs: Vec<_> = (0..num_nodes)
			.map(|_| Arc::new(InMemoryKeyServerKeyPair::new(Random.generate()))).collect();
		let key_storages: Vec<_> = (0..num_nodes).map(|_| Arc::new(InMemoryKeyStorage::default())).collect();
		let acl_storages: Vec<_> = (0..num_nodes).map(|_| Arc::new(InMemoryPermissiveAclStorage::default())).collect();
		let clusters: Vec<_> = (0..num_nodes).into_iter()
			.map(|i| {
				new_test_cluster(
					messages.clone(),
					Arc::new(InMemoryKeyServerSet::new(
						false,
						key_pairs[i].address(),
						key_pairs.iter()
							.enumerate()
							.map(|(j, kp)| (kp.address(), format!("127.0.0.1:{}", ports_begin + j as u16).parse().unwrap()))
						.collect()),
					),
					key_pairs[i].clone(),
					key_storages[i].clone(),
					acl_storages[i].clone(),
					preserve_sessions,
				).unwrap()
			})
			.collect();

		let clusters_map = clusters.iter().map(|c| (c.data.self_key_pair.address(), c.clone())).collect();
		let key_pairs_map = key_pairs.into_iter().map(|kp| (kp.address(), kp)).collect();
		let key_storages_map = clusters.iter().zip(key_storages.into_iter())
			.map(|(c, ks)| (c.data.self_key_pair.address(), ks)).collect();
		let acl_storages_map = clusters.iter().zip(acl_storages.into_iter())
			.map(|(c, acls)| (c.data.self_key_pair.address(), acls)).collect();
		MessageLoop { preserve_sessions, messages, key_pairs_map, acl_storages_map, key_storages_map, clusters_map }
	}

	#[test]
	fn cluster_wont_start_generation_session_if_not_fully_connected() {
		let ml = make_clusters(3);
		ml.cluster(0).data.connections.disconnect(ml.cluster(0).data.self_key_pair.address());
		match ml.cluster(0).client().new_generation_session(SessionId::from([1u8; 32]), Default::default(), Default::default(), 1) {
			Err(Error::NodeDisconnected) => (),
			Err(e) => panic!("unexpected error {:?}", e),
			_ => panic!("unexpected success"),
		}
	}

	#[test]
	fn error_in_generation_session_broadcasted_to_all_other_nodes() {
		let _ = ::env_logger::try_init();
		let ml = make_clusters(3);

		// ask one of nodes to produce faulty generation sessions
		ml.cluster(1).client().make_faulty_generation_sessions();

		// start && wait for generation session to fail
		let session = ml.cluster(0).client()
			.new_generation_session(SessionId::from([1u8; 32]), Default::default(), Default::default(), 1).unwrap().session;
		ml.loop_until(|| session.joint_public_and_secret().is_some()
			&& ml.cluster(0).client().generation_session(&SessionId::from([1u8; 32])).is_none());
		assert!(session.joint_public_and_secret().unwrap().is_err());

		// check that faulty session is either removed from all nodes, or nonexistent (already removed)
		for i in 1..3 {
			if let Some(session) = ml.cluster(i).client().generation_session(&SessionId::from([1u8; 32])) {
				// wait for both session completion && session removal (session completion event is fired
				// before session is removed from its own container by cluster)
				ml.loop_until(|| session.joint_public_and_secret().is_some()
					&& ml.cluster(i).client().generation_session(&SessionId::from([1u8; 32])).is_none());
				assert!(session.joint_public_and_secret().unwrap().is_err());
			}
		}
	}

	#[test]
	fn generation_session_completion_signalled_if_failed_on_master() {
		let _ = ::env_logger::try_init();
		let ml = make_clusters(3);

		// ask one of nodes to produce faulty generation sessions
		ml.cluster(0).client().make_faulty_generation_sessions();

		// start && wait for generation session to fail
		let session = ml.cluster(0).client()
			.new_generation_session(SessionId::from([1u8; 32]), Default::default(), Default::default(), 1).unwrap().session;
		ml.loop_until(|| session.joint_public_and_secret().is_some()
			&& ml.cluster(0).client().generation_session(&SessionId::from([1u8; 32])).is_none());
		assert!(session.joint_public_and_secret().unwrap().is_err());

		// check that faulty session is either removed from all nodes, or nonexistent (already removed)
		for i in 1..3 {
			if let Some(session) = ml.cluster(i).client().generation_session(&SessionId::from([1u8; 32])) {
				let session = session.clone();
				// wait for both session completion && session removal (session completion event is fired
				// before session is removed from its own container by cluster)
				ml.loop_until(|| session.joint_public_and_secret().is_some()
					&& ml.cluster(i).client().generation_session(&SessionId::from([1u8; 32])).is_none());
				assert!(session.joint_public_and_secret().unwrap().is_err());
			}
		}
	}

	#[test]
	fn generation_session_is_removed_when_succeeded() {
		let _ = ::env_logger::try_init();
		let ml = make_clusters(3);

		// start && wait for generation session to complete
		let session = ml.cluster(0).client()
			.new_generation_session(SessionId::from([1u8; 32]), Default::default(), Default::default(), 1).unwrap().session;
		ml.loop_until(|| (session.state() == GenerationSessionState::Finished
			|| session.state() == GenerationSessionState::Failed)
			&& ml.cluster(0).client().generation_session(&SessionId::from([1u8; 32])).is_none());
		assert!(session.joint_public_and_secret().unwrap().is_ok());

		// check that on non-master nodes session is either:
		// already removed
		// or it is removed right after completion
		for i in 1..3 {
			if let Some(session) = ml.cluster(i).client().generation_session(&SessionId::from([1u8; 32])) {
				// run to completion if completion message is still on the way
				// AND check that it is actually removed from cluster sessions
				ml.loop_until(|| (session.state() == GenerationSessionState::Finished
					|| session.state() == GenerationSessionState::Failed)
					&& ml.cluster(i).client().generation_session(&SessionId::from([1u8; 32])).is_none());
			}
		}
	}

	#[test]
	fn sessions_are_removed_when_initialization_fails() {
		let ml = make_clusters(3);
		let client = ml.cluster(0).client();

		// generation session
		{
			// try to start generation session => fail in initialization
			assert_eq!(
				client.new_generation_session(SessionId::from([1u8; 32]), None, Default::default(), 100).map(|_| ()),
				Err(Error::NotEnoughNodesForThreshold));

			// try to start generation session => fails in initialization
			assert_eq!(
				client.new_generation_session(SessionId::from([1u8; 32]), None, Default::default(), 100).map(|_| ()),
				Err(Error::NotEnoughNodesForThreshold));

			assert!(ml.cluster(0).data.sessions.generation_sessions.is_empty());
		}

		// decryption session
		{
			// try to start decryption session => fails in initialization
			assert_eq!(
				client.new_decryption_session(
					Default::default(), Default::default(), Requester::Signature(Default::default()), Some(Default::default()), false, false
				).map(|_| ()),
				Err(Error::InvalidMessage));

			// try to start generation session => fails in initialization
			assert_eq!(
				client.new_decryption_session(
					Default::default(), Default::default(), Requester::Signature(Default::default()), Some(Default::default()), false, false
				).map(|_| ()),
				Err(Error::InvalidMessage));

			assert!(ml.cluster(0).data.sessions.decryption_sessions.is_empty());
			assert!(ml.cluster(0).data.sessions.negotiation_sessions.is_empty());
		}
	}

	#[test]
	fn schnorr_signing_session_completes_if_node_does_not_have_a_share() {
		let _ = ::env_logger::try_init();
		let ml = make_clusters(3);
		let dummy_session_id = SessionId::from([1u8; 32]);

		// start && wait for generation session to complete
		let session = ml.cluster(0).client().
			new_generation_session(dummy_session_id, Default::default(), Default::default(), 1).unwrap().session;
		ml.loop_until(|| (session.state() == GenerationSessionState::Finished
			|| session.state() == GenerationSessionState::Failed)
			&& ml.cluster(0).client().generation_session(&dummy_session_id).is_none());
		assert!(session.joint_public_and_secret().unwrap().is_ok());

		// now remove share from node2
		assert!((0..3).all(|i| ml.cluster(i).data.sessions.generation_sessions.is_empty()));
		ml.cluster(2).data.key_storage.remove(&dummy_session_id).unwrap();

		// and try to sign message with generated key
		let dummy_message = [1u8; 32].into();
		let signature = sign(Random.generate().secret(), &dummy_message).unwrap();
		let session0 = ml.cluster(0).client()
			.new_schnorr_signing_session(dummy_session_id, signature.into(), None, Default::default()).unwrap();
		let session = ml.cluster(0).data.sessions.schnorr_signing_sessions.first().unwrap();

		ml.loop_until(|| session.is_finished() && (0..3).all(|i|
			ml.cluster(i).data.sessions.schnorr_signing_sessions.is_empty()));
		session0.into_wait_future().wait().unwrap();

		// and try to sign message with generated key using node that has no key share
		let signature = sign(Random.generate().secret(), &dummy_message).unwrap();
		let session2 = ml.cluster(2).client()
			.new_schnorr_signing_session(dummy_session_id, signature.into(), None, Default::default()).unwrap();
		let session = ml.cluster(2).data.sessions.schnorr_signing_sessions.first().unwrap();

		ml.loop_until(|| session.is_finished()  && (0..3).all(|i|
			ml.cluster(i).data.sessions.schnorr_signing_sessions.is_empty()));
		session2.into_wait_future().wait().unwrap();

		// now remove share from node1
		ml.cluster(1).data.key_storage.remove(&dummy_session_id).unwrap();

		// and try to sign message with generated key
		let signature = sign(Random.generate().secret(), &dummy_message).unwrap();
		let session1 = ml.cluster(0).client()
			.new_schnorr_signing_session(dummy_session_id, signature.into(), None, Default::default()).unwrap();
		let session = ml.cluster(0).data.sessions.schnorr_signing_sessions.first().unwrap();

		ml.loop_until(|| session.is_finished());
		session1.into_wait_future().wait().unwrap_err();
	}

	#[test]
	fn ecdsa_signing_session_completes_if_node_does_not_have_a_share() {
		let _ = ::env_logger::try_init();
		let ml = make_clusters(4);
		let dummy_session_id = SessionId::from([1u8; 32]);

		// start && wait for generation session to complete
		let session = ml.cluster(0).client()
			.new_generation_session(dummy_session_id, Default::default(), Default::default(), 1).unwrap().session;
		ml.loop_until(|| (session.state() == GenerationSessionState::Finished
			|| session.state() == GenerationSessionState::Failed)
			&& ml.cluster(0).client().generation_session(&dummy_session_id).is_none());
		assert!(session.joint_public_and_secret().unwrap().is_ok());

		// now remove share from node2
		assert!((0..3).all(|i| ml.cluster(i).data.sessions.generation_sessions.is_empty()));
		ml.cluster(2).data.key_storage.remove(&dummy_session_id).unwrap();

		// and try to sign message with generated key
		let dummy_message = [1u8; 32].into();
		let signature = sign(Random.generate().secret(), &dummy_message).unwrap();
		let session0 = ml.cluster(0).client()
			.new_ecdsa_signing_session(dummy_session_id, signature.into(), None, H256::random()).unwrap();
		let session = ml.cluster(0).data.sessions.ecdsa_signing_sessions.first().unwrap();

		ml.loop_until(|| session.is_finished() && (0..3).all(|i|
			ml.cluster(i).data.sessions.ecdsa_signing_sessions.is_empty()));
		session0.into_wait_future().wait().unwrap();

		// and try to sign message with generated key using node that has no key share
		let signature = sign(Random.generate().secret(), &dummy_message).unwrap();
		let session2 = ml.cluster(2).client()
			.new_ecdsa_signing_session(dummy_session_id, signature.into(), None, H256::random()).unwrap();
		let session = ml.cluster(2).data.sessions.ecdsa_signing_sessions.first().unwrap();
		ml.loop_until(|| session.is_finished()  && (0..3).all(|i|
			ml.cluster(i).data.sessions.ecdsa_signing_sessions.is_empty()));
		session2.into_wait_future().wait().unwrap();

		// now remove share from node1
		ml.cluster(1).data.key_storage.remove(&dummy_session_id).unwrap();

		// and try to sign message with generated key
		let signature = sign(Random.generate().secret(), &dummy_message).unwrap();
		let session1 = ml.cluster(0).client()
			.new_ecdsa_signing_session(dummy_session_id, signature.into(), None, H256::random()).unwrap();
		let session = ml.cluster(0).data.sessions.ecdsa_signing_sessions.first().unwrap();
		ml.loop_until(|| session.is_finished());
		session1.into_wait_future().wait().unwrap_err();
	}
}
