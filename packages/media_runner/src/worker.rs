use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Instant};

use atm0s_sdn::{
    generate_node_addr,
    secure::{HandshakeBuilderXDA, StaticKeyAuthorization},
    services::{manual_discovery, visualization},
    ControllerPlaneCfg, DataPlaneCfg, DataWorkerHistory, NetInput, NetOutput, NodeAddr, SdnExtIn, SdnExtOut, SdnWorker, SdnWorkerBusEvent, SdnWorkerCfg, SdnWorkerInput, SdnWorkerOutput, TimePivot,
};
use atm0s_sdn_network::data_plane::NetPair;
use media_server_connector::agent_service::ConnectorAgentServiceBuilder;
use media_server_core::cluster::{self, MediaCluster};
use media_server_gateway::{agent_service::GatewayAgentServiceBuilder, NodeMetrics, ServiceKind, AGENT_SERVICE_ID};
use media_server_protocol::{
    cluster::{ClusterMediaInfo, ClusterNodeGenericInfo, ClusterNodeInfo},
    gateway::generate_gateway_zone_tag,
    protobuf::{
        cluster_connector::{connector_request, PeerEvent},
        gateway::{ConnectResponse, RemoteIceResponse},
    },
    record::SessionRecordEvent,
    transport::{
        webrtc,
        whep::{self, WhepConnectRes, WhepDeleteRes, WhepRemoteIceRes},
        whip::{self, WhipConnectRes, WhipDeleteRes, WhipRemoteIceRes},
        RpcReq, RpcRes,
    },
};
use media_server_secure::MediaEdgeSecure;
use rand::{random, rngs::OsRng};
use sans_io_runtime::{
    backend::{BackendIncoming, BackendOutgoing},
    collections::DynamicDeque,
    TaskSwitcher, TaskSwitcherBranch,
};
use transport_webrtc::{GroupInput, MediaWorkerWebrtc, VariantParams, WebrtcSession};

const FEEDBACK_GATEWAY_AGENT_INTERVAL: u64 = 1000; //only feedback every second

pub struct MediaConfig<ES> {
    pub ice_lite: bool,
    pub webrtc_addrs: Vec<SocketAddr>,
    pub webrtc_addrs_alt: Vec<SocketAddr>,
    pub secure: Arc<ES>,
    pub max_live: HashMap<ServiceKind, u32>,
}

pub type SdnConfig = SdnWorkerCfg<UserData, SC, SE, TC, TW>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Sdn,
    MediaWebrtc,
}

//for sdn
#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub enum UserData {
    Cluster,
    Room(cluster::RoomUserData),
    Record(u64),
}
#[derive(Clone, Debug, convert_enum::From, convert_enum::TryInto)]
pub enum SC {
    Visual(visualization::Control<ClusterNodeInfo>),
    Gateway(media_server_gateway::agent_service::Control),
    Connector(media_server_connector::agent_service::Control),
}

#[derive(Clone, Debug, convert_enum::From, convert_enum::TryInto)]
pub enum SE {
    Visual(visualization::Event<ClusterNodeInfo>),
    Gateway(media_server_gateway::agent_service::Event),
    Connector(media_server_connector::agent_service::Event),
}
pub type TC = ();
pub type TW = ();

pub enum Input {
    NodeStats(NodeMetrics),
    ExtRpc(u64, RpcReq<usize>),
    /// ext, is_controller
    ExtSdn(SdnExtIn<UserData, SC>, bool),
    Net(Owner, BackendIncoming),
    Bus(SdnWorkerBusEvent<UserData, SC, SE, TC, TW>),
}

pub enum Output {
    ExtRpc(u64, RpcRes<usize>),
    ExtSdn(SdnExtOut<UserData, SE>),
    Net(Owner, BackendOutgoing),
    Bus(SdnWorkerBusEvent<UserData, SC, SE, TC, TW>),
    Record(u64, Instant, SessionRecordEvent),
    Continue,
}

#[derive(num_enum::TryFromPrimitive, num_enum::IntoPrimitive)]
#[repr(usize)]
enum TaskType {
    Sdn,
    MediaCluster,
    MediaWebrtc,
}

