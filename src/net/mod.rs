/*
 copyright: (c) 2013-2019 by Blockstack PBC, a public benefit corporation.

 This file is part of Blockstack.

 Blockstack is free software. You may redistribute or modify
 it under the terms of the GNU General Public License as published by
 the Free Software Foundation, either version 3 of the License or
 (at your option) any later version.

 Blockstack is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY, including without the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 GNU General Public License for more details.

 You should have received a copy of the GNU General Public License
 along with Blockstack. If not, see <http://www.gnu.org/licenses/>.
*/

pub mod asn;
pub mod chat;
pub mod codec;
pub mod connection;
pub mod db;
pub mod dns;
pub mod download;
pub mod http;
pub mod inv;
pub mod neighbors;
pub mod p2p;
pub mod poll;
pub mod prune;
pub mod rpc;
pub mod server;

use std::fmt;
use std::hash::Hash;
use std::hash::Hasher;
use std::error;
use std::net::SocketAddr;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::collections::HashMap;
use std::io::prelude::*;
use std::io;
use std::io::{Read, Write};
use std::str::FromStr;
use std::cmp::PartialEq;
use std::convert::TryFrom;
use std::convert::From;
use std::ops::Deref;
use std::borrow::Borrow;

use url;

use rand::RngCore;
use rand::thread_rng;

use serde_json;
use serde::{Serialize, Deserialize};

use regex::Regex;

use burnchains::BurnchainHeaderHash;
use burnchains::Txid;

use chainstate::burn::ConsensusHash;
use chainstate::burn::CONSENSUS_HASH_ENCODED_SIZE;
use chainstate::burn::BlockHeaderHash;

use chainstate::stacks::Error as chain_error;
use chainstate::stacks::StacksBlock;
use chainstate::stacks::StacksMicroblock;
use chainstate::stacks::StacksTransaction;
use chainstate::stacks::StacksPublicKey;

use chainstate::stacks::Error as chainstate_error;

use util::hash::Hash160;
use util::hash::DOUBLE_SHA256_ENCODED_SIZE;
use util::hash::HASH160_ENCODED_SIZE;

use util::db::Error as db_error;
use util::db::DBConn;

use util::log;

use util::secp256k1::Secp256k1PublicKey;
use util::secp256k1::MessageSignature;
use util::secp256k1::MESSAGE_SIGNATURE_ENCODED_SIZE;
use util::strings::UrlString;

use serde::ser::Error as ser_Error;
use serde::de::Error as de_Error;

use vm::clarity::Error as clarity_error;
use chainstate::stacks::index::Error as marf_error;

use self::dns::*;

#[derive(Debug)]
pub enum Error {
    /// Failed to encode 
    SerializeError(String),
    /// Failed to read
    ReadError(io::Error),
    /// Failed to decode 
    DeserializeError(String),
    /// Filaed to write
    WriteError(io::Error),
    /// Underflow -- not enough bytes to form the message
    UnderflowError(String),
    /// Overflow -- message too big 
    OverflowError(String),
    /// Wrong protocol family 
    WrongProtocolFamily,
    /// Array is too big 
    ArrayTooLong,
    /// Receive timed out 
    RecvTimeout,
    /// Error signing a message
    SigningError(String),
    /// Error verifying a message 
    VerifyingError(String),
    /// Read stream is drained.  Try again
    TemporarilyDrained,
    /// Read stream has reached EOF (socket closed, end-of-file reached, etc.)
    PermanentlyDrained,
    /// Failed to read from the FS 
    FilesystemError,
    /// Database error 
    DBError(db_error),
    /// Socket mutex was poisoned
    SocketMutexPoisoned,
    /// Socket not instantiated
    SocketNotConnectedToPeer,
    /// Not connected to peer
    ConnectionBroken,
    /// Connection could not be (re-)established
    ConnectionError,
    /// Too many outgoing messages 
    OutboxOverflow,
    /// Too many incoming messages 
    InboxOverflow,
    /// Send error 
    SendError(String),
    /// Recv error 
    RecvError(String),
    /// Invalid message 
    InvalidMessage,
    /// Invalid network handle
    InvalidHandle,
    /// Invalid handshake 
    InvalidHandshake,
    /// Stale neighbor
    StaleNeighbor,
    /// No such neighbor 
    NoSuchNeighbor,
    /// Failed to bind
    BindError,
    /// Failed to poll 
    PollError,
    /// Failed to accept 
    AcceptError,
    /// Failed to register socket with poller 
    RegisterError,
    /// Failed to query socket metadata 
    SocketError,
    /// server is not bound to a socket
    NotConnected,
    /// Remote peer is not connected 
    PeerNotConnected,
    /// Too many peers
    TooManyPeers,
    /// Peer already connected 
    AlreadyConnected(usize),
    /// Message already in progress
    InProgress,
    /// Data URL is not known
    NoDataUrl,
    /// Error resolving a DNS name
    LookupError(String),
    /// MARF error, percolated up from chainstate
    MARFError(marf_error),
    /// Clarity VM error, percolated up from chainstate
    ClarityError(clarity_error),
    /// Catch-all for chainstate errors that don't map cleanly into network errors
    ChainstateError(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::SerializeError(ref s) => fmt::Display::fmt(s, f),
            Error::DeserializeError(ref s) => fmt::Display::fmt(s, f),
            Error::ReadError(ref io) => fmt::Display::fmt(io, f),
            Error::WriteError(ref io) => fmt::Display::fmt(io, f),
            Error::UnderflowError(ref s) => fmt::Display::fmt(s, f),
            Error::OverflowError(ref s) => fmt::Display::fmt(s, f),
            Error::WrongProtocolFamily => write!(f, "Improper use of protocol family"),
            Error::ArrayTooLong => write!(f, "Array too long"),
            Error::RecvTimeout => write!(f, "Packet receive timeout"),
            Error::SigningError(ref s) => fmt::Display::fmt(s, f),
            Error::VerifyingError(ref s) => fmt::Display::fmt(s, f),
            Error::TemporarilyDrained => write!(f, "Temporarily out of bytes to read; try again later"),
            Error::PermanentlyDrained => write!(f, "Out of bytes to read"),
            Error::FilesystemError => write!(f, "Disk I/O error"),
            Error::DBError(ref e) => fmt::Display::fmt(e, f),
            Error::SocketMutexPoisoned => write!(f, "socket mutex was poisoned"),
            Error::SocketNotConnectedToPeer => write!(f, "not connected to peer"),
            Error::ConnectionBroken => write!(f, "connection to peer node is broken"),
            Error::ConnectionError => write!(f, "connection to peer could not be (re-)established"),
            Error::OutboxOverflow => write!(f, "too many outgoing messages queued"),
            Error::InboxOverflow => write!(f, "too many messages pending"),
            Error::SendError(ref s) => fmt::Display::fmt(s, f),
            Error::RecvError(ref s) => fmt::Display::fmt(s, f),
            Error::InvalidMessage => write!(f, "invalid message (malformed or bad signature)"),
            Error::InvalidHandle => write!(f, "invalid network handle"),
            Error::InvalidHandshake => write!(f, "invalid handshake from remote peer"),
            Error::StaleNeighbor => write!(f, "neighbor is too far behind the chain tip"),
            Error::NoSuchNeighbor => write!(f, "no such neighbor"),
            Error::BindError => write!(f, "Failed to bind to the given address"),
            Error::PollError => write!(f, "Failed to poll"),
            Error::AcceptError => write!(f, "Failed to accept connection"),
            Error::RegisterError => write!(f, "Failed to register socket with poller"),
            Error::SocketError => write!(f, "Socket error"),
            Error::NotConnected => write!(f, "Not connected to peer network"),
            Error::PeerNotConnected => write!(f, "Remote peer is not connected to us"),
            Error::TooManyPeers => write!(f, "Too many peer connections open"),
            Error::AlreadyConnected(ref _id) => write!(f, "Peer already connected"),
            Error::InProgress => write!(f, "Message already in progress"),
            Error::NoDataUrl => write!(f, "No data URL available"),
            Error::LookupError(ref s) => fmt::Display::fmt(s, f),
            Error::ChainstateError(ref s) => fmt::Display::fmt(s, f),
            Error::ClarityError(ref e) => fmt::Display::fmt(e, f),
            Error::MARFError(ref e) => fmt::Display::fmt(e, f),
        }
    }
}

