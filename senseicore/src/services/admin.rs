// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use super::{PaginationRequest, PaginationResponse};
use crate::chain::manager::SenseiChainManager;
use crate::database::SenseiDatabase;
use crate::disk::FilesystemLogger;
use crate::error::Error as SenseiError;
use crate::events::SenseiEvent;
use crate::p2p::utils::parse_peer_info;
use crate::p2p::SenseiP2P;
use crate::{config::SenseiConfig, hex_utils, node::LightningNode, version};

use entity::node::{self, NodeRole};
use entity::sea_orm::{ActiveModelTrait, ActiveValue, EntityTrait};
use entity::{access_token, seconds_since_epoch};
use futures::stream::{self, StreamExt};
use lightning::ln::channelmanager::ChannelDetails;
use lightning::ln::msgs::RoutingMessageHandler;
use lightning::ln::PaymentHash;
use lightning::routing::gossip::NodeId;
use lightning::routing::router::{RouteHop, RouteParameters};
use lightning::routing::scoring::Score;
use lightning::util::ser::{Readable, Writeable};
use lightning_background_processor::BackgroundProcessor;
use lightning_invoice::payment::Router;
use macaroon::Macaroon;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{collections::hash_map::Entry, fs, sync::Arc};
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;
use uuid::Uuid;

pub struct NodeHandle {
    pub node: Arc<LightningNode>,
    pub background_processor: BackgroundProcessor,
    pub handles: Vec<JoinHandle<()>>,
}