#[derive(convert_enum::From, Debug, Clone, Copy, Hash, PartialEq, Eq)]
enum MediaClusterEndpoint {
    Webrtc(WebrtcSession),
}

#[allow(clippy::type_complexity)]
pub struct MediaServerWorker<ES: 'static + MediaEdgeSecure> {
    worker: u16,
    sdn_addr: NodeAddr,
    sdn_worker: TaskSwitcherBranch<SdnWorker<UserData, SC, SE, TC, TW>, SdnWorkerOutput<UserData, SC, SE, TC, TW>>,
    sdn_backend_addrs: HashMap<SocketAddr, usize>,
    sdn_backend_slots: HashMap<usize, SocketAddr>,
    media_cluster: TaskSwitcherBranch<MediaCluster<MediaClusterEndpoint>, cluster::Output<MediaClusterEndpoint>>,
    media_webrtc: TaskSwitcherBranch<MediaWorkerWebrtc<ES>, transport_webrtc::GroupOutput>,
    media_max_live: u32,
    switcher: TaskSwitcher,
    queue: DynamicDeque<Output, 16>,
    timer: TimePivot,
    last_feedback_gateway_agent: u64,
    secure: Arc<ES>,
}

impl<ES: 'static + MediaEdgeSecure> MediaServerWorker<ES> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        worker: u16,
        node_id: u32,
        session: u64,
        secret: &str,
        controller: bool,
        sdn_bind_addrs: Vec<SocketAddr>,
        sdn_custom_addrs: Vec<SocketAddr>,
        sdn_zone: u32,
        media: MediaConfig<ES>,
    ) -> Self {
        let secure = media.secure.clone(); //TODO why need this?
        let mut media_max_live = 0;
        for (_, max) in media.max_live.iter() {
            media_max_live += *max;
        }
        let node_addr = generate_node_addr(node_id, &sdn_bind_addrs, sdn_custom_addrs);
        let node_info = ClusterNodeInfo::Media(
            ClusterNodeGenericInfo {
                addr: node_addr.to_string(),
                cpu: 0,
                memory: 0,
                disk: 0,
            },
            ClusterMediaInfo { live: 0, max: media_max_live },
        );

        let visualization = Arc::new(visualization::VisualizationServiceBuilder::new(node_info, false));
        let discovery = Arc::new(manual_discovery::ManualDiscoveryServiceBuilder::new(
            node_addr.clone(),
            vec![],
            vec![generate_gateway_zone_tag(sdn_zone)],
        ));
        let gateway = Arc::new(GatewayAgentServiceBuilder::new(media.max_live));
        let connector = Arc::new(ConnectorAgentServiceBuilder::new());
        let history = Arc::new(DataWorkerHistory::default());

        let sdn_config = SdnConfig {
            node_id,
            controller: if controller {
                Some(ControllerPlaneCfg {
                    session,
                    bind_addrs: sdn_bind_addrs.clone(),
                    authorization: Arc::new(StaticKeyAuthorization::new(secret)),
                    handshake_builder: Arc::new(HandshakeBuilderXDA),
                    random: Box::new(OsRng),
                    services: vec![visualization.clone(), discovery.clone(), gateway.clone(), connector.clone()],
                    history: history.clone(),
                })
            } else {
                None
            },
            tick_ms: 1000,
            data: DataPlaneCfg {
                worker_id: 0,
                services: vec![visualization, discovery, gateway],
                history,
            },
        };

        let mut queue = DynamicDeque::default();
        for addr in sdn_bind_addrs {
            queue.push_back(Output::Net(Owner::Sdn, BackendOutgoing::UdpListen { addr, reuse: true }));
        }

        Self {
            worker,
            sdn_addr: node_addr,
            sdn_worker: TaskSwitcherBranch::new(SdnWorker::new(sdn_config), TaskType::Sdn),
            media_cluster: TaskSwitcherBranch::default(TaskType::MediaCluster),
            media_webrtc: TaskSwitcherBranch::new(MediaWorkerWebrtc::new(media.webrtc_addrs, media.webrtc_addrs_alt, media.ice_lite, media.secure), TaskType::MediaWebrtc),
            media_max_live,
            switcher: TaskSwitcher::new(3),
            queue,
            timer: TimePivot::build(),
            last_feedback_gateway_agent: 0,
            secure,
            sdn_backend_addrs: Default::default(),
            sdn_backend_slots: Default::default(),
        }
    }

    pub fn tasks(&self) -> usize {
        self.sdn_worker.tasks() + self.sdn_worker.tasks()
    }

    pub fn on_tick(&mut self, now: Instant) {
        let s = &mut self.switcher;
        let now_ms = self.timer.timestamp_ms(now);
        self.sdn_worker.input(s).on_tick(now_ms);
        self.media_cluster.input(s).on_tick(now);
        self.media_webrtc.input(s).on_tick(now);

        if self.last_feedback_gateway_agent + FEEDBACK_GATEWAY_AGENT_INTERVAL <= now_ms {
            self.last_feedback_gateway_agent = now_ms;

            let webrtc_live = self.media_webrtc.tasks() as u32;
            self.sdn_worker.input(s).on_event(
                now_ms,
                SdnWorkerInput::ExtWorker(SdnExtIn::ServicesControl(
                    AGENT_SERVICE_ID.into(),
                    UserData::Cluster,
                    media_server_gateway::agent_service::Control::WorkerUsage(ServiceKind::Webrtc, self.worker, webrtc_live).into(),
                )),
            );
        }
    }

    pub fn on_event(&mut self, now: Instant, input: Input) {
        match input {
            Input::NodeStats(metrics) => {
                let now_ms = self.timer.timestamp_ms(now);
                // we send info to visualization for console UI
                self.sdn_worker.input(&mut self.switcher).on_event(
                    now_ms,
                    SdnWorkerInput::ExtWorker(SdnExtIn::ServicesControl(
                        visualization::SERVICE_ID.into(),
                        UserData::Cluster,
                        visualization::Control::UpdateInfo(ClusterNodeInfo::Media(
                            ClusterNodeGenericInfo {
                                addr: self.sdn_addr.to_string(),
                                cpu: metrics.cpu,
                                memory: metrics.memory,
                                disk: metrics.disk,
                            },
                            ClusterMediaInfo {
                                live: self.media_webrtc.tasks() as u32,
                                max: self.media_max_live,
                            },
                        ))
                        .into(),
                    )),
                );
                self.sdn_worker.input(&mut self.switcher).on_event(
                    now_ms,
                    SdnWorkerInput::ExtWorker(SdnExtIn::ServicesControl(
                        AGENT_SERVICE_ID.into(),
                        UserData::Cluster,
                        media_server_gateway::agent_service::Control::NodeStats(metrics).into(),
                    )),
                );
            }
            Input::ExtRpc(req_id, req) => self.process_rpc(now, req_id, req),
            Input::ExtSdn(ext, is_controller) => {
                let now_ms = self.timer.timestamp_ms(now);
                if is_controller {
                    self.sdn_worker.input(&mut self.switcher).on_event(now_ms, SdnWorkerInput::Ext(ext));
                } else {
                    self.sdn_worker.input(&mut self.switcher).on_event(now_ms, SdnWorkerInput::ExtWorker(ext));
                }
            }
            Input::Net(owner, event) => match owner {
                Owner::Sdn => {
                    let now_ms = self.timer.timestamp_ms(now);
                    match event {
                        BackendIncoming::UdpPacket { slot, from, data } => {
                            let local = self.sdn_backend_slots.get(&slot).expect("Should have local addr");
                            self.sdn_worker
                                .input(&mut self.switcher)
                                .on_event(now_ms, SdnWorkerInput::Net(NetInput::UdpPacket(NetPair::new(*local, from), data)));
                        }
                        BackendIncoming::UdpListenResult { bind, result } => {
                            if let Ok((addr, slot)) = result {
                                log::info!("[MediaServerWorker] sdn listen success on {addr}, slot {slot}");
                                self.sdn_backend_addrs.insert(addr, slot);
                                self.sdn_backend_slots.insert(slot, addr);
                            } else {
                                log::warn!("[MediaServerWorker] sdn listen error on {bind}");
                            }
                        }
                    }
                }
                Owner::MediaWebrtc => {
                    self.media_webrtc.input(&mut self.switcher).on_event(now, transport_webrtc::GroupInput::Net(event));
                }
            },
            Input::Bus(event) => {
                let now_ms = self.timer.timestamp_ms(now);
                self.sdn_worker.input(&mut self.switcher).on_event(now_ms, SdnWorkerInput::Bus(event));
            }
        }
    }

    pub fn pop_output(&mut self, now: Instant) -> Option<Output> {
        if let Some(out) = self.queue.pop_front() {
            return Some(out);
        }
        while let Some(c) = self.switcher.current() {
            match c.try_into().ok()? {
                TaskType::Sdn => {
                    let now_ms = self.timer.timestamp_ms(now);
                    if let Some(out) = self.sdn_worker.pop_output(now_ms, &mut self.switcher) {
                        return Some(self.output_sdn(now, out));
                    }
                }
                TaskType::MediaCluster => {
                    if let Some(out) = self.media_cluster.pop_output((), &mut self.switcher) {
                        return Some(self.output_cluster(now, out));
                    }
                }
                TaskType::MediaWebrtc => {
                    if let Some(out) = self.media_webrtc.pop_output(now, &mut self.switcher) {
                        return Some(self.output_webrtc(now, out));
                    }
                }
            }
        }
        None
    }

    pub fn shutdown(&mut self, now: Instant) {
        let now_ms = self.timer.timestamp_ms(now);
        self.sdn_worker.input(&mut self.switcher).on_event(now_ms, SdnWorkerInput::ShutdownRequest);
        self.media_cluster.input(&mut self.switcher).shutdown(now);
        self.media_webrtc.input(&mut self.switcher).shutdown(now);
    }
}