impl error::Error for Error {
    fn cause(&self) -> Option<&dyn error::Error> {
        match *self {
            Error::SerializeError(ref _s) => None,
            Error::ReadError(ref io) => Some(io),
            Error::DeserializeError(ref _s) => None,
            Error::WriteError(ref io) => Some(io),
            Error::UnderflowError(ref _s) => None,
            Error::OverflowError(ref _s) => None,
            Error::WrongProtocolFamily => None,
            Error::ArrayTooLong => None,
            Error::RecvTimeout => None,
            Error::SigningError(ref _s) => None,
            Error::VerifyingError(ref _s) => None,
            Error::TemporarilyDrained => None,
            Error::PermanentlyDrained => None,
            Error::FilesystemError => None,
            Error::DBError(ref e) => Some(e),
            Error::SocketMutexPoisoned => None,
            Error::SocketNotConnectedToPeer => None,
            Error::ConnectionBroken => None,
            Error::ConnectionError => None,
            Error::OutboxOverflow => None,
            Error::InboxOverflow => None,
            Error::SendError(ref _s) => None,
            Error::RecvError(ref _s) => None,
            Error::InvalidMessage => None,
            Error::InvalidHandle => None,
            Error::InvalidHandshake => None,
            Error::StaleNeighbor => None,
            Error::NoSuchNeighbor => None,
            Error::BindError => None,
            Error::PollError => None,
            Error::AcceptError => None,
            Error::RegisterError => None,
            Error::SocketError => None,
            Error::NotConnected => None,
            Error::PeerNotConnected => None,
            Error::TooManyPeers => None,
            Error::AlreadyConnected(ref _id) => None,
            Error::InProgress => None,
            Error::NoDataUrl => None,
            Error::LookupError(ref _s) => None,
            Error::ChainstateError(ref _s) => None,
            Error::ClarityError(ref e) => Some(e),
            Error::MARFError(ref e) => Some(e),
        }
    }
}

impl From<chain_error> for Error {
    fn from(e: chain_error) -> Error {
        match e {
            chain_error::InvalidStacksBlock(s) => Error::ChainstateError(format!("Invalid stacks block: {}", s)),
            chain_error::InvalidStacksMicroblock(msg, hash) => Error::ChainstateError(format!("Invalid stacks microblock {:?}: {}", hash, msg)),
            chain_error::InvalidStacksTransaction(s) => Error::ChainstateError(format!("Invalid stacks transaction: {}", s)),
            chain_error::PostConditionFailed(s) => Error::ChainstateError(format!("Postcondition failed: {}", s)),
            chain_error::ClarityError(e) => Error::ClarityError(e),
            chain_error::DBError(e) => Error::DBError(e),
            chain_error::NetError(e) => e,
            chain_error::MARFError(e) => Error::MARFError(e),
            chain_error::ReadError(e) => Error::ReadError(e),
            chain_error::WriteError(e) => Error::WriteError(e),
            _ => Error::ChainstateError(format!("Stacks chainstate error: {:?}", &e))
        }
    }
}

#[cfg(test)]
impl PartialEq for Error {
    /// (make I/O errors comparable for testing purposes)
    fn eq(&self, other: &Self) -> bool {
        let s1 = format!("{:?}", self);
        let s2 = format!("{:?}", other);
        s1 == s2
    }
}

/// Helper trait for various primitive types that make up Stacks messages
pub trait StacksMessageCodec {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), Error>
        where Self: Sized;
    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<Self, Error>
        where Self: Sized;
}

/// A container for an IPv4 or IPv6 address.
/// Rules:
/// -- If this is an IPv6 address, the octets are in network byte order
/// -- If this is an IPv4 address, the octets must encode an IPv6-to-IPv4-mapped address
pub struct PeerAddress([u8; 16]);
impl_array_newtype!(PeerAddress, u8, 16);
impl_array_hexstring_fmt!(PeerAddress);
impl_byte_array_newtype!(PeerAddress, u8, 16);
pub const PEER_ADDRESS_ENCODED_SIZE : u32 = 16;

impl Serialize for PeerAddress {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let inst = format!("{}", self.to_socketaddr(0).ip());
        s.serialize_str(inst.as_str())
    }
}

impl<'de> Deserialize<'de> for PeerAddress {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<PeerAddress, D::Error> {
        let inst = String::deserialize(d)?;
        let ip = inst.parse::<IpAddr>()
            .map_err(de_Error::custom)?;

        Ok(PeerAddress::from_ip(&ip))
    }
}

impl PeerAddress {
    /// Is this an IPv4 address?
    pub fn is_ipv4(&self) -> bool {
        self.ipv4_octets().is_some()
    }
    
    /// Get the octet representation of this peer address as an IPv4 address.
    /// The last 4 bytes of the list contain the IPv4 address.
    /// This method returns None if the bytes don't encode a valid IPv4-mapped address (i.e. ::ffff:0:0/96)
    pub fn ipv4_octets(&self) -> Option<[u8; 4]> {
        if self.0[0..12] != [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff] {
            return None;
        }
        let mut ret = [0u8; 4];
        ret.copy_from_slice(&self.0[12..16]);
        Some(ret)
    }

    /// Return the bit representation of this peer address as an IPv4 address, in network byte
    /// order.  Return None if this is not an IPv4 address.
    pub fn ipv4_bits(&self) -> Option<u32> {
        let octets_opt = self.ipv4_octets();
        if octets_opt.is_none() {
            return None;
        }

        let octets = octets_opt.unwrap();
        Some(
            ((octets[0] as u32) << 24) | 
            ((octets[1] as u32) << 16) | 
            ((octets[2] as u32) << 8) |
            ((octets[3] as u32))
        )
    }

    /// Convert to SocketAddr
    pub fn to_socketaddr(&self, port: u16) -> SocketAddr {
        if self.is_ipv4() {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(self.0[12], self.0[13], self.0[14], self.0[15])), port)
        }
        else {
            let addr_words : [u16; 8] = [
                ((self.0[0] as u16) << 8) | (self.0[1] as u16),
                ((self.0[2] as u16) << 8) | (self.0[3] as u16),
                ((self.0[4] as u16) << 8) | (self.0[5] as u16),
                ((self.0[6] as u16) << 8) | (self.0[7] as u16),
                ((self.0[8] as u16) << 8) | (self.0[9] as u16),
                ((self.0[10] as u16) << 8) | (self.0[11] as u16),
                ((self.0[12] as u16) << 8) | (self.0[13] as u16),
                ((self.0[14] as u16) << 8) | (self.0[15] as u16)
            ];

            SocketAddr::new(IpAddr::V6(Ipv6Addr::new(addr_words[0], addr_words[1], addr_words[2], addr_words[3], addr_words[4], addr_words[5], addr_words[6], addr_words[7])), port)
        }
    }

    /// Convert from socket address 
    pub fn from_socketaddr(addr: &SocketAddr) -> PeerAddress {
        PeerAddress::from_ip(&addr.ip())
    }

    /// Convert from IP address
    pub fn from_ip(addr: &IpAddr) -> PeerAddress {
        match addr {
            IpAddr::V4(ref addr) => {
                let octets = addr.octets();
                PeerAddress([0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, octets[0], octets[1], octets[2], octets[3]])
            },
            IpAddr::V6(ref addr) => {
                let words = addr.segments();
                PeerAddress([(words[0] >> 8) as u8, (words[0] & 0xff) as u8,
                             (words[1] >> 8) as u8, (words[1] & 0xff) as u8,
                             (words[2] >> 8) as u8, (words[2] & 0xff) as u8,
                             (words[3] >> 8) as u8, (words[3] & 0xff) as u8,
                             (words[4] >> 8) as u8, (words[4] & 0xff) as u8,
                             (words[5] >> 8) as u8, (words[5] & 0xff) as u8,
                             (words[6] >> 8) as u8, (words[6] & 0xff) as u8,
                             (words[7] >> 8) as u8, (words[7] & 0xff) as u8])
            }
        }
    }

    /// Convert from ipv4 octets
    pub fn from_ipv4(o1: u8, o2: u8, o3: u8, o4: u8) -> PeerAddress {
        PeerAddress([0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, o1, o2, o3, o4])
    }
}

/// A container for public keys (compressed secp256k1 public keys)
pub struct StacksPublicKeyBuffer(pub [u8; 33]);
impl_array_newtype!(StacksPublicKeyBuffer, u8, 33);
impl_array_hexstring_fmt!(StacksPublicKeyBuffer);
impl_byte_array_newtype!(StacksPublicKeyBuffer, u8, 33);