#[derive(Clone)]
pub struct NodeCreateInfo {
    pub username: String,
    pub alias: String,
    pub passphrase: String,
    pub start: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NodeCreateResult {
    pubkey: String,
    macaroon: String,
    listen_addr: String,
    listen_port: i32,
    id: String,
}

pub enum AdminRequest {
    GetStatus {
        pubkey: Option<String>,
        authenticated_admin: bool,
    },
    CreateAdmin {
        username: String,
        passphrase: String,
    },
    CreateNode {
        username: String,
        alias: String,
        passphrase: String,
        start: bool,
    },
    BatchCreateNode {
        nodes: Vec<NodeCreateInfo>,
    },
    ListNodes {
        pagination: PaginationRequest,
    },
    DeleteNode {
        pubkey: String,
    },
    StartNode {
        pubkey: String,
        passphrase: String,
    },
    StopNode {
        pubkey: String,
    },
    CreateToken {
        name: String,
        expires_at: u64,
        scope: String,
        single_use: bool,
    },
    ListTokens {
        pagination: PaginationRequest,
    },
    DeleteToken {
        id: String,
    },
    ConnectGossipPeer {
        node_connection_string: String,
    },
    FindRoute {
        payer_public_key_hex: String,
        route_params_hex: String,
        payment_hash_hex: String,
        first_hops: Vec<String>,
    },
    NodeInfo {
        node_id_hex: String,
    },
    PathSuccessful {
        path: Vec<String>,
    },
    PathFailed {
        path: Vec<String>,
        short_channel_id: u64,
    },
    GossipNodeAnnouncement {
        msg_hex: String,
    },
    GossipChannelAnnouncement {
        msg_hex: String,
    },
    GossipChannelUpdate {
        msg_hex: String,
    },
    GetNetworkGraph {},
}

#[derive(Serialize, Debug)]
#[serde(untagged)]
pub enum AdminResponse {
    GetStatus {
        version: String,
        setup: bool,
        authenticated_node: bool,
        authenticated_admin: bool,
        alias: Option<String>,
        pubkey: Option<String>,
        username: Option<String>,
        role: Option<i16>,
    },
    CreateAdmin {
        token: String,
    },
    CreateNode {
        pubkey: String,
        macaroon: String,
        listen_addr: String,
        listen_port: i32,
        id: String,
    },
    BatchCreateNode {
        nodes: Vec<NodeCreateResult>,
    },
    ListNodes {
        nodes: Vec<node::Model>,
        pagination: PaginationResponse,
    },
    DeleteNode {},
    StartNode {
        macaroon: String,
    },
    StopNode {},
    CreateToken {
        token: access_token::Model,
    },
    ListTokens {
        tokens: Vec<access_token::Model>,
        pagination: PaginationResponse,
    },
    DeleteToken {},
    ConnectGossipPeer {},
    FindRoute {
        route: String,
    },
    NodeInfo {
        node_info: Option<String>,
    },
    PathSuccessful {},
    PathFailed {},
    GossipNodeAnnouncement {},
    GossipChannelAnnouncement {},
    GossipChannelUpdate {},
    GetNetworkGraph {
        nodes: Vec<String>,
        channels: Vec<String>,
    },
    Error(Error),
}

pub type NodeDirectory = Arc<Mutex<HashMap<String, Option<NodeHandle>>>>;

#[derive(Clone)]
pub struct AdminService {
    pub data_dir: String,
    pub config: Arc<SenseiConfig>,
    pub node_directory: NodeDirectory,
    pub database: Arc<SenseiDatabase>,
    pub chain_manager: Arc<SenseiChainManager>,
    pub event_sender: broadcast::Sender<SenseiEvent>,
    pub available_ports: Arc<Mutex<VecDeque<u16>>>,
    pub p2p: Arc<SenseiP2P>,
    pub logger: Arc<FilesystemLogger>,
    pub stop_signal: Arc<AtomicBool>,
}

impl AdminService {
    pub async fn new(
        data_dir: &str,
        config: SenseiConfig,
        database: SenseiDatabase,
        chain_manager: Arc<SenseiChainManager>,
        event_sender: broadcast::Sender<SenseiEvent>,
        runtime_handle: tokio::runtime::Handle,
        stop_signal: Arc<AtomicBool>,
    ) -> Self {
        let mut used_ports = HashSet::new();
        let mut available_ports = VecDeque::new();
        database
            .list_ports_in_use()
            .await
            .unwrap()
            .into_iter()
            .for_each(|port| {
                used_ports.insert(port);
            });

        for port in config.port_range_min..config.port_range_max {
            if !used_ports.contains(&port) {
                available_ports.push_back(port);
            }
        }

        let logger = Arc::new(FilesystemLogger::new(
            String::from(data_dir),
            config.network,
        ));
        let database = Arc::new(database);
        let config = Arc::new(config);
        let p2p = Arc::new(
            SenseiP2P::new(
                config.clone(),
                database.clone(),
                logger.clone(),
                runtime_handle.clone(),
                stop_signal.clone(),
            )
            .await,
        );

        Self {
            data_dir: String::from(data_dir),
            config,
            node_directory: Arc::new(Mutex::new(HashMap::new())),
            database,
            chain_manager,
            event_sender,
            available_ports: Arc::new(Mutex::new(available_ports)),
            logger,
            p2p,
            stop_signal,
        }
    }
}

#[derive(Serialize, Debug)]
pub enum Error {
    Generic(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Generic(e.to_string())
    }
}

impl From<SenseiError> for Error {
    fn from(e: SenseiError) -> Self {
        Self::Generic(e.to_string())
    }
}

impl From<macaroon::MacaroonError> for Error {
    fn from(_e: macaroon::MacaroonError) -> Self {
        Self::Generic(String::from("macaroon error"))
    }
}

impl From<migration::DbErr> for Error {
    fn from(e: migration::DbErr) -> Self {
        Self::Generic(e.to_string())
    }
}

impl AdminService {
    pub async fn call(&self, request: AdminRequest) -> Result<AdminResponse, Error> {
        match request {
            AdminRequest::GetStatus {
                pubkey,
                authenticated_admin,
            } => {
                let setup = self.database.get_root_access_token().await?.is_some();
                match pubkey {
                    Some(pubkey) => {
                        let pubkey_node = self.database.get_node_by_pubkey(&pubkey).await?;
                        match pubkey_node {
                            Some(pubkey_node) => {
                                let directory = self.node_directory.lock().await;
                                let _node_running = directory.contains_key(&pubkey);

                                Ok(AdminResponse::GetStatus {
                                    version: version::get_version(),
                                    alias: Some(pubkey_node.alias),
                                    setup,
                                    authenticated_admin,
                                    authenticated_node: true,
                                    pubkey: Some(pubkey_node.pubkey),
                                    username: Some(pubkey_node.username),
                                    role: Some(pubkey_node.role),
                                })
                            }
                            None => Ok(AdminResponse::GetStatus {
                                version: version::get_version(),
                                alias: None,
                                setup,
                                authenticated_admin,
                                authenticated_node: false,
                                pubkey: None,
                                username: None,
                                role: None,
                            }),
                        }
                    }
                    None => Ok(AdminResponse::GetStatus {
                        version: version::get_version(),
                        alias: None,
                        setup,
                        authenticated_admin,
                        authenticated_node: false,
                        pubkey: None,
                        username: None,
                        role: None,
                    }),
                }
            }
            AdminRequest::CreateAdmin {
                username,
                passphrase,
            } => {
                let root_token = self.database.create_root_access_token().await.unwrap();
                let _user = self
                    .database
                    .create_user(username, passphrase)
                    .await
                    .unwrap();

                Ok(AdminResponse::CreateAdmin {
                    token: root_token.token,
                })
            }
            AdminRequest::StartNode { pubkey, passphrase } => {
                let node = self.database.get_node_by_pubkey(&pubkey).await?;
                match node {
                    Some(node) => {
                        let macaroon = LightningNode::get_macaroon_for_node(
                            &node.id,
                            &passphrase,
                            self.database.clone(),
                        )
                        .await?;
                        let macaroon = macaroon.serialize(macaroon::Format::V2)?;
                        self.start_node(node, passphrase).await?;
                        Ok(AdminResponse::StartNode {
                            macaroon: hex_utils::hex_str(macaroon.as_slice()),
                        })
                    }
                    None => Err(Error::Generic(String::from("node not found"))),
                }
            }
            AdminRequest::StopNode { pubkey } => {
                let node = self.database.get_node_by_pubkey(&pubkey).await?;
                match node {
                    Some(node) => {
                        self.stop_node(pubkey).await?;

                        let mut node: node::ActiveModel = node.into();
                        node.status = ActiveValue::Set(node::NodeStatus::Stopped.into());
                        node.update(self.database.get_connection()).await?;

                        Ok(AdminResponse::StopNode {})
                    }
                    None => {
                        // try stopping it anyway?
                        Ok(AdminResponse::StopNode {})
                    }
                }
            }
            AdminRequest::CreateNode {
                username,
                alias,
                passphrase,
                start,
            } => {
                let (node, macaroon) = self
                    .create_node(username, alias, passphrase.clone(), node::NodeRole::Default)
                    .await?;

                let macaroon = macaroon.serialize(macaroon::Format::V2)?;

                if start {
                    self.start_node(node.clone(), passphrase).await?;
                }
                Ok(AdminResponse::CreateNode {
                    pubkey: node.pubkey,
                    macaroon: hex_utils::hex_str(macaroon.as_slice()),
                    listen_addr: node.listen_addr,
                    listen_port: node.listen_port,
                    id: node.id,
                })
            }
            AdminRequest::BatchCreateNode { nodes } => {
                let nodes_and_macaroons = self.batch_create_nodes(nodes.clone()).await?;

                for ((node, _macaroon), node_create_info) in
                    nodes_and_macaroons.iter().zip(nodes.iter())
                {
                    if node_create_info.start {
                        self.start_node(node.clone(), node_create_info.passphrase.clone())
                            .await?;
                    }
                }

                Ok(AdminResponse::BatchCreateNode {
                    nodes: nodes_and_macaroons
                        .into_iter()
                        .map(|(node, macaroon)| {
                            let macaroon = macaroon.serialize(macaroon::Format::V2).unwrap();
                            NodeCreateResult {
                                pubkey: node.pubkey,
                                macaroon: hex_utils::hex_str(macaroon.as_slice()),
                                listen_addr: node.listen_addr,
                                listen_port: node.listen_port,
                                id: node.id,
                            }
                        })
                        .collect::<Vec<_>>(),
                })
            }
            AdminRequest::ListNodes { pagination } => {
                let (nodes, pagination) = self.list_nodes(pagination).await?;
                Ok(AdminResponse::ListNodes { nodes, pagination })
            }
            AdminRequest::DeleteNode { pubkey } => {
                let node = self.database.get_node_by_pubkey(&pubkey).await?;
                match node {
                    Some(node) => {
                        self.delete_node(node).await?;
                        Ok(AdminResponse::DeleteNode {})
                    }
                    None => Err(Error::Generic(String::from("node not found"))),
                }
            }
            AdminRequest::CreateToken {
                name,
                expires_at,
                scope,
                single_use,
            } => {
                let access_token = self
                    .database
                    .create_access_token(name, scope, expires_at.try_into().unwrap(), single_use)
                    .await?;

                Ok(AdminResponse::CreateToken {
                    token: access_token,
                })
            }
            AdminRequest::ListTokens { pagination } => {
                let (tokens, pagination) = self.list_tokens(pagination).await?;
                Ok(AdminResponse::ListTokens { tokens, pagination })
            }
            AdminRequest::DeleteToken { id } => {
                self.database.delete_access_token(id).await?;
                Ok(AdminResponse::DeleteToken {})
            }
            AdminRequest::ConnectGossipPeer {
                node_connection_string,
            } => {
                let (pubkey, addr) = parse_peer_info(node_connection_string).await?;

                let _res = self
                    .p2p
                    .peer_connector
                    .connect_routing_peer(pubkey, addr)
                    .await;

                Ok(AdminResponse::ConnectGossipPeer {})
            }
            AdminRequest::FindRoute {
                payer_public_key_hex,
                route_params_hex,
                payment_hash_hex,
                first_hops,
            } => {
                let payer = hex_utils::to_compressed_pubkey(&payer_public_key_hex)
                    .expect("valid payer public key hex");
                let mut route_params_readable =
                    Cursor::new(hex_utils::to_vec(&route_params_hex).unwrap());
                let mut payment_hash_readable =
                    Cursor::new(hex_utils::to_vec(&payment_hash_hex).unwrap());
                let first_hops = first_hops
                    .iter()
                    .map(|hop| {
                        let mut channel_details_readable =
                            Cursor::new(hex_utils::to_vec(hop).unwrap());
                        ChannelDetails::read(&mut channel_details_readable).unwrap()
                    })
                    .collect::<Vec<_>>();

                let route_params = RouteParameters::read(&mut route_params_readable).unwrap();
                let payment_hash = PaymentHash::read(&mut payment_hash_readable).unwrap();

                let scorer = self.p2p.scorer.lock().unwrap();
                let router = self.p2p.get_router();
                router
                    .find_route(
                        &payer,
                        &route_params,
                        &payment_hash,
                        Some(&first_hops.iter().collect::<Vec<_>>()),
                        &scorer,
                    )
                    .map(|route| AdminResponse::FindRoute {
                        route: hex_utils::hex_str(&route.encode()),
                    })
                    .map_err(|e| Error::Generic(format!("{:?}", e)))
            }
            AdminRequest::NodeInfo { node_id_hex } => {
                let mut node_id_readable = Cursor::new(hex_utils::to_vec(&node_id_hex).unwrap());
                let node_id = NodeId::read(&mut node_id_readable).unwrap();
                let network_graph = self.p2p.network_graph.read_only();
                Ok(AdminResponse::NodeInfo {
                    node_info: network_graph
                        .nodes()
                        .get(&node_id)
                        .map(|node_info| hex_utils::hex_str(&node_info.encode())),
                })
            }
            AdminRequest::PathSuccessful { path } => {
                let path = path
                    .iter()
                    .map(|route_hop| {
                        let mut readable_hop = Cursor::new(hex_utils::to_vec(route_hop).unwrap());
                        RouteHop::read(&mut readable_hop).unwrap()
                    })
                    .collect::<Vec<_>>();
                let mut scorer = self.p2p.scorer.lock().unwrap();
                scorer.payment_path_successful(&path.iter().collect::<Vec<_>>());
                Ok(AdminResponse::PathSuccessful {})
            }
            AdminRequest::PathFailed {
                path,
                short_channel_id,
            } => {
                let path = path
                    .iter()
                    .map(|route_hop| {
                        let mut readable_hop = Cursor::new(hex_utils::to_vec(route_hop).unwrap());
                        RouteHop::read(&mut readable_hop).unwrap()
                    })
                    .collect::<Vec<_>>();
                let mut scorer = self.p2p.scorer.lock().unwrap();
                scorer.payment_path_failed(&path.iter().collect::<Vec<_>>(), short_channel_id);
                Ok(AdminResponse::PathFailed {})
            }
            AdminRequest::GossipNodeAnnouncement { msg_hex } => {
                let mut msg_readable = Cursor::new(hex_utils::to_vec(&msg_hex).unwrap());
                let msg = lightning::ln::msgs::NodeAnnouncement::read(&mut msg_readable).unwrap();
                let _res = self.p2p.p2p_gossip.handle_node_announcement(&msg);
                Ok(AdminResponse::GossipNodeAnnouncement {})
            }
            AdminRequest::GossipChannelAnnouncement { msg_hex } => {
                let mut msg_readable = Cursor::new(hex_utils::to_vec(&msg_hex).unwrap());
                let msg =
                    lightning::ln::msgs::ChannelAnnouncement::read(&mut msg_readable).unwrap();
                let _res = self.p2p.p2p_gossip.handle_channel_announcement(&msg);
                Ok(AdminResponse::GossipChannelAnnouncement {})
            }
            AdminRequest::GossipChannelUpdate { msg_hex } => {
                let mut msg_readable = Cursor::new(hex_utils::to_vec(&msg_hex).unwrap());
                let msg = lightning::ln::msgs::ChannelUpdate::read(&mut msg_readable).unwrap();
                let _res = self.p2p.p2p_gossip.handle_channel_update(&msg);
                Ok(AdminResponse::GossipChannelUpdate {})
            }
            AdminRequest::GetNetworkGraph {} => {
                let graph = self.p2p.network_graph.read_only();
                let channels = graph.channels();
                let nodes = graph.nodes();
                Ok(AdminResponse::GetNetworkGraph {
                    channels: channels
                        .iter()
                        .map(|(_scid, info)| {
                            let node_one = nodes
                                .get(&info.node_one)
                                .and_then(|info| {
                                    info.announcement_info
                                        .as_ref()
                                        .map(|info| info.alias.to_string())
                                })
                                .unwrap_or(format!("{:?}", info.node_one));

                            let node_two = nodes
                                .get(&info.node_two)
                                .and_then(|info| {
                                    info.announcement_info
                                        .as_ref()
                                        .map(|info| info.alias.to_string())
                                })
                                .unwrap_or(format!("{:?}", info.node_two));

                            format!("{:?} <=> {:?}", node_one, node_two)
                        })
                        .collect::<Vec<String>>(),
                    nodes: nodes
                        .iter()
                        .map(|(node_id, info)| {
                            info.announcement_info
                                .as_ref()
                                .map(|info| info.alias.to_string())
                                .unwrap_or(format!("{:?}", node_id))
                        })
                        .collect::<Vec<String>>(),
                })
            }
        }
    }