impl<ES: 'static + MediaEdgeSecure> MediaServerWorker<ES> {
    fn output_sdn(&mut self, now: Instant, out: SdnWorkerOutput<UserData, SC, SE, TC, TW>) -> Output {
        match out {
            SdnWorkerOutput::Ext(out) => Output::ExtSdn(out),
            SdnWorkerOutput::ExtWorker(out) => match out {
                SdnExtOut::FeaturesEvent(UserData::Cluster, _event) => Output::Continue,
                SdnExtOut::FeaturesEvent(UserData::Room(room), event) => {
                    self.media_cluster.input(&mut self.switcher).on_sdn_event(now, room, event);
                    Output::Continue
                }
                SdnExtOut::FeaturesEvent(UserData::Record(..), ..) => Output::Continue,
                SdnExtOut::ServicesEvent(..) => Output::Continue,
            },
            SdnWorkerOutput::Net(out) => match out {
                NetOutput::UdpPacket(pair, data) => {
                    if let Some(slot) = self.sdn_backend_addrs.get(&pair.local) {
                        Output::Net(Owner::Sdn, BackendOutgoing::UdpPacket { slot: *slot, to: pair.remote, data })
                    } else {
                        Output::Continue
                    }
                }
                NetOutput::UdpPackets(pairs, data) => {
                    let to = pairs.into_iter().filter_map(|p| self.sdn_backend_addrs.get(&p.local).map(|s| (*s, p.remote))).collect::<Vec<_>>();
                    if to.is_empty() {
                        Output::Continue
                    } else {
                        Output::Net(Owner::Sdn, BackendOutgoing::UdpPackets2 { to, data })
                    }
                }
            },
            SdnWorkerOutput::Bus(event) => Output::Bus(event),
            SdnWorkerOutput::ShutdownResponse => Output::Continue,
            SdnWorkerOutput::Continue => Output::Continue,
        }
    }

    fn output_cluster(&mut self, now: Instant, out: cluster::Output<MediaClusterEndpoint>) -> Output {
        match out {
            cluster::Output::Sdn(room, control) => {
                let now_ms = self.timer.timestamp_ms(now);
                self.sdn_worker
                    .input(&mut self.switcher)
                    .on_event(now_ms, SdnWorkerInput::ExtWorker(SdnExtIn::FeaturesControl(UserData::Room(room), control)));
                Output::Continue
            }
            cluster::Output::Endpoint(endpoints, event) => {
                for endpoint in endpoints {
                    match endpoint {
                        MediaClusterEndpoint::Webrtc(session) => {
                            self.media_webrtc.input(&mut self.switcher).on_event(now, transport_webrtc::GroupInput::Cluster(session, event.clone()));
                        }
                    }
                }
                Output::Continue
            }
            cluster::Output::Continue => Output::Continue,
        }
    }

    fn output_webrtc(&mut self, now: Instant, out: transport_webrtc::GroupOutput) -> Output {
        match out {
            transport_webrtc::GroupOutput::Net(out) => Output::Net(Owner::MediaWebrtc, out),
            transport_webrtc::GroupOutput::Cluster(session, room, control) => {
                self.media_cluster.input(&mut self.switcher).on_endpoint_control(now, session.into(), room, control);
                Output::Continue
            }
            transport_webrtc::GroupOutput::PeerEvent(_, session_id, ts, event) => {
                let now_ms = self.timer.timestamp_ms(now);
                self.sdn_worker.input(&mut self.switcher).on_event(
                    now_ms,
                    SdnWorkerInput::ExtWorker(SdnExtIn::ServicesControl(
                        media_server_connector::AGENT_SERVICE_ID.into(),
                        UserData::Cluster,
                        media_server_connector::agent_service::Control::Request(self.timer.timestamp_ms(ts), connector_request::Request::Peer(PeerEvent { session_id, event: Some(event) })).into(),
                    )),
                );
                Output::Continue
            }
            transport_webrtc::GroupOutput::RecordEvent(_, session_id, ts, event) => Output::Record(session_id, ts, event),
            transport_webrtc::GroupOutput::Ext(session, ext) => match ext {
                transport_webrtc::ExtOut::RemoteIce(req_id, variant, res) => match variant {
                    transport_webrtc::Variant::Whip => Output::ExtRpc(req_id, RpcRes::Whip(whip::RpcRes::RemoteIce(res.map(|_| WhipRemoteIceRes {})))),
                    transport_webrtc::Variant::Whep => Output::ExtRpc(req_id, RpcRes::Whep(whep::RpcRes::RemoteIce(res.map(|_| WhepRemoteIceRes {})))),
                    transport_webrtc::Variant::Webrtc => Output::ExtRpc(req_id, RpcRes::Webrtc(webrtc::RpcRes::RemoteIce(res.map(|added| RemoteIceResponse { added })))),
                },
                transport_webrtc::ExtOut::RestartIce(req_id, _, res) => Output::ExtRpc(
                    req_id,
                    RpcRes::Webrtc(webrtc::RpcRes::RestartIce(res.map(|(ice_lite, sdp)| {
                        (
                            session.index(),
                            ConnectResponse {
                                conn_id: "".to_string(),
                                sdp,
                                ice_lite,
                            },
                        )
                    }))),
                ),
            },
            transport_webrtc::GroupOutput::Shutdown(_session) => Output::Continue,
            transport_webrtc::GroupOutput::Continue => Output::Continue,
        }
    }
}