pub const STACKS_PUBLIC_KEY_ENCODED_SIZE : u32 = 33;

/// supported HTTP content types
#[derive(Debug, Clone, PartialEq)]
pub enum HttpContentType {
    Bytes,
    Text,
    JSON
}

impl fmt::Display for HttpContentType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl HttpContentType {
    pub fn as_str(&self) -> &'static str {
        match *self {
            HttpContentType::Bytes => "application/octet-stream",
            HttpContentType::Text => "text/plain",
            HttpContentType::JSON => "application/json"
        }
    }
}

impl FromStr for HttpContentType {
    type Err = Error;

    fn from_str(header: &str) -> Result<HttpContentType, Error> {
        let s = header.to_string().to_lowercase();
        if s == "application/octet-stream" {
            Ok(HttpContentType::Bytes)
        }
        else if s == "text/plain" {
            Ok(HttpContentType::Text)
        }
        else if s == "application/json" {
            Ok(HttpContentType::JSON)
        }
        else {
            Err(Error::DeserializeError("Unsupported HTTP content type".to_string()))
        }
    }
}

/// HTTP request preamble
#[derive(Debug, Clone, PartialEq)]
pub struct HttpRequestPreamble {
    pub version: HttpVersion,
    pub verb: String,
    pub path: String,
    pub host: PeerHost,
    pub content_type: Option<HttpContentType>,
    pub content_length: Option<u32>,
    pub keep_alive: bool,
    pub headers: HashMap<String, String>
}

/// HTTP response preamble
#[derive(Debug, Clone, PartialEq)]
pub struct HttpResponsePreamble {
    pub status_code: u16,
    pub reason: String,
    pub keep_alive: bool,
    pub content_length: Option<u32>,     // if not given, then content will be transfer-encoed: chunked
    pub content_type: HttpContentType,   // required header
    pub request_id: u32,                 // X-Request-ID
    pub headers: HashMap<String, String>
}

/// Maximum size an HTTP request or response preamble can be (within reason)
pub const HTTP_PREAMBLE_MAX_ENCODED_SIZE : u32 = 4096;

/// P2P message preamble -- included in all p2p network messages
#[derive(Debug, Clone, PartialEq)]
pub struct Preamble {
    pub peer_version: u32,                          // software version
    pub network_id: u32,                            // mainnet, testnet, etc.
    pub seq: u32,                                   // message sequence number -- pairs this message to a request
    pub burn_block_height: u64,                     // last-seen block height (at chain tip)
    pub burn_consensus_hash: ConsensusHash,         // consensus hash at block_height
    pub burn_stable_block_height: u64,              // latest stable block height (e.g. chain tip minus 7)
    pub burn_stable_consensus_hash: ConsensusHash,  // consensus hash for burn_stable_block_height
    pub additional_data: u32,                       // RESERVED; pointer to additional data (should be all 0's if not used)
    pub signature: MessageSignature,                // signature from the peer that sent this
    pub payload_len: u32                            // length of the following payload, including relayers vector
}

/// P2P preamble length (addands correspond to fields above)
pub const PREAMBLE_ENCODED_SIZE: u32 = 
    4 +
    4 +
    4 +
    8 +
    CONSENSUS_HASH_ENCODED_SIZE +
    8 +
    CONSENSUS_HASH_ENCODED_SIZE +
    4 +
    MESSAGE_SIGNATURE_ENCODED_SIZE +
    4;

/// Request for a block inventory or a list of blocks.
#[derive(Debug, Clone, PartialEq)]
pub struct GetBlocksInv {
    pub consensus_hash: ConsensusHash,               // _last_ consensus hash.  Look backwards in time from this consensus hash
    pub num_blocks: u16                              // number of _prior_ blocks to ask for, starting at this burn header hash (not to exceed BLOCKS_INV_DATA_MAX_BITLEN)
}

/// A bit vector that describes which block and microblock data node has data for in a given burn
/// chain block range.  Sent in reply to a GetBlocksInv.
#[derive(Debug, Clone, PartialEq)]
pub struct BlocksInvData {
    pub bitlen: u16,                            // number of bits represented in bitvec (not to exceed BLOCKS_INV_DATA_MAX_BITLEN).  Bits correspond to sortitions on the canonical burn chain fork.
    pub block_bitvec: Vec<u8>,                  // bitmap of which blocks the peer has, in sortition order.  block_bitvec[i] & (1 << j) != 0 means that this peer has the block for sortition 8*i + j
    pub microblocks_bitvec: Vec<u8>,            // bitmap of which confirmed micrblocks the peer has, in sortition order.  microblocks_bitvec[i] & (1 << j) != 0 means that this peer has the microblocks produced by sortition 8*i + j
}

/// List of blocks pushed
#[derive(Debug, Clone, PartialEq)]
pub struct BlocksData {
    pub blocks: Vec<StacksBlock>
}

/// Microblocks pushed
#[derive(Debug, Clone, PartialEq)]
pub struct MicroblocksData {
    pub microblocks: Vec<StacksMicroblock>
}

/// A descriptor of a peer
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NeighborAddress {
    #[serde(rename = "ip")]
    pub addrbytes: PeerAddress,
    pub port: u16,
    pub public_key_hash: Hash160        // used as a hint; useful for when a node trusts another node to be honest about this
}

impl fmt::Display for NeighborAddress {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}://{:?}", &self.public_key_hash, &self.addrbytes.to_socketaddr(self.port))
    }
}

impl fmt::Debug for NeighborAddress {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}://{:?}", &self.public_key_hash, &self.addrbytes.to_socketaddr(self.port))
    }
}

pub const NEIGHBOR_ADDRESS_ENCODED_SIZE : u32 =
    PEER_ADDRESS_ENCODED_SIZE +
    2 +
    HASH160_ENCODED_SIZE;

/// A descriptor of a list of known peers
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NeighborsData {
    pub neighbors: Vec<NeighborAddress>
}

/// Handshake request -- this is the first message sent to a peer.
/// The remote peer will reply a HandshakeAccept with just a preamble
/// if the peer accepts.  Otherwise it will get a HandshakeReject with just
/// a preamble.
///
/// To keep peer knowledge fresh, nodes will send handshakes to each other
/// as heartbeat messages.
#[derive(Debug, Clone, PartialEq)]
pub struct HandshakeData {
    pub addrbytes: PeerAddress,
    pub port: u16,
    pub services: u16,                          // bit field representing services this node offers
    pub node_public_key: StacksPublicKeyBuffer,
    pub expire_block_height: u64,               // burn block height after which this node's key will be revoked,
    pub data_url: UrlString
}

#[repr(u8)]
pub enum ServiceFlags {
    RELAY = 0x01,
    RPC = 0x02,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HandshakeAcceptData {
    pub handshake: HandshakeData,       // this peer's handshake information
    pub heartbeat_interval: u32,        // hint as to how long this peer will remember you
}

#[derive(Debug, Clone, PartialEq)]
pub struct NackData {
    pub error_code: u32,
}
pub mod NackErrorCodes {
    pub const HandshakeRequired : u32 = 1;
    pub const NoSuchBurnchainBlock : u32 = 2;
}

#[derive(Debug, Clone, PartialEq)]
pub struct PingData {
    pub nonce: u32
}

#[derive(Debug, Clone, PartialEq)]
pub struct PongData {
    pub nonce: u32
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelayData {
    pub peer: NeighborAddress,
    pub seq: u32,
    pub signature: MessageSignature
}

pub const RELAY_DATA_ENCODED_SIZE : u32 =
    NEIGHBOR_ADDRESS_ENCODED_SIZE +
    4 +
    MESSAGE_SIGNATURE_ENCODED_SIZE;

/// All P2P message types
#[derive(Debug, Clone, PartialEq)]
pub enum StacksMessageType {
    Handshake(HandshakeData),
    HandshakeAccept(HandshakeAcceptData),
    HandshakeReject,
    GetNeighbors,
    Neighbors(NeighborsData),
    GetBlocksInv(GetBlocksInv),
    BlocksInv(BlocksInvData),
    Blocks(BlocksData),
    Microblocks(MicroblocksData),
    Transaction(StacksTransaction),
    Nack(NackData),
    Ping(PingData),
    Pong(PongData),
    // TODO: get unconfirmed transactions
    // TODO: get unconfirmed microblocks
}

/// Peer address variants
#[derive(Clone, PartialEq)]
pub enum PeerHost {
    DNS(String, u16),
    IP(PeerAddress, u16)
}

impl fmt::Display for PeerHost {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            PeerHost::DNS(ref s, ref p) => write!(f, "{}:{}", s, p),
            PeerHost::IP(ref a, ref p) => write!(f, "{}", a.to_socketaddr(*p))
        }
    }
}

