use std::net::IpAddr;

use crate::{
    endpoint::{PeerId, RoomId},
    protobuf,
};

use super::{ConnLayer, RpcResult};

#[derive(Debug, Clone)]
pub struct WhipConnectReq {
    pub session_id: u64,
    pub sdp: String,
    pub room: RoomId,
    pub peer: PeerId,
    pub record: bool,
    pub ip: IpAddr,
    pub user_agent: String,
}

#[derive(Debug, Clone)]
pub struct WhipConnectRes<Conn> {
    pub conn_id: Conn,
    pub sdp: String,
}

#[derive(Debug, Clone)]
pub struct WhipRemoteIceReq<Conn> {
    pub conn_id: Conn,
    pub ice: String,
}

#[derive(Debug, Clone)]
pub struct WhipRemoteIceRes {}

#[derive(Debug, Clone)]
pub struct WhipDeleteReq<Conn> {
    pub conn_id: Conn,
}

#[derive(Debug, Clone)]
pub struct WhipDeleteRes {}

#[derive(Debug, Clone, convert_enum::From, convert_enum::TryInto)]
pub enum RpcReq<Conn> {
    Connect(WhipConnectReq),
    RemoteIce(WhipRemoteIceReq<Conn>),
    Delete(WhipDeleteReq<Conn>),
}

impl<Conn: ConnLayer> RpcReq<Conn> {
    pub fn down(self) -> (RpcReq<Conn::Down>, Option<Conn::DownRes>) {
        match self {
            RpcReq::Connect(req) => (RpcReq::Connect(req), None),
            RpcReq::RemoteIce(req) => {
                let (down, layer) = req.conn_id.down();
                (RpcReq::RemoteIce(WhipRemoteIceReq { conn_id: down, ice: req.ice }), Some(layer))
            }
            RpcReq::Delete(req) => {
                let (down, layer) = req.conn_id.down();
                (RpcReq::Delete(WhipDeleteReq { conn_id: down }), Some(layer))
            }
        }
    }

    pub fn get_down_part(&self) -> Option<Conn::DownRes> {
        match self {
            RpcReq::Connect(_req) => None,
            RpcReq::RemoteIce(req) => Some(req.conn_id.get_down_part()),
            RpcReq::Delete(req) => Some(req.conn_id.get_down_part()),
        }
    }
}

#[derive(Debug, Clone, convert_enum::From, convert_enum::TryInto)]
pub enum RpcRes<Conn> {
    Connect(RpcResult<WhipConnectRes<Conn>>),
    RemoteIce(RpcResult<WhipRemoteIceRes>),
    Delete(RpcResult<WhipDeleteRes>),
}

impl<Conn: ConnLayer> RpcRes<Conn> {
    pub fn up(self, param: Conn::UpParam) -> RpcRes<Conn::Up> {
        match self {
            RpcRes::Connect(Ok(res)) => RpcRes::Connect(Ok(WhipConnectRes {
                conn_id: res.conn_id.up(param),
                sdp: res.sdp,
            })),
            RpcRes::Connect(Err(e)) => RpcRes::Connect(Err(e)),
            RpcRes::RemoteIce(res) => RpcRes::RemoteIce(res),
            RpcRes::Delete(res) => RpcRes::Delete(res),
        }
    }
}

impl TryFrom<protobuf::cluster_gateway::WhipConnectRequest> for WhipConnectReq {
    type Error = ();
    fn try_from(value: protobuf::cluster_gateway::WhipConnectRequest) -> Result<Self, Self::Error> {
        Ok(Self {
            session_id: value.session_id,
            sdp: value.sdp,
            room: value.room.into(),
            peer: value.peer.into(),
            record: value.record,
            ip: value.ip.parse().map_err(|_| ())?,
            user_agent: value.user_agent,
        })
    }
}

impl From<WhipConnectReq> for protobuf::cluster_gateway::WhipConnectRequest {
    fn from(val: WhipConnectReq) -> Self {
        protobuf::cluster_gateway::WhipConnectRequest {
            session_id: val.session_id,
            user_agent: val.user_agent,
            ip: val.ip.to_string(),
            sdp: val.sdp,
            room: val.room.0,
            peer: val.peer.0,
            record: val.record,
        }
    }
}