    async fn list_tokens(
        &self,
        pagination: PaginationRequest,
    ) -> Result<(Vec<access_token::Model>, PaginationResponse), crate::error::Error> {
        self.database.list_access_tokens(pagination).await
    }

    async fn list_nodes(
        &self,
        pagination: PaginationRequest,
    ) -> Result<(Vec<node::Model>, PaginationResponse), crate::error::Error> {
        self.database.list_nodes(pagination).await
    }

    async fn batch_create_nodes(
        &self,
        nodes: Vec<NodeCreateInfo>,
    ) -> Result<Vec<(node::Model, Macaroon)>, crate::error::Error> {
        let built_node_futures = nodes
            .into_iter()
            .map(|info| {
                self.build_node(
                    info.username,
                    info.alias,
                    info.passphrase,
                    NodeRole::Default,
                )
            })
            .collect::<Vec<_>>();

        let stream_of_futures = stream::iter(built_node_futures);
        let buffered = stream_of_futures.buffer_unordered(10);
        let mut built_nodes = buffered
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|built_result| built_result.unwrap())
            .collect::<Vec<_>>();

        let mut nodes_with_macaroons = Vec::with_capacity(built_nodes.len());
        let mut db_nodes = Vec::with_capacity(built_nodes.len());
        let mut db_seeds = Vec::with_capacity(built_nodes.len());
        let mut db_macaroons = Vec::with_capacity(built_nodes.len());