impl<ES: 'static + MediaEdgeSecure> MediaServerWorker<ES> {
    fn process_rpc(&mut self, now: Instant, req_id: u64, req: RpcReq<usize>) {
        log::info!("[MediaServerWorker] incoming rpc req {req_id}");
        match req {
            RpcReq::Whip(req) => match req {
                whip::RpcReq::Connect(req) => {
                    match self
                        .media_webrtc
                        .input(&mut self.switcher)
                        .spawn(req.ip, req.session_id, transport_webrtc::VariantParams::Whip(req.room, req.peer, req.record), &req.sdp)
                    {
                        Ok((_ice_lite, sdp, conn_id)) => self.queue.push_back(Output::ExtRpc(req_id, RpcRes::Whip(whip::RpcRes::Connect(Ok(WhipConnectRes { conn_id, sdp }))))),
                        Err(e) => self.queue.push_back(Output::ExtRpc(req_id, RpcRes::Whip(whip::RpcRes::Connect(Err(e))))),
                    }
                }
                whip::RpcReq::RemoteIce(req) => {
                    log::info!("on rpc request {req_id}, whip::RpcReq::RemoteIce");
                    self.media_webrtc.input(&mut self.switcher).on_event(
                        now,
                        GroupInput::Ext(req.conn_id.into(), transport_webrtc::ExtIn::RemoteIce(req_id, transport_webrtc::Variant::Whip, vec![req.ice])),
                    );
                }
                whip::RpcReq::Delete(req) => {
                    //TODO check error instead of auto response ok
                    self.queue.push_back(Output::ExtRpc(req_id, RpcRes::Whip(whip::RpcRes::Delete(Ok(WhipDeleteRes {})))));
                    self.media_webrtc.input(&mut self.switcher).on_event(now, GroupInput::Close(req.conn_id.into()));
                }
            },
            RpcReq::Whep(req) => match req {
                whep::RpcReq::Connect(req) => {
                    let peer_id = format!("whep-{}", random::<u64>());
                    match self
                        .media_webrtc
                        .input(&mut self.switcher)
                        .spawn(req.ip, req.session_id, transport_webrtc::VariantParams::Whep(req.room, peer_id.into()), &req.sdp)
                    {
                        Ok((_ice_lite, sdp, conn_id)) => self.queue.push_back(Output::ExtRpc(req_id, RpcRes::Whep(whep::RpcRes::Connect(Ok(WhepConnectRes { conn_id, sdp }))))),
                        Err(e) => self.queue.push_back(Output::ExtRpc(req_id, RpcRes::Whep(whep::RpcRes::Connect(Err(e))))),
                    }
                }
                whep::RpcReq::RemoteIce(req) => {
                    log::info!("on rpc request {req_id}, whep::RpcReq::RemoteIce");
                    self.media_webrtc.input(&mut self.switcher).on_event(
                        now,
                        GroupInput::Ext(req.conn_id.into(), transport_webrtc::ExtIn::RemoteIce(req_id, transport_webrtc::Variant::Whep, vec![req.ice])),
                    );
                }
                whep::RpcReq::Delete(req) => {
                    //TODO check error instead of auto response ok
                    self.queue.push_back(Output::ExtRpc(req_id, RpcRes::Whep(whep::RpcRes::Delete(Ok(WhepDeleteRes {})))));
                    self.media_webrtc.input(&mut self.switcher).on_event(now, GroupInput::Close(req.conn_id.into()));
                }
            },
            RpcReq::Webrtc(req) => match req {
                webrtc::RpcReq::Connect(session_id, ip, user_agent, req, record) => {
                    match self
                        .media_webrtc
                        .input(&mut self.switcher)
                        .spawn(ip, session_id, VariantParams::Webrtc(user_agent, req.clone(), record, self.secure.clone()), &req.sdp)
                    {
                        Ok((ice_lite, sdp, conn_id)) => self.queue.push_back(Output::ExtRpc(
                            req_id,
                            RpcRes::Webrtc(webrtc::RpcRes::Connect(Ok((
                                conn_id,
                                ConnectResponse {
                                    conn_id: "".to_string(),
                                    sdp,
                                    ice_lite,
                                },
                            )))),
                        )),
                        Err(e) => self.queue.push_back(Output::ExtRpc(req_id, RpcRes::Webrtc(webrtc::RpcRes::Connect(Err(e))))),
                    }
                }
                webrtc::RpcReq::RemoteIce(conn, ice) => {
                    log::info!("on rpc request {req_id}, webrtc::RpcReq::RemoteIce");
                    self.media_webrtc.input(&mut self.switcher).on_event(
                        now,
                        GroupInput::Ext(conn.into(), transport_webrtc::ExtIn::RemoteIce(req_id, transport_webrtc::Variant::Webrtc, ice.candidates)),
                    );
                }
                webrtc::RpcReq::RestartIce(conn, ip, user_agent, req, record) => {
                    log::info!("on rpc request {req_id}, webrtc::RpcReq::RestartIce");
                    self.media_webrtc.input(&mut self.switcher).on_event(
                        now,
                        GroupInput::Ext(conn.into(), transport_webrtc::ExtIn::RestartIce(req_id, transport_webrtc::Variant::Webrtc, ip, user_agent, req, record)),
                    );
                }
                webrtc::RpcReq::Delete(_) => todo!(),
            },
        }
    }
}