impl fmt::Debug for PeerHost {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            PeerHost::DNS(ref s, ref p) => write!(f, "PeerHost::DNS({},{})", s, p),
            PeerHost::IP(ref a, ref p) => write!(f, "PeerHost::IP({:?},{})", a, p)
        }
    }
}

impl Hash for PeerHost {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match *self {
            PeerHost::DNS(ref name, ref port) => {
                "DNS".hash(state);
                name.hash(state);
                port.hash(state);
            },
            PeerHost::IP(ref addrbytes, ref port) => {
                "IP".hash(state);
                addrbytes.hash(state);
                port.hash(state);
            }
        }
    }
}

impl PeerHost {
    pub fn hostname(&self) -> String {
        match *self {
            PeerHost::DNS(ref s, _) => s.clone(),
            PeerHost::IP(ref a, ref p) => format!("{}", a.to_socketaddr(*p).ip())
        }
    }
    
    pub fn port(&self) -> u16 {
        match *self {
            PeerHost::DNS(_, ref p) => *p,
            PeerHost::IP(_, ref p) => *p
        }
    }

    pub fn from_host_port(host: String, port: u16) -> PeerHost {
        // try as IP, and fall back to DNS 
        match host.parse::<IpAddr>() {
            Ok(addr) => PeerHost::IP(PeerAddress::from_ip(&addr), port),
            Err(_) => PeerHost::DNS(host, port)
        }
    }

    pub fn from_socketaddr(socketaddr: &SocketAddr) -> PeerHost {
        PeerHost::IP(PeerAddress::from_socketaddr(socketaddr), socketaddr.port())
    }

    pub fn try_from_url(url_str: &UrlString) -> Option<PeerHost> {
        let url = match url_str.parse_to_block_url() {
            Ok(url) => url,
            Err(_e) => {
                return None;
            }
        };

        let port = match url.port_or_known_default() {
            Some(port) => port,
            None => {
                return None;
            }
        };

        match url.host() {
            Some(url::Host::Domain(name)) => Some(PeerHost::DNS(name.to_string(), port)),
            Some(url::Host::Ipv4(addr)) => Some(PeerHost::from_socketaddr(&SocketAddr::new(IpAddr::V4(addr), port))),
            Some(url::Host::Ipv6(addr)) => Some(PeerHost::from_socketaddr(&SocketAddr::new(IpAddr::V6(addr), port))),
            None => None
        }
    }
}

/// The data we return on GET /v2/info
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerInfoData {
    peer_version: u32,
    burn_consensus: ConsensusHash,
    burn_block_height: u64,
    stable_burn_consensus: ConsensusHash,
    stable_burn_block_height: u64,
    server_version: String,
    network_id: u32,
    parent_network_id: u32,
}

#[derive(Debug, Clone, PartialEq, Copy, Hash)]
#[repr(u8)]
pub enum HttpVersion {
    Http10 = 0x10,
    Http11 = 0x11
}

#[derive(Debug, Clone, PartialEq, Hash)]
pub struct HttpRequestMetadata {
    pub version: HttpVersion,
    pub peer: PeerHost,
    pub keep_alive: bool
}

/// Request ID to use or expect from non-Stacks HTTP clients.
/// In particular, if a HTTP response does not contain the x-request-id header, then it's assumed
/// to be this value.  This is needed to support fetching immutables like block and microblock data
/// from non-Stacks nodes (like Gaia hubs, CDNs, vanilla HTTP servers, and so on).
pub const HTTP_REQUEST_ID_RESERVED : u32 = 0;

impl HttpRequestMetadata {
    pub fn new(host: String, port: u16) -> HttpRequestMetadata {
        HttpRequestMetadata {
            version: HttpVersion::Http11,
            peer: PeerHost::from_host_port(host, port),
            keep_alive: true,
        }
    }

    pub fn from_host(peer_host: PeerHost) -> HttpRequestMetadata {
        HttpRequestMetadata {
            version: HttpVersion::Http11,
            peer: peer_host,
            keep_alive: true,
        }
    }

    pub fn from_preamble(preamble: &HttpRequestPreamble) -> HttpRequestMetadata {
        HttpRequestMetadata {
            version: preamble.version,
            peer: preamble.host.clone(),
            keep_alive: preamble.keep_alive,
        }
    }
}

/// All HTTP request paths we support, and the arguments they carry in their paths
#[derive(Debug, Clone, PartialEq)]
pub enum HttpRequestType {
    GetInfo(HttpRequestMetadata),
    GetNeighbors(HttpRequestMetadata),
    GetBlock(HttpRequestMetadata, BlockHeaderHash),
    GetMicroblocksIndexed(HttpRequestMetadata, BlockHeaderHash),
    GetMicroblocksConfirmed(HttpRequestMetadata, BlockHeaderHash),
    GetMicroblocksUnconfirmed(HttpRequestMetadata, BlockHeaderHash, u16),
    PostTransaction(HttpRequestMetadata, StacksTransaction)
}

/// The fields that Actually Matter to http responses
#[derive(Debug, Clone, PartialEq)]
pub struct HttpResponseMetadata {
    pub client_version: HttpVersion,
    pub client_keep_alive: bool,
    pub request_id: u32,
    pub content_length: Option<u32>,
}

impl HttpResponseMetadata {
    pub fn make_request_id() -> u32 {
        let mut rng = thread_rng();
        let mut request_id = HTTP_REQUEST_ID_RESERVED;
        while request_id == HTTP_REQUEST_ID_RESERVED {
            request_id = rng.next_u32();
        }
        request_id
    }

    pub fn new(client_version: HttpVersion, request_id: u32, content_length: Option<u32>, client_keep_alive: bool) -> HttpResponseMetadata {
        HttpResponseMetadata {
            client_version: client_version,
            client_keep_alive: client_keep_alive,
            request_id: request_id,
            content_length: content_length,
        }
    }

    pub fn from_preamble(request_version: HttpVersion, preamble: &HttpResponsePreamble) -> HttpResponseMetadata {
        HttpResponseMetadata {
            client_version: request_version,
            client_keep_alive: preamble.keep_alive,
            request_id: preamble.request_id,
            content_length: preamble.content_length.clone(),
        }
    }
}

/// All data-plane message types a peer can reply with.
#[derive(Debug, Clone, PartialEq)]
pub enum HttpResponseType {
    PeerInfo(HttpResponseMetadata, PeerInfoData),
    Neighbors(HttpResponseMetadata, NeighborsData),
    Block(HttpResponseMetadata, StacksBlock),
    BlockStream(HttpResponseMetadata),
    Microblocks(HttpResponseMetadata, Vec<StacksMicroblock>),
    MicroblockStream(HttpResponseMetadata),
    TransactionID(HttpResponseMetadata, Txid),
    
    // peer-given error responses
    BadRequest(HttpResponseMetadata, String),
    Unauthorized(HttpResponseMetadata, String),
    PaymentRequired(HttpResponseMetadata, String),
    Forbidden(HttpResponseMetadata, String),
    NotFound(HttpResponseMetadata, String),
    ServerError(HttpResponseMetadata, String),
    ServiceUnavailable(HttpResponseMetadata, String),
    Error(HttpResponseMetadata, u16, String)
}

#[derive(Debug, Clone, PartialEq, Copy)]
pub enum UrlScheme {
    Http,
    Https
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum StacksMessageID {
    Handshake = 0,
    HandshakeAccept = 1,
    HandshakeReject = 2,
    GetNeighbors = 3,
    Neighbors = 4,
    GetBlocksInv = 5,
    BlocksInv = 6,
    Blocks = 7,
    Microblocks = 8,
    Transaction = 9,
    Nack = 10,
    Ping = 11,
    Pong = 12,
    Reserved = 255
}

/// Message type for all P2P Stacks network messages
#[derive(Debug, Clone, PartialEq)]
pub struct StacksMessage {
    pub preamble: Preamble,
    pub relayers: Vec<RelayData>,
    pub payload: StacksMessageType,
}

/// Message type for HTTP
#[derive(Debug, Clone, PartialEq)]
pub enum StacksHttpMessage {
    Request(HttpRequestType),
    Response(HttpResponseType),
}

/// HTTP message preamble
#[derive(Debug, Clone, PartialEq)]
pub enum StacksHttpPreamble {
    Request(HttpRequestPreamble),
    Response(HttpResponsePreamble)
}

/// Network messages implement this to have multiple messages in flight.
pub trait MessageSequence {
    fn request_id(&self) -> u32;
    fn get_message_name(&self) -> &'static str;
}

pub trait ProtocolFamily {
    type Preamble: StacksMessageCodec + Send + Sync + Clone + PartialEq + std::fmt::Debug;
    type Message : MessageSequence + Send + Sync + Clone + PartialEq + std::fmt::Debug;