        for (node, macaroon, db_node, db_seed, db_macaroon) in built_nodes.drain(..) {
            nodes_with_macaroons.push((node, macaroon));
            db_nodes.push(db_node);
            db_seeds.push(db_seed);
            db_macaroons.push(db_macaroon);
        }

        entity::node::Entity::insert_many(db_nodes)
            .exec(self.database.get_connection())
            .await?;
        entity::kv_store::Entity::insert_many(db_seeds)
            .exec(self.database.get_connection())
            .await?;
        entity::macaroon::Entity::insert_many(db_macaroons)
            .exec(self.database.get_connection())
            .await?;

        Ok(nodes_with_macaroons)
    }

    async fn build_node(
        &self,
        username: String,
        alias: String,
        passphrase: String,
        role: node::NodeRole,
    ) -> Result<
        (
            entity::node::Model,
            Macaroon,
            entity::node::ActiveModel,
            entity::kv_store::ActiveModel,
            entity::macaroon::ActiveModel,
        ),
        crate::error::Error,
    > {
        // IP/PORT
        let listen_addr = self.config.api_host.clone();

        let listen_port: i32 = match role {
            node::NodeRole::Default => {
                let mut available_ports = self.available_ports.lock().await;
                available_ports.pop_front().unwrap().into()
            }
        };

        // NODE ID
        let node_id = Uuid::new_v4().to_string();

        // NODE DIRECTORY
        let node_directory = format!("{}/{}/{}", self.data_dir, self.config.network, node_id);
        fs::create_dir_all(node_directory)?;

        // NODE SEED
        let seed = LightningNode::generate_seed();
        let encrypted_seed = LightningNode::encrypt_seed(&seed, passphrase.as_bytes())?;

        let seed_active_model = self
            .database
            .get_seed_active_model(node_id.clone(), encrypted_seed);

        // NODE PUBKEY
        let node_pubkey = LightningNode::get_node_pubkey_from_seed(&seed);

        // NODE MACAROON
        let (macaroon, macaroon_id) = LightningNode::generate_macaroon(&seed, node_pubkey.clone())?;

        let encrypted_macaroon = LightningNode::encrypt_macaroon(&macaroon, passphrase.as_bytes())?;

        let now = seconds_since_epoch();

        let db_macaroon = entity::macaroon::ActiveModel {
            id: ActiveValue::Set(macaroon_id),
            node_id: ActiveValue::Set(node_id.clone()),
            encrypted_macaroon: ActiveValue::Set(encrypted_macaroon),
            created_at: ActiveValue::Set(now),
            updated_at: ActiveValue::Set(now),
        };

        // NODE
        let active_node = entity::node::ActiveModel {
            id: ActiveValue::Set(node_id.clone()),
            pubkey: ActiveValue::Set(node_pubkey.clone()),
            username: ActiveValue::Set(username.clone()),
            alias: ActiveValue::Set(alias.clone()),
            network: ActiveValue::Set(self.config.network.to_string()),
            listen_addr: ActiveValue::Set(listen_addr.clone()),
            listen_port: ActiveValue::Set(listen_port),
            role: ActiveValue::Set(role.clone().into()),
            status: ActiveValue::Set(node::NodeStatus::Stopped.into()),
            created_at: ActiveValue::Set(now),
            updated_at: ActiveValue::Set(now),
        };

        let node = node::Model {
            id: node_id,
            role: role.into(),
            username,
            alias,
            network: self.config.network.to_string(),
            listen_addr,
            listen_port,
            pubkey: node_pubkey,
            created_at: now,
            updated_at: now,
            status: node::NodeStatus::Stopped.into(),
        };

        Ok((node, macaroon, active_node, seed_active_model, db_macaroon))
    }