    /// Return the maximum possible length of the serialized Preamble type
    fn preamble_size_hint(&mut self) -> usize;
    
    /// Determine how long the message payload will be, given the Preamble (may return None if the
    /// payload length cannot be determined solely by the Preamble).
    fn payload_len(&mut self, preamble: &Self::Preamble) -> Option<usize>;

    /// Given a byte buffer of a length at last that of the value returned by preamble_size_hint,
    /// parse a Preamble and return both the Preamble and the number of bytes actually consumed by it.
    fn read_preamble(&mut self, buf: &[u8]) -> Result<(Self::Preamble, usize), Error>;

    /// Given a preamble and a byte buffer, parse out a message and return both the message and the
    /// number of bytes actually consumed by it.  Only used if the message is _not_ streamed.  The
    /// buf slice is guaranteed to have at least `payload_len()` bytes if `payload_len()` returns
    /// Some(...).
    fn read_payload(&mut self, preamble: &Self::Preamble, buf: &[u8]) -> Result<(Self::Message, usize), Error>;
    
    /// Given a preamble and a Read, attempt to stream a message.  This will be called if
    /// `payload_len()` returns None.  This method will be repeatedly called with new data until a
    /// message can be obtained; therefore, the ProtocolFamily implementation will need to do its
    /// own bufferring and state-tracking.
    fn stream_payload<R: Read>(&mut self, preamble: &Self::Preamble, fd: &mut R) -> Result<(Option<(Self::Message, usize)>, usize), Error>;

    /// Given a public key, a preamble, and the yet-to-be-parsed message bytes, verify the message
    /// authenticity.  Not all protocols need to do this.
    fn verify_payload_bytes(&mut self, key: &StacksPublicKey, preamble: &Self::Preamble, bytes: &[u8]) -> Result<(), Error>;

    /// Given a Write and a Message, write it out.  This method is also responsible for generating
    /// and writing out a Preamble for its Message.
    fn write_message<W: Write>(&mut self, fd: &mut W, message: &Self::Message) -> Result<(), Error>;
}

// these implement the ProtocolFamily trait 
#[derive(Debug, Clone, PartialEq)]
pub struct StacksP2P {}

pub use self::http::StacksHttp;

// an array in our protocol can't exceed this many items
pub const ARRAY_MAX_LEN : u32 = u32::max_value();

// maximum number of neighbors in a NeighborsData
pub const MAX_NEIGHBORS_DATA_LEN : u32 = 128;

// maximum number of relayers -- will be an upper bound on the peer graph diameter
pub const MAX_RELAYERS_LEN : u32 = 16;

// messages can't be bigger than 16MB plus the preamble and relayers
pub const MAX_MESSAGE_LEN : u32 = (1 + 16 * 1024 * 1024) + (PREAMBLE_ENCODED_SIZE + MAX_RELAYERS_LEN * RELAY_DATA_ENCODED_SIZE);

// maximum value of a blocks's inv data bitlen.
// NOTE: This needs to be a multiple of 8
#[cfg(test)] pub const BLOCKS_INV_DATA_MAX_BITLEN : u32 = 32;
#[cfg(not(test))] pub const BLOCKS_INV_DATA_MAX_BITLEN : u32 = 65536;

macro_rules! impl_byte_array_message_codec {
    ($thing:ident, $len:expr) => {
        impl StacksMessageCodec for $thing {
            fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), ::net::Error> {
                fd.write_all(self.as_bytes()).map_err(::net::Error::WriteError)
            }
            fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<$thing, ::net::Error> {
                let mut buf = [0u8; ($len as usize)];
                fd.read_exact(&mut buf).map_err(::net::Error::ReadError)?;
                let ret = $thing::from_bytes(&buf).expect("BUG: buffer is not the right size");
                Ok(ret)
            }
        }
    }
}
impl_byte_array_message_codec!(ConsensusHash, 20);
impl_byte_array_message_codec!(Hash160, 20);
impl_byte_array_message_codec!(BurnchainHeaderHash, 32);
impl_byte_array_message_codec!(BlockHeaderHash, 32);
impl_byte_array_message_codec!(MessageSignature, 65);
impl_byte_array_message_codec!(PeerAddress, 16);
impl_byte_array_message_codec!(StacksPublicKeyBuffer, 33);

impl_byte_array_serde!(ConsensusHash);

/// neighbor identifier 
#[derive(Clone, Eq)]
pub struct NeighborKey {
    pub peer_version: u32,
    pub network_id: u32,
    pub addrbytes: PeerAddress,
    pub port: u16,
}

impl Hash for NeighborKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // ignores peer version and network ID -- we don't accept or deal with messages that have
        // incompatible versions or network IDs in the first place
        self.addrbytes.hash(state);
        self.port.hash(state);
    }
}

impl PartialEq for NeighborKey {
    fn eq(&self, other: &NeighborKey) -> bool {
        // peer version doesn't count 
        self.network_id == other.network_id && self.addrbytes == other.addrbytes && self.port == other.port
    }
}

impl fmt::Display for NeighborKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let peer_version_str = if self.peer_version > 0 { format!("{:08x}", self.peer_version).to_string() } else { "UNKNOWN".to_string() };
        let network_id_str = if self.network_id > 0 { format!("{:08x}", self.network_id).to_string() } else { "UNKNOWN".to_string() };
        write!(f, "{}+{}://{:?}", peer_version_str, network_id_str, &self.addrbytes.to_socketaddr(self.port))
    }
}

impl fmt::Debug for NeighborKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl NeighborKey {
    pub fn from_neighbor_address(peer_version: u32, network_id: u32, na: &NeighborAddress) -> NeighborKey {
        NeighborKey {
            peer_version: peer_version,
            network_id: network_id,
            addrbytes: na.addrbytes.clone(),
            port: na.port
        }
    }
}

/// Entry in the neighbor set
#[derive(Debug, Clone, PartialEq)]
pub struct Neighbor {
    pub addr: NeighborKey,
    
    // fields below this can change at runtime
    pub public_key: Secp256k1PublicKey,
    pub expire_block: u64,
    pub last_contact_time: u64,
    
    pub whitelisted: i64,       // whitelist deadline (negative == "forever")
    pub blacklisted: i64,       // blacklist deadline (negative == "forever")

    pub asn: u32,               // AS number
    pub org: u32,               // organization identifier

    pub in_degree: u32,         // number of peers who list this peer as a neighbor
    pub out_degree: u32,        // number of neighbors this peer has
}

pub const NUM_NEIGHBORS : usize = 32;

// maximum number of unconfirmed microblocks can get streamed to us
pub const MAX_MICROBLOCKS_UNCONFIRMED : usize = 1024;

/// Result of doing network work
pub struct NetworkResult {
    pub unhandled_messages: HashMap<usize, Vec<StacksMessage>>,
    pub blocks: Vec<StacksBlock>,
    pub confirmed_microblocks: Vec<Vec<StacksMicroblock>>,
    pub unconfirmed_microblocks: Vec<StacksMicroblock>,
    pub transactions: Vec<StacksTransaction>
}