    async fn create_node(
        &self,
        username: String,
        alias: String,
        passphrase: String,
        role: node::NodeRole,
    ) -> Result<(node::Model, Macaroon), crate::error::Error> {
        let (node, macaroon, db_node, db_seed, db_macaroon) =
            self.build_node(username, alias, passphrase, role).await?;

        db_seed.insert(self.database.get_connection()).await?;
        db_macaroon.insert(self.database.get_connection()).await?;
        db_node.insert(self.database.get_connection()).await?;

        Ok((node, macaroon))
    }

    // note: please be sure to stop the node first? maybe?
    // TODO: this was never updated with the DB rewrite
    //       need to release the port and actually delete the node
    async fn delete_node(&self, node: node::Model) -> Result<(), crate::error::Error> {
        let data_dir = format!("{}/{}/{}", self.data_dir, self.config.network, node.id);
        Ok(fs::remove_dir_all(&data_dir)?)
    }

    async fn start_node(
        &self,
        node: node::Model,
        passphrase: String,
    ) -> Result<(), crate::error::Error> {
        let status = {
            let mut node_directory = self.node_directory.lock().await;
            match node_directory.entry(node.pubkey.clone()) {
                Entry::Vacant(entry) => {
                    entry.insert(None);
                    None
                }
                Entry::Occupied(entry) => {
                    // TODO: verify passphrase
                    match entry.get() {
                        Some(_handle) => Some(Some(())),
                        None => Some(None),
                    }
                }
            }
        };
        match status {
            None => {
                let (lightning_node, handles, background_processor) = LightningNode::new(
                    self.config.clone(),
                    node.id.clone(),
                    vec![node.listen_addr.clone()],
                    node.listen_port.try_into().unwrap(),
                    node.alias.clone(),
                    format!(
                        "{}/{}/{}",
                        self.data_dir,
                        self.config.network,
                        node.id.clone()
                    ),
                    passphrase,
                    self.p2p.clone(),
                    self.chain_manager.clone(),
                    self.database.clone(),
                    self.event_sender.clone(),
                )
                .await?;

                println!(
                    "starting {}@{}:{}",
                    node.pubkey.clone(),
                    self.config.api_host.clone(),
                    node.listen_port
                );

                {
                    let mut node_directory = self.node_directory.lock().await;
                    if let Entry::Occupied(mut entry) = node_directory.entry(node.pubkey.clone()) {
                        entry.insert(Some(NodeHandle {
                            node: Arc::new(lightning_node.clone()),
                            background_processor,
                            handles,
                        }));
                    }
                }

                let mut node: node::ActiveModel = node.into();
                node.status = ActiveValue::Set(node::NodeStatus::Running.into());
                node.listen_addr = ActiveValue::Set(self.config.api_host.clone());
                node.save(self.database.get_connection()).await?;

                Ok(())
            }
            Some(None) => Ok(()),
            Some(Some(_)) => Ok(()),
        }
    }