impl NetworkResult {
    pub fn new() -> NetworkResult {
        NetworkResult {
            unhandled_messages: HashMap::new(),
            blocks: vec![],
            confirmed_microblocks: vec![],
            unconfirmed_microblocks: vec![],
            transactions: vec![]
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use net::asn::*;
    use net::chat::*;
    use net::connection::*;
    use net::codec::*;
    use net::db::*;
    use net::neighbors::*;
    use net::p2p::*;
    use net::poll::*;
    use net::Error as net_error;

    use chainstate::burn::*;
    use chainstate::burn::operations::*;
    use chainstate::burn::db::burndb;
    use chainstate::burn::db::burndb::*;
    use chainstate::*;
    use chainstate::stacks::miner::*;
    use chainstate::stacks::miner::test::*;

    use chainstate::stacks::db::StacksChainState;

    use chainstate::stacks::index::TrieHash;

    use burnchains::*;
    use burnchains::burnchain::*;
    use burnchains::test::*;

    use burnchains::bitcoin::*;
    use burnchains::bitcoin::address::*;
    use burnchains::bitcoin::keys::*;
    
    use util::secp256k1::*;
    use util::hash::*;
    use util::uint::*;
    use util::get_epoch_time_secs;

    use address::*;

    use std::net::*;
    use std::io;
    use std::io::Read;
    use std::io::Write;
    use std::io::ErrorKind;
    use std::io::Cursor;
    use std::ops::Deref;
    use std::ops::DerefMut;
    use std::collections::HashMap;
    use std::thread;

    use std::fs;
    
    use rand::RngCore;
    use rand;

    use mio;

    // emulate a socket
    pub struct NetCursor<T> {
        c: Cursor<T>,
        closed: bool,
        block: bool,
        read_error: Option<io::ErrorKind>,
        write_error: Option<io::ErrorKind>
    }

    impl<T> NetCursor<T> {
        pub fn new(inner: T) -> NetCursor<T> {
            NetCursor {
                c: Cursor::new(inner),
                closed: false,
                block: false,
                read_error: None,
                write_error: None,
            }
        }

        pub fn close(&mut self) -> () {
            self.closed = true;
        }

        pub fn block(&mut self) -> () {
            self.block = true;
        }

        pub fn unblock(&mut self) -> () {
            self.block = false;
        }

        pub fn set_read_error(&mut self, e: Option<io::ErrorKind>) -> () {
            self.read_error = e;
        }

        pub fn set_write_error(&mut self, e: Option<io::ErrorKind>) -> () {
            self.write_error = e;
        }
    }

    impl<T> Deref for NetCursor<T> {
        type Target = Cursor<T>;
        fn deref(&self) -> &Cursor<T> {
            &self.c
        }
    }
    
    impl<T> DerefMut for NetCursor<T> {
        fn deref_mut(&mut self) -> &mut Cursor<T> {
            &mut self.c
        }
    }

    impl<T> Read for NetCursor<T>
    where
        T: AsRef<[u8]>
    {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.block {
                return Err(io::Error::from(ErrorKind::WouldBlock));
            }
            if self.closed {
                return Ok(0);
            }
            match self.read_error {
                Some(ref e) => {
                    return Err(io::Error::from((*e).clone()));
                }
                None => {}
            }
            
            let sz = self.c.read(buf)?;
            if sz == 0 {
                // when reading from a non-blocking socket, a return value of 0 indicates the
                // remote end was closed.  For this reason, when we're out of bytes to read on our
                // inner cursor, but still have bytes, we need to re-interpret this as EWOULDBLOCK.
                return Err(io::Error::from(ErrorKind::WouldBlock));
            }
            else {
                return Ok(sz);
            }
        }
    }

    impl Write for NetCursor<&mut [u8]> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.block {
                return Err(io::Error::from(ErrorKind::WouldBlock));
            }
            if self.closed {
                return Err(io::Error::from(ErrorKind::Other));      // EBADF
            }
            match self.write_error {
                Some(ref e) => {
                    return Err(io::Error::from((*e).clone()));
                }
                None => {}
            }
            self.c.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.c.flush()
        }
    }
    
    impl Write for NetCursor<&mut Vec<u8>> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.c.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.c.flush()
        }
    }
    
    impl Write for NetCursor<Vec<u8>> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.c.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.c.flush()
        }
    }

    /// make a TCP server and a pair of TCP client sockets
    pub fn make_tcp_sockets() -> (mio::tcp::TcpListener, mio::tcp::TcpStream, mio::tcp::TcpStream) {
        let mut rng = rand::thread_rng();
        let (std_listener, port) = {
            let std_listener;
            let mut next_port;
            loop {
                next_port = 1024 + (rng.next_u32() % (65535 - 1024));
                let hostport = format!("127.0.0.1:{}", next_port);
                std_listener = match std::net::TcpListener::bind(&hostport.parse::<std::net::SocketAddr>().unwrap()) {
                    Ok(sock) => sock,
                    Err(e) => match e.kind() {
                        io::ErrorKind::AddrInUse => {
                            continue;
                        }
                        _ => {
                            assert!(false, "TcpListener::bind({}): {:?}", &hostport, &e);
                            unreachable!();
                        }
                    }
                };
                break;
            }
            (std_listener, next_port)
        };

        let std_sock_1 = std::net::TcpStream::connect(&format!("127.0.0.1:{}", port).parse::<std::net::SocketAddr>().unwrap()).unwrap();
        let sock_1 = mio::tcp::TcpStream::from_stream(std_sock_1).unwrap();
        let (std_sock_2, _) = std_listener.accept().unwrap();
        let sock_2 = mio::tcp::TcpStream::from_stream(std_sock_2).unwrap();

        sock_1.set_nodelay(true).unwrap();
        sock_2.set_nodelay(true).unwrap();

        let listener = mio::tcp::TcpListener::from_std(std_listener).unwrap();
        
        (listener, sock_1, sock_2)
    }


    // describes a peer's initial configuration
    #[derive(Debug, Clone)]
    pub struct TestPeerConfig {
        pub network_id: u32,
        pub peer_version: u32,
        pub current_block: u64,
        pub private_key: Secp256k1PrivateKey,
        pub private_key_expire: u64,
        pub initial_neighbors: Vec<Neighbor>,
        pub asn4_entries: Vec<ASEntry4>,
        pub burnchain: Burnchain,
        pub connection_opts: ConnectionOptions,
        pub server_port: u16,
        pub http_port: u16,
        pub asn: u32,
        pub org: u32,
        pub whitelisted: i64,
        pub blacklisted: i64,
        pub data_url: UrlString,
        pub test_name: String,
    }

    impl TestPeerConfig {
        pub fn default() -> TestPeerConfig {
            let conn_opts = ConnectionOptions::default();
            let start_block = 1;
            let burnchain = Burnchain::default_unittest(start_block, &BurnchainHeaderHash::from_hex("0000000000000000000000000000000000000000000000000000000000000000").unwrap());
            TestPeerConfig {
                network_id: 0x80000000,
                peer_version: 0x01020304,
                current_block: start_block + (burnchain.consensus_hash_lifetime + 1) as u64,
                private_key: Secp256k1PrivateKey::new(),
                private_key_expire: start_block + conn_opts.private_key_lifetime,
                initial_neighbors: vec![],
                asn4_entries: vec![],
                burnchain: burnchain,
                connection_opts: conn_opts,
                server_port: 32000,
                http_port: 32001,
                asn: 0,
                org: 0,
                whitelisted: 0,
                blacklisted: 0,
                data_url: "".into(),
                test_name: "".into()
            }
        }

        pub fn from_port(p: u16) -> TestPeerConfig {
            let mut config = TestPeerConfig {
                server_port: p,
                http_port: p + 1,
                ..TestPeerConfig::default()
            };
            config.data_url = UrlString::try_from(format!("http://localhost:{}", config.http_port).as_str()).unwrap();
            config
        }
        
        pub fn new(test_name: &str, p2p_port: u16, rpc_port: u16) -> TestPeerConfig {
            let mut config = TestPeerConfig {
                test_name: test_name.into(),
                server_port: p2p_port,
                http_port: rpc_port,
                ..TestPeerConfig::default()
            };
            config.data_url = UrlString::try_from(format!("http://localhost:{}", config.http_port).as_str()).unwrap();
            config
        }

        pub fn add_neighbor(&mut self, n: &Neighbor) -> () {
            self.initial_neighbors.push(n.clone());
        }

        pub fn to_neighbor(&self) -> Neighbor {
            Neighbor {
                addr: NeighborKey {
                    peer_version: self.peer_version,
                    network_id: self.network_id,
                    addrbytes: PeerAddress([0,0,0,0,0,0,0,0,0,0,0xff,0xff,127,0,0,1]),
                    port: self.server_port
                },
                public_key: Secp256k1PublicKey::from_private(&self.private_key),
                expire_block: self.private_key_expire,

                // not known yet
                last_contact_time: 0,
                whitelisted: self.whitelisted,
                blacklisted: self.blacklisted,
                asn: self.asn,
                org: self.org,
                in_degree: 0,
                out_degree: 0
            }
        }

        pub fn to_peer_host(&self) -> PeerHost {
            PeerHost::IP(PeerAddress([0,0,0,0,0,0,0,0,0,0,0xff,0xff,127,0,0,1]), self.http_port)
        }
    }

    pub fn make_test_chain_dbs(testname: &str, burnchain: &Burnchain, network_id: u32, key_expires: u64, data_url: UrlString, asn4_entries: &Vec<ASEntry4>, initial_neighbors: &Vec<Neighbor>) -> (PeerDB, BurnDB, StacksChainState) {
        let test_path = format!("/tmp/blockstack-test-databases-{}", testname);
        match fs::metadata(&test_path) {
            Ok(_) => {
                fs::remove_dir_all(&test_path).unwrap();
            },
            Err(_) => {}
        };

        fs::create_dir_all(&test_path).unwrap();

        let burndb_path = format!("{}/burn", &test_path);
        let peerdb_path = format!("{}/peers.db", &test_path);
        let chainstate_path = format!("{}/chainstate", &test_path);

        let peerdb = PeerDB::connect(&peerdb_path, true, network_id, burnchain.network_id, key_expires, data_url.clone(), &asn4_entries, Some(&initial_neighbors)).unwrap();
        let burndb = BurnDB::connect(&burndb_path, burnchain.first_block_height, &burnchain.first_block_hash, get_epoch_time_secs(), true).unwrap();
        let chainstate = StacksChainState::open(false, network_id, &chainstate_path).unwrap();

        (peerdb, burndb, chainstate)
    }

    pub fn dns_thread_start(max_inflight: u64) -> (DNSClient, thread::JoinHandle<()>) {
        let (mut resolver, client) = DNSResolver::new(max_inflight);
        let jh = thread::spawn(move || {
            resolver.thread_main();
        });
        (client, jh)
    }

    pub fn dns_thread_shutdown(dns_client: DNSClient, thread_handle: thread::JoinHandle<()>) {
        drop(dns_client);
        thread_handle.join().unwrap();
    }

    pub struct TestPeer {
        pub config: TestPeerConfig,
        pub network: PeerNetwork,
        pub burndb: Option<BurnDB>,
        pub miner: TestMiner,
        pub stacks_node: Option<TestStacksNode>,
        chainstate_path: String,
    }

    impl TestPeer {
        pub fn new(config: TestPeerConfig) -> TestPeer {
            let test_path = format!("/tmp/blockstack-test-peer-{}-{}", &config.test_name, config.server_port);
            match fs::metadata(&test_path) {
                Ok(_) => {
                    fs::remove_dir_all(&test_path).unwrap();
                },
                Err(_) => {}
            };

            fs::create_dir_all(&test_path).unwrap();

            let mut miner_factory = TestMinerFactory::new();
            let mut miner = miner_factory.next_miner(&config.burnchain, 1, 1, AddressHashMode::SerializeP2PKH);

            let burndb_path = format!("{}/burn", &test_path);
            let peerdb_path = format!("{}/peers.db", &test_path);
            let chainstate_path = format!("{}/chainstate", &test_path);

            let mut peerdb = PeerDB::connect(&peerdb_path, true, config.network_id, config.burnchain.network_id, config.private_key_expire, config.data_url.clone(), &config.asn4_entries, Some(&config.initial_neighbors)).unwrap();
            let mut burndb = BurnDB::connect(&burndb_path, config.burnchain.first_block_height, &config.burnchain.first_block_hash, get_epoch_time_secs(), true).unwrap();
            let chainstate = StacksChainState::open(false, config.network_id, &chainstate_path).unwrap();
            let mut stacks_node = TestStacksNode::from_chainstate(chainstate);

            {
                let prev_snapshot = {
                    let tx = burndb.tx_begin().unwrap();
                    BurnDB::get_first_block_snapshot(&tx).unwrap()
                };

                let mut fork = TestBurnchainFork::new(prev_snapshot.block_height, &prev_snapshot.burn_header_hash, &prev_snapshot.index_root, 0);
                for i in prev_snapshot.block_height..config.current_block {
                    let burn_block = {
                        let mut tx = burndb.tx_begin().unwrap();
                        let mut burn_block = fork.next_block(&mut tx);
                        stacks_node.add_key_register(&mut burn_block, &mut miner);
                        burn_block
                    };
                    fork.append_block(burn_block);
                    
                    fork.mine_pending_blocks(&mut burndb, &config.burnchain);
                }
            }

            let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), config.server_port);
            let http_local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), config.http_port);

            {
                let mut tx = peerdb.tx_begin().unwrap();
                PeerDB::set_local_ipaddr(&mut tx, &PeerAddress::from_socketaddr(&local_addr), config.server_port).unwrap();
                PeerDB::set_local_services(&mut tx, ServiceFlags::RELAY as u16).unwrap();
                PeerDB::set_local_private_key(&mut tx, &config.private_key, config.private_key_expire).unwrap();
                
                tx.commit().unwrap();
            }
           
            let local_peer = PeerDB::get_local_peer(peerdb.conn()).unwrap();
            let burnchain_view = {
                let mut tx = burndb.tx_begin().unwrap();
                BurnDB::get_burnchain_view(&mut tx, &config.burnchain).unwrap()
            };
            let mut peer_network = PeerNetwork::new(peerdb, local_peer, config.peer_version, config.burnchain.clone(), burnchain_view, config.connection_opts.clone());

            peer_network.bind(&local_addr, &http_local_addr).unwrap();
            
            TestPeer {
                config: config,
                network: peer_network,
                burndb: Some(burndb),
                miner: miner,
                stacks_node: Some(stacks_node),
                chainstate_path: chainstate_path
            }
        }

        pub fn connect_initial(&mut self) -> Result<(), net_error> {
            let local_peer = PeerDB::get_local_peer(self.network.peerdb.conn()).unwrap();
            let chain_view = match self.burndb {
                Some(ref mut burndb) => {
                    let mut tx = burndb.tx_begin().unwrap();
                    BurnDB::get_burnchain_view(&mut tx, &self.config.burnchain).unwrap()
                }
                None => panic!("Misconfigured peer: no burndb")
            };

            self.network.local_peer = local_peer;
            self.network.chain_view = chain_view;

            for n in self.config.initial_neighbors.iter() {
                self.network.connect_peer(&n.addr)
                    .and_then(|e| Ok(()))?;
            }
            Ok(())
        }

        pub fn step(&mut self) -> Result<NetworkResult, net_error> {
            let mut burndb = self.burndb.take().unwrap();
            let mut stacks_node = self.stacks_node.take().unwrap();

            let ret = self.network.run(&mut burndb, &mut stacks_node.chainstate, None, 10);

            self.burndb = Some(burndb);
            self.stacks_node = Some(stacks_node);

            ret
        }
        
        pub fn step_dns(&mut self, dns_client: &mut DNSClient) -> Result<NetworkResult, net_error> {
            let mut burndb = self.burndb.take().unwrap();
            let mut stacks_node = self.stacks_node.take().unwrap();

            let ret = self.network.run(&mut burndb, &mut stacks_node.chainstate, Some(dns_client), 10);

            self.burndb = Some(burndb);
            self.stacks_node = Some(stacks_node);

            ret
        }

        // this is a fake block -- don't try inserting it
        pub fn empty_burnchain_block(&self, block_height: u64) -> BurnchainBlock {
            assert!(block_height + 1 >= self.config.burnchain.first_block_height);
            let prev_block_height = block_height - 1;

            let block_hash_i = Uint256::from_u64(block_height);
            let mut block_hash_bytes = [0u8; 32];
            block_hash_bytes.copy_from_slice(&block_hash_i.to_u8_slice());
            
            let prev_block_hash_i = Uint256::from_u64(prev_block_height);
            let mut prev_block_hash_bytes = [0u8; 32];
            prev_block_hash_bytes.copy_from_slice(&prev_block_hash_i.to_u8_slice());

            BurnchainBlock::Bitcoin(BitcoinBlock {
                block_height: block_height + 1,
                block_hash: BurnchainHeaderHash(block_hash_bytes),
                parent_block_hash: BurnchainHeaderHash(prev_block_hash_bytes),
                txs: vec![],
                timestamp: get_epoch_time_secs()
            })
        }

        fn make_empty_burnchain_block(&mut self) -> BurnchainBlock {
            let empty_block = {
                let burndb = self.burndb.take().unwrap();
                let sn = BurnDB::get_canonical_burn_chain_tip(burndb.conn()).unwrap();
                let empty_block = self.empty_burnchain_block(sn.block_height);
                self.burndb = Some(burndb);
                empty_block
            };
            empty_block
        }

        pub fn next_burnchain_block(&mut self, mut blockstack_ops: Vec<BlockstackOperationType>) -> u64 {
            let mut burndb = self.burndb.take().unwrap();
            let block_height = {
                let mut tx = burndb.tx_begin().unwrap();
                let tip = BurnDB::get_canonical_burn_chain_tip(&tx).unwrap();
                let block_header = BurnchainBlockHeader::from_parent_snapshot(&tip, BurnchainHeaderHash::from_test_data(tip.block_height + 1, &TrieHash([0u8; 32]), 12345), blockstack_ops.len() as u64);

                for op in blockstack_ops.iter_mut() {
                    match op {
                        BlockstackOperationType::LeaderKeyRegister(ref mut data) => {
                            data.burn_header_hash = block_header.block_hash.clone();
                            data.consensus_hash = tip.consensus_hash.clone();
                        },
                        BlockstackOperationType::LeaderBlockCommit(ref mut data) => {
                            data.burn_header_hash = block_header.block_hash.clone();
                        },
                        BlockstackOperationType::UserBurnSupport(ref mut data) => {
                            data.burn_header_hash = block_header.block_hash.clone();
                            data.consensus_hash = tip.consensus_hash.clone();
                        }
                    }
                }
                        
                Burnchain::process_block_txs(&mut tx, &tip, &block_header, &self.config.burnchain, blockstack_ops).unwrap();
                tx.commit().unwrap();
                block_header.block_height
            };
            self.burndb = Some(burndb);
            block_height
        }

        pub fn preprocess_stacks_block(&mut self, block: &StacksBlock) -> Result<bool, String> {
            let mut burndb = self.burndb.take().unwrap();
            let mut node = self.stacks_node.take().unwrap();
            let res = {
                let mut tx = burndb.tx_begin().unwrap();
                let tip = BurnDB::get_canonical_burn_chain_tip(&tx).unwrap();
                let sn_opt = BurnDB::get_block_snapshot_for_winning_stacks_block(&mut tx, &tip.burn_header_hash, &block.block_hash()).unwrap();
                if sn_opt.is_none() {
                    return Err(format!("No such block in canonical burn fork: {}", &block.block_hash()));
                }
                let sn = sn_opt.unwrap();
                node.chainstate.preprocess_anchored_block(&mut tx, &sn.burn_header_hash, sn.burn_header_timestamp, block, &sn.parent_burn_header_hash)
                    .map_err(|e| format!("Failed to preprocess anchored block: {:?}", &e))
            };
            self.burndb = Some(burndb);
            self.stacks_node = Some(node);
            res
        }
        
        pub fn preprocess_stacks_microblocks(&mut self, microblocks: &Vec<StacksMicroblock>) -> Result<bool, String> {
            assert!(microblocks.len() > 0);
            let mut burndb = self.burndb.take().unwrap();
            let mut node = self.stacks_node.take().unwrap();
            let res = {
                let mut tx = burndb.tx_begin().unwrap();
                let tip = BurnDB::get_canonical_burn_chain_tip(&tx).unwrap();
                let anchor_block_hash = microblocks[0].header.prev_block.clone();
                let sn_opt = BurnDB::get_block_snapshot_for_winning_stacks_block(&mut tx, &tip.burn_header_hash, &anchor_block_hash).unwrap();
                if sn_opt.is_none() {
                    return Err(format!("No such anchor block in canonical burn fork: {:?}", anchor_block_hash));
                }
                let sn = sn_opt.unwrap();
                let mut res = Ok(true);
                for mblock in microblocks.iter() {
                    res = node.chainstate.preprocess_streamed_microblock(&sn.burn_header_hash, &anchor_block_hash, mblock)
                        .map_err(|e| format!("Failed to preprocess microblock: {:?}", &e));

                    if res.is_err() {
                        break;
                    }
                }
                res
            };
            
            self.burndb = Some(burndb);
            self.stacks_node = Some(node);
            res
        }

        pub fn process_stacks_epoch_at_tip(&mut self, block: &StacksBlock, microblocks: &Vec<StacksMicroblock>) -> () {
            let mut burndb = self.burndb.take().unwrap();
            let mut node = self.stacks_node.take().unwrap();
            {
                let mut tx = burndb.tx_begin().unwrap();
                let tip = BurnDB::get_canonical_burn_chain_tip(&tx).unwrap();
                node.chainstate.preprocess_stacks_epoch(&mut tx, &tip, block, microblocks).unwrap();
            }
    
            loop {
                let processed = node.chainstate.process_blocks(1).unwrap();
                if processed.len() == 0 {
                    break;
                }
                match processed[0] {
                    (Some(ref header_info), _) => {
                        continue;
                    },
                    (None, _) => {
                        break;
                    }
                }
            }

            self.burndb = Some(burndb);
            self.stacks_node = Some(node);
        }

        pub fn add_empty_burnchain_block(&mut self) -> u64 {
            self.next_burnchain_block(vec![])
        }

        pub fn chainstate(&mut self) -> &mut StacksChainState {
            &mut self.stacks_node.as_mut().unwrap().chainstate
        }

        // have this peer produce an anchored block and microblock tail using its internal miner.
        pub fn make_default_tenure(&mut self) -> (Vec<BlockstackOperationType>, StacksBlock, Vec<StacksMicroblock>) {
            let mut burndb = self.burndb.take().unwrap();
            let mut burn_block = {
                let sn = BurnDB::get_canonical_burn_chain_tip(burndb.conn()).unwrap();
                TestBurnchainBlock::new(&sn, 0)
            };
          
            let mut stacks_node = self.stacks_node.take().unwrap();

            let parent_block_opt = stacks_node.get_last_anchored_block(&self.miner);
            let parent_microblock_header_opt = get_last_microblock_header(&stacks_node, &self.miner, parent_block_opt.as_ref());
            let last_key = stacks_node.get_last_key(&self.miner);

            let network_id = self.config.network_id;
            let chainstate_path = self.chainstate_path.clone();
            let burn_block_height = burn_block.block_height;

            let (stacks_block, microblocks, block_commit_op) = stacks_node.mine_stacks_block(&mut burndb, &mut self.miner, &mut burn_block, &last_key, parent_block_opt.as_ref(), 1000, |mut builder, ref mut miner| {
                let mut miner_chainstate = StacksChainState::open(false, network_id, &chainstate_path).unwrap();
                let mut epoch = builder.epoch_begin(&mut miner_chainstate).unwrap();

                let (stacks_block, microblocks) = mine_smart_contract_block_contract_call_microblock(&mut epoch, &mut builder, miner, burn_block_height as usize, parent_microblock_header_opt.as_ref());

                builder.epoch_finish(epoch);
                (stacks_block, microblocks)
            });
            
            let leader_key_op = stacks_node.add_key_register(&mut burn_block, &mut self.miner);

            self.stacks_node = Some(stacks_node);
            self.burndb = Some(burndb);
            (vec![BlockstackOperationType::LeaderKeyRegister(leader_key_op), BlockstackOperationType::LeaderBlockCommit(block_commit_op)], stacks_block, microblocks)
        }

        pub fn to_neighbor(&self) -> Neighbor {
            self.config.to_neighbor()
        }

        pub fn to_peer_host(&self) -> PeerHost {
            self.config.to_peer_host()
        }

        pub fn get_public_key(&self) -> Secp256k1PublicKey {
            let local_peer = PeerDB::get_local_peer(&self.network.peerdb.conn()).unwrap();
            Secp256k1PublicKey::from_private(&local_peer.private_key)
        }

        pub fn get_peerdb_conn(&self) -> &DBConn {
            self.network.peerdb.conn()
        }

        pub fn get_burnchain_view(&mut self) -> Result<BurnchainView, db_error> {
            let mut burndb = self.burndb.take().unwrap();
            let view_res = {
                let mut tx = burndb.tx_begin().unwrap();
                BurnDB::get_burnchain_view(&mut tx, &self.config.burnchain)
            };
            self.burndb = Some(burndb);
            view_res
        }

        pub fn dump_frontier(&self) -> () {
            let conn = self.network.peerdb.conn();
            let peers = PeerDB::get_all_peers(conn).unwrap();
            info!("--- BEGIN ALL PEERS ({}) ---", peers.len());
            info!("{:#?}", &peers);
            info!("--- END ALL PEERS ({}) -----", peers.len());
        }
    }
}