    async fn stop_node(&self, pubkey: String) -> Result<(), crate::error::Error> {
        let mut node_directory = self.node_directory.lock().await;
        let entry = node_directory.entry(pubkey.clone());

        if let Entry::Occupied(entry) = entry {
            if let Some(node_handle) = entry.remove() {
                // Disconnect our peers and stop accepting new connections. This ensures we don't continue
                // updating our channel data after we've stopped the background processor.
                node_handle.node.peer_manager.disconnect_all_peers();
                node_handle.node.stop_listen.store(true, Ordering::Release);
                self.p2p
                    .peer_connector
                    .unregister_node(node_handle.node.id.clone());
                let _res = node_handle.background_processor.stop();
                for handle in node_handle.handles {
                    handle.abort();
                }

                match self.database.get_node_by_pubkey(&pubkey).await? {
                    Some(node) => {
                        let mut node: node::ActiveModel = node.into();
                        node.status = ActiveValue::Set(node::NodeStatus::Stopped.into());
                        node.save(self.database.get_connection()).await?;
                    }
                    None => {
                        // TODO: this shouldn't be possible.  surface this some other way?
                        println!("couldn't find node pubkey in database while stopping");
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn stop(&self) -> Result<(), crate::error::Error> {
        println!("sensei is preparing to shut down...");
        let pubkeys = {
            let node_directory = self.node_directory.lock().await;
            node_directory.keys().cloned().collect::<Vec<String>>()
        };

        for pubkey in pubkeys.into_iter() {
            self.stop_node(pubkey).await.unwrap();
        }

        self.chain_manager.stop().await;
        self.p2p.stop().await;
        println!("all set, goodbye!");
        Ok(())
    }
}
