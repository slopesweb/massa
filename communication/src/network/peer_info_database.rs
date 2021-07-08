use super::config::NetworkConfig;
use crate::error::{ChannelError, CommunicationError, NetworkConnectionErrorType};
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use serde_json::json;
use serde_json::Value;
use std::collections::HashMap;
use std::net::IpAddr;
use time::UTime;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};

#[derive(Clone, Copy, Serialize, Deserialize, Debug)]
pub struct PeerInfo {
    pub ip: IpAddr,
    pub banned: bool,
    pub bootstrap: bool,
    pub last_alive: Option<UTime>,
    pub last_failure: Option<UTime>,
    pub advertised: bool,

    #[serde(default = "usize::default")]
    pub active_out_connection_attempts: usize,
    #[serde(default = "usize::default")]
    pub active_out_connections: usize,
    #[serde(default = "usize::default")]
    pub active_in_connections: usize,
}

impl PeerInfo {
    /// true if there is at least one connection attempt /
    ///  one active connection in either direction
    /// with this peer
    fn is_active(&self) -> bool {
        self.active_out_connection_attempts > 0
            || self.active_out_connections > 0
            || self.active_in_connections > 0
    }
}

pub struct PeerInfoDatabase {
    cfg: NetworkConfig,
    peers: HashMap<IpAddr, PeerInfo>,
    saver_join_handle: JoinHandle<()>,
    saver_watch_tx: watch::Sender<HashMap<IpAddr, PeerInfo>>,
    active_out_connection_attempts: usize,
    active_out_connections: usize,
    active_in_connections: usize,
    wakeup_interval: UTime,
}

/// Saves banned, advertised and bootstrap peers to a file.
/// Can return an error if the writing fails.
async fn dump_peers(
    peers: &HashMap<IpAddr, PeerInfo>,
    file_path: &std::path::PathBuf,
) -> Result<(), CommunicationError> {
    let peer_vec: Vec<Value> = peers
        .values()
        .filter(|v| v.banned || v.advertised || v.bootstrap)
        //        .cloned()
        .map(|peer| {
            json!({
                "ip": peer.ip,
                "banned": peer.banned,
                "bootstrap": peer.bootstrap,
                "last_alive": peer.last_alive,
                "last_failure": peer.last_failure,
                "advertised": peer.advertised,
            })
        })
        .collect();

    tokio::fs::write(file_path, serde_json::to_string_pretty(&peer_vec)?).await?;
    Ok(())
}

/// Cleans up the peer database using max values
/// provided by NetworkConfig.ProtocolConfig.
/// If opt_new_peers is provided, adds its contents as well.
///
/// Note: only non-active, non-bootstrap peers are counted when clipping to size limits.
fn cleanup_peers(
    cfg: &NetworkConfig,
    peers: &mut HashMap<IpAddr, PeerInfo>,
    opt_new_peers: Option<&Vec<IpAddr>>,
) {
    // filter and map new peers, remove duplicates
    let mut res_new_peers: Vec<PeerInfo> = if let Some(new_peers) = opt_new_peers {
        new_peers
            .iter()
            .unique()
            .filter(|&ip| {
                if let Some(mut p) = peers.get_mut(&ip) {
                    // avoid already-known IPs, but mark them as advertised
                    p.advertised = true;
                    return false;
                }
                if !ip.is_global() {
                    // avoid non-global IPs
                    return false;
                }
                if let Some(our_ip) = cfg.routable_ip {
                    // avoid our own IP
                    if ip == &our_ip {
                        return false;
                    }
                }
                true
            })
            .take(cfg.max_advertise_length)
            .map(|&ip| PeerInfo {
                ip,
                banned: false,
                bootstrap: false,
                last_alive: None,
                last_failure: None,
                advertised: true,
                active_out_connection_attempts: 0,
                active_out_connections: 0,
                active_in_connections: 0,
            })
            .collect()
    } else {
        Vec::new()
    };

    // split between peers that need to be kept (keep_peers),
    // inactive banned peers (banned_peers)
    // and other inactive but advertised peers (idle_peers)
    // drop other peers (inactive non-advertised, non-keep)
    let mut keep_peers: Vec<PeerInfo> = Vec::new();
    let mut banned_peers: Vec<PeerInfo> = Vec::new();
    let mut idle_peers: Vec<PeerInfo> = Vec::new();
    for (ip, p) in peers.drain() {
        if !ip.is_global() {
            // avoid non-global IPs
            continue;
        }
        if let Some(our_ip) = cfg.routable_ip {
            // avoid our own IP
            if ip == our_ip {
                continue;
            }
        }
        if p.bootstrap || p.is_active() {
            keep_peers.push(p);
        } else if p.banned {
            banned_peers.push(p);
        } else if p.advertised {
            idle_peers.push(p);
        } // else drop peer (idle and not advertised)
    }

    // append new peers to idle_peers
    // stable sort to keep new_peers order,
    // also prefer existing peers over new ones
    // truncate to max length
    idle_peers.append(&mut res_new_peers);
    idle_peers.sort_by_key(|&p| (std::cmp::Reverse(p.last_alive), p.last_failure));
    idle_peers.truncate(cfg.max_idle_peers);

    // sort and truncate inactive banned peers
    banned_peers.sort_unstable_by_key(|&p| (std::cmp::Reverse(p.last_failure), p.last_alive));
    banned_peers.truncate(cfg.max_banned_peers);

    // gather everything back
    peers.extend(keep_peers.into_iter().map(|p| (p.ip, p)));
    peers.extend(banned_peers.into_iter().map(|p| (p.ip, p)));
    peers.extend(idle_peers.into_iter().map(|p| (p.ip, p)));
}

impl PeerInfoDatabase {
    /// Creates new peerInfoDatabase from NetworkConfig.
    /// Can fail reading the file containing peers.
    /// will only emit a warning if peers dumping failed.
    pub async fn new(cfg: &NetworkConfig) -> Result<Self, CommunicationError> {
        // wakeup interval
        let wakeup_interval = cfg.wakeup_interval;

        // load from file
        let mut peers = serde_json::from_str::<Vec<PeerInfo>>(
            &tokio::fs::read_to_string(&cfg.peers_file).await?,
        )?
        .into_iter()
        .map(|p| (p.ip, p))
        .collect::<HashMap<IpAddr, PeerInfo>>();

        // cleanup
        cleanup_peers(&cfg, &mut peers, None);

        // setup saver
        let peers_file = cfg.peers_file.clone();
        let peers_file_dump_interval = cfg.peers_file_dump_interval;
        let (saver_watch_tx, mut saver_watch_rx) = watch::channel(peers.clone());
        let mut need_dump = false;
        let saver_join_handle = tokio::spawn(async move {
            let delay = sleep(Duration::from_millis(0));
            tokio::pin!(delay);
            loop {
                tokio::select! {
                    opt_p = saver_watch_rx.changed() => match opt_p {
                        Ok(_) => if !need_dump {
                            delay.set(sleep(peers_file_dump_interval.to_duration()));
                            need_dump = true;
                        },
                        Err(_) => break
                    },
                    _ = &mut delay, if need_dump => {
                        let to_dump = saver_watch_rx.borrow().clone();
                        match dump_peers(&to_dump, &peers_file).await {
                            Ok(_) => { need_dump = false; },
                            Err(e) => {
                                warn!("could not dump peers to file: {}", e);
                                delay.set(sleep(peers_file_dump_interval.to_duration()));
                            }
                        }
                    }
                }
            }
        });

        // return struct
        Ok(PeerInfoDatabase {
            cfg: cfg.clone(),
            peers,
            saver_join_handle,
            saver_watch_tx,
            active_out_connection_attempts: 0,
            active_out_connections: 0,
            active_in_connections: 0,
            wakeup_interval,
        })
    }

    /// Request peers dump to file
    fn request_dump(&self) -> Result<(), CommunicationError> {
        //use map_err to avoir Ok(self.saver_watch_tx.send(self.peers.clone())?)
        //which to unwrap that Ok
        self.saver_watch_tx
            .send(self.peers.clone())
            .map_err(|err| ChannelError::from(err).into())
    }

    /// Cleanly closes peerInfoDatabase, performing one last peer dump.
    /// A warining is raised on dump failure.
    pub async fn stop(self) -> Result<(), CommunicationError> {
        drop(self.saver_watch_tx);
        self.saver_join_handle.await?;
        if let Err(e) = dump_peers(&self.peers, &self.cfg.peers_file).await {
            warn!("could not dump peers to file: {}", e);
        }
        Ok(())
    }

    /// Gets avaible out connection attempts
    /// accordig to NeworkConfig and current connections and connection attempts.
    pub fn get_available_out_connection_attempts(&self) -> usize {
        std::cmp::min(
            self.cfg
                .target_out_connections
                .saturating_sub(self.active_out_connection_attempts)
                .saturating_sub(self.active_out_connections),
            self.cfg
                .max_out_connnection_attempts
                .saturating_sub(self.active_out_connection_attempts),
        )
    }

    /// Sorts peers by ( last_failure, rev(last_success) )
    /// and returns as many peers as there are avaible slots to attempt outgoing connections to.
    pub fn get_out_connection_candidate_ips(&self) -> Result<Vec<IpAddr>, CommunicationError> {
        /*
            get_connect_candidate_ips must return the full sorted list where:
                advertised && !banned && out_connection_attempts==0 && out_connections==0 && in_connections=0
                sorted_by = ( last_failure, rev(last_success) )
        */
        let available_slots = self.get_available_out_connection_attempts();
        if available_slots == 0 {
            return Ok(Vec::new());
        }
        let now = UTime::now()?;
        let mut sorted_peers: Vec<PeerInfo> = self
            .peers
            .values()
            .filter(|&p| {
                if !(p.advertised && !p.banned && !p.is_active()) {
                    return false;
                }
                if let Some(last_failure) = p.last_failure {
                    if let Some(last_alive) = p.last_alive {
                        if last_alive > last_failure {
                            return true;
                        }
                    }
                    return now
                        .saturating_sub(last_failure)
                        .saturating_sub(self.wakeup_interval)
                        > UTime::from(0u64);
                }
                true
            })
            .copied()
            .collect();
        sorted_peers.sort_unstable_by_key(|&p| (p.last_failure, std::cmp::Reverse(p.last_alive)));
        Ok(sorted_peers
            .into_iter()
            .take(available_slots)
            .map(|p| p.ip)
            .collect::<Vec<IpAddr>>())
    }

    pub fn get_peers(&self) -> &HashMap<IpAddr, PeerInfo> {
        &self.peers
    }

    /// Returns a vec of advertisable IpAddrs sorted by ( last_failure, rev(last_success) )
    pub fn get_advertisable_peer_ips(&self) -> Vec<IpAddr> {
        let mut sorted_peers: Vec<PeerInfo> = self
            .peers
            .values()
            .filter(|&p| (p.advertised && !p.banned))
            .copied()
            .collect();
        sorted_peers.sort_unstable_by_key(|&p| (std::cmp::Reverse(p.last_alive), p.last_failure));
        let mut sorted_ips: Vec<IpAddr> = sorted_peers
            .into_iter()
            .take(self.cfg.max_advertise_length)
            .map(|p| p.ip)
            .collect();
        if let Some(our_ip) = self.cfg.routable_ip {
            sorted_ips.insert(0, our_ip);
            sorted_ips.truncate(self.cfg.max_advertise_length);
        }
        sorted_ips
    }

    /// Acknowledges a new out connection attempt to ip.
    ///
    /// Panics if :
    /// - target ip is not global
    /// - there are too many out connection attempts
    /// - ip does not match with a known peer
    pub fn new_out_connection_attempt(&mut self, ip: &IpAddr) -> Result<(), CommunicationError> {
        if !ip.is_global() {
            return Err(CommunicationError::InvalidIpError(ip.clone()));
        }
        if self.get_available_out_connection_attempts() == 0 {
            return Err(CommunicationError::PeerConnectionError(
                NetworkConnectionErrorType::ToManyConnectionAttempt(ip.clone()),
            ));
        }
        self.peers
            .get_mut(&ip)
            .ok_or(CommunicationError::PeerConnectionError(
                NetworkConnectionErrorType::PeerInfoNotFoundError(ip.clone()),
            ))?
            .active_out_connection_attempts += 1;
        self.active_out_connection_attempts += 1;
        Ok(())
    }

    /// Merges new_peers with our peers using the cleanup_peers function.
    /// A dump is requested afterwards.
    pub fn merge_candidate_peers(
        &mut self,
        new_peers: &Vec<IpAddr>,
    ) -> Result<(), CommunicationError> {
        if new_peers.is_empty() {
            return Ok(());
        }
        cleanup_peers(&self.cfg, &mut self.peers, Some(&new_peers));
        self.request_dump()
    }

    /// Sets the peer status as alive.
    /// Panics if ip does not match a known peer.
    /// Requests a subsequent dump.
    pub fn peer_alive(&mut self, ip: &IpAddr) -> Result<(), CommunicationError> {
        self.peers
            .get_mut(&ip)
            .ok_or(CommunicationError::PeerConnectionError(
                NetworkConnectionErrorType::PeerInfoNotFoundError(ip.clone()),
            ))?
            .last_alive = Some(UTime::now()?);
        self.request_dump()
    }

    /// Sets the peer status as failed.
    /// Panics if the peer is unknown.
    /// Requests a dump.
    pub fn peer_failed(&mut self, ip: &IpAddr) -> Result<(), CommunicationError> {
        self.peers
            .get_mut(&ip)
            .ok_or(CommunicationError::PeerConnectionError(
                NetworkConnectionErrorType::PeerInfoNotFoundError(ip.clone()),
            ))?
            .last_failure = Some(UTime::now()?);
        self.request_dump()
    }

    /// Sets that the peer is banned now.
    /// Panics if the ip does not match an unknown peer.
    /// If the peer is not active, the database is cleaned up.
    /// A dump is requested.
    pub fn peer_banned(&mut self, ip: &IpAddr) -> Result<(), CommunicationError> {
        let peer = self
            .peers
            .get_mut(&ip)
            .ok_or(CommunicationError::PeerConnectionError(
                NetworkConnectionErrorType::PeerInfoNotFoundError(ip.clone()),
            ))?;
        peer.last_failure = Some(UTime::now()?);
        if !peer.banned {
            peer.banned = true;
            if !peer.is_active() && !peer.bootstrap {
                cleanup_peers(&self.cfg, &mut self.peers, None);
            }
        }
        self.request_dump()
    }

    /// Notifies of a closed outgoing connection.
    ///
    /// Panics if :
    /// - too many out connections closed
    /// - the peer is unknown
    /// - too many out connections closed for that peer
    ///
    /// If the peer is not active nor bootstrap,
    /// peers are cleaned up and a dump is requested
    pub fn out_connection_closed(&mut self, ip: &IpAddr) -> Result<(), CommunicationError> {
        if self.active_out_connections == 0 {
            return Err(CommunicationError::PeerConnectionError(
                NetworkConnectionErrorType::CloseConnectionWithNoConnectionToClose(ip.clone()),
            ));
        }
        let peer = self
            .peers
            .get_mut(&ip)
            .ok_or(CommunicationError::PeerConnectionError(
                NetworkConnectionErrorType::PeerInfoNotFoundError(ip.clone()),
            ))?;

        if peer.active_out_connections == 0 {
            return Err(CommunicationError::PeerConnectionError(
                NetworkConnectionErrorType::CloseConnectionWithNoConnectionToClose(ip.clone()),
            ));
        }
        self.active_out_connections -= 1;
        peer.active_out_connections -= 1;
        if !peer.is_active() && !peer.bootstrap {
            cleanup_peers(&self.cfg, &mut self.peers, None);
            self.request_dump()
        } else {
            Ok(())
        }
    }

    /// Notifies that an inbound connection is closed.
    ///
    /// Panics if :
    /// - too many in connections closed
    /// - the peer is unknown
    /// - too many in connections closed for that peer
    ///
    /// If the peer is not active nor bootstrap
    /// peers are cleaned up and a dump is requested.
    pub fn in_connection_closed(&mut self, ip: &IpAddr) -> Result<(), CommunicationError> {
        if self.active_in_connections == 0 {
            return Err(CommunicationError::PeerConnectionError(
                NetworkConnectionErrorType::CloseConnectionWithNoConnectionToClose(ip.clone()),
            ));
        }
        let peer = self
            .peers
            .get_mut(&ip)
            .ok_or(CommunicationError::PeerConnectionError(
                NetworkConnectionErrorType::PeerInfoNotFoundError(ip.clone()),
            ))?;

        if peer.active_in_connections == 0 {
            return Err(CommunicationError::PeerConnectionError(
                NetworkConnectionErrorType::CloseConnectionWithNoConnectionToClose(ip.clone()),
            ));
        }
        self.active_in_connections -= 1;
        peer.active_in_connections -= 1;
        if !peer.is_active() && !peer.bootstrap {
            cleanup_peers(&self.cfg, &mut self.peers, None);
            self.request_dump()
        } else {
            Ok(())
        }
    }

    /// Yay an out connection attempt succeded.
    /// returns false if there are no slots left for out connections.
    /// The peer is set to advertized.
    ///
    /// Panics if :
    /// - too many out connection attempts succeeded
    /// - an unknown peer connection attempt succeeded
    /// - too many out connection attempts succeded for that peer
    ///
    /// A dump is requested.
    pub fn try_out_connection_attempt_success(
        &mut self,
        ip: &IpAddr,
    ) -> Result<bool, CommunicationError> {
        // a connection attempt succeeded
        // remove out connection attempt and add out connection
        if self.active_out_connection_attempts == 0 {
            return Err(CommunicationError::PeerConnectionError(
                NetworkConnectionErrorType::ToManyConnectionAttempt(ip.clone()),
            ));
        }
        if self.active_out_connections >= self.cfg.target_out_connections {
            return Ok(false);
        }
        let peer = self
            .peers
            .get_mut(&ip)
            .ok_or(CommunicationError::PeerConnectionError(
                NetworkConnectionErrorType::PeerInfoNotFoundError(ip.clone()),
            ))?;

        if peer.active_out_connection_attempts == 0 {
            return Err(CommunicationError::PeerConnectionError(
                NetworkConnectionErrorType::ToManyConnectionAttempt(ip.clone()),
            ));
        }
        self.active_out_connection_attempts -= 1;
        peer.active_out_connection_attempts -= 1;
        peer.advertised = true; // we just connected to it. Assume advertised.
        if peer.banned {
            peer.last_failure = Some(UTime::now()?);
            if !peer.is_active() && !peer.bootstrap {
                cleanup_peers(&self.cfg, &mut self.peers, None);
            }
            self.request_dump()?;
            return Ok(false);
        }
        self.active_out_connections += 1;
        peer.active_out_connections += 1;
        self.request_dump()?;
        Ok(true)
    }

    /// Oh no an out connection attempt failed.
    ///
    /// Panics if:
    /// - too many out connection attempts failed
    /// - an unknown peer connection attempt failed
    /// - too many out connection attampts failed for tha peer
    ///
    /// A dump is requested.
    pub fn out_connection_attempt_failed(&mut self, ip: &IpAddr) -> Result<(), CommunicationError> {
        if self.active_out_connection_attempts == 0 {
            return Err(CommunicationError::PeerConnectionError(
                NetworkConnectionErrorType::ToManyConnectionFailure(ip.clone()),
            ));
        }
        let peer = self
            .peers
            .get_mut(&ip)
            .ok_or(CommunicationError::PeerConnectionError(
                NetworkConnectionErrorType::PeerInfoNotFoundError(ip.clone()),
            ))?;
        if peer.active_out_connection_attempts == 0 {
            return Err(CommunicationError::PeerConnectionError(
                NetworkConnectionErrorType::ToManyConnectionFailure(ip.clone()),
            ));
        }
        self.active_out_connection_attempts -= 1;
        peer.active_out_connection_attempts -= 1;
        peer.last_failure = Some(UTime::now()?);
        if !peer.is_active() && !peer.bootstrap {
            cleanup_peers(&self.cfg, &mut self.peers, None);
        }
        self.request_dump()
    }

    /// An ip has successfully connected to us.
    /// returns true if some in slots for connections are left.
    /// If the corresponding peer exists, it is updated,
    /// otherwise it is created (not advertised).
    /// A dump is requested.
    pub fn try_new_in_connection(&mut self, ip: &IpAddr) -> Result<bool, CommunicationError> {
        // try to create a new input connection, return false if no slots
        if !ip.is_global()
            || self.active_in_connections >= self.cfg.max_in_connections
            || self.cfg.max_in_connections_per_ip == 0
        {
            return Ok(false);
        }
        if let Some(our_ip) = self.cfg.routable_ip {
            // avoid our own IP
            if *ip == our_ip {
                warn!("incomming connection from our own IP");
                return Ok(false);
            }
        }
        let peer = self.peers.entry(*ip).or_insert(PeerInfo {
            ip: *ip,
            banned: false,
            bootstrap: false,
            last_alive: None,
            last_failure: None,
            advertised: false,
            active_out_connection_attempts: 0,
            active_out_connections: 0,
            active_in_connections: 0,
        });
        if peer.banned {
            massa_trace!("in_connection_refused_peer_banned", {"ip": peer.ip});
            peer.last_failure = Some(UTime::now()?);
            self.request_dump()?;
            return Ok(false);
        }
        if peer.active_in_connections >= self.cfg.max_in_connections_per_ip {
            self.request_dump()?;
            return Ok(false);
        }
        self.active_in_connections += 1;
        peer.active_in_connections += 1;
        self.request_dump()?;
        Ok(true)
    }
}

//to start alone RUST_BACKTRACE=1 cargo test peer_info_database -- --nocapture --test-threads=1
#[cfg(test)]
mod tests {
    use super::super::config::NetworkConfig;
    use super::*;

    #[tokio::test]
    async fn test_try_new_in_connection_in_connection_closed() {
        let mut network_config = example_network_config();
        network_config.target_out_connections = 5;
        let mut peers: HashMap<IpAddr, PeerInfo> = HashMap::new();

        //add peers
        //peer Ok, return
        let connected_peers1 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)));
        peers.insert(connected_peers1.ip.clone(), connected_peers1);
        let mut connected_peers1 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 12)));
        connected_peers1.bootstrap = true;
        connected_peers1.banned = true;
        peers.insert(connected_peers1.ip.clone(), connected_peers1);

        let wakeup_interval = network_config.wakeup_interval;
        let (saver_watch_tx, mut saver_watch_rx) = watch::channel(peers.clone());

        let saver_join_handle = tokio::spawn(async move {
            loop {
                match saver_watch_rx.changed().await {
                    Ok(()) => (),
                    _ => break,
                }
            }
        });

        let mut db = PeerInfoDatabase {
            cfg: network_config,
            peers,
            saver_join_handle,
            saver_watch_tx,
            active_out_connection_attempts: 0,
            active_out_connections: 0,
            active_in_connections: 0,
            wakeup_interval,
        };

        //test with no connection attempt before
        let res = db.in_connection_closed(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)));
        if let Err(CommunicationError::PeerConnectionError(
            NetworkConnectionErrorType::CloseConnectionWithNoConnectionToClose(ip_err),
        )) = res
        {
            assert_eq!(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)), ip_err);
        } else {
            assert!(false, "ToManyConnectionAttempt error not return");
        }

        let res = db
            .try_new_in_connection(&IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 0, 11)))
            .unwrap();
        assert!(!res, "not global ip not detected.");
        let res = db
            .try_new_in_connection(&IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
            .unwrap();
        assert!(!res, "local ip not detected.");

        let res = db
            .try_new_in_connection(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)))
            .unwrap();
        assert!(res, "in connection not accepted.");
        let res = db
            .try_new_in_connection(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 12)))
            .unwrap();
        assert!(!res, "banned peer not detected.");

        //test with a not connected peer
        let res = db.in_connection_closed(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 12)));
        if let Err(CommunicationError::PeerConnectionError(
            NetworkConnectionErrorType::CloseConnectionWithNoConnectionToClose(ip_err),
        )) = res
        {
            assert_eq!(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 12)), ip_err);
        } else {
            assert!(false, "ToManyConnectionAttempt error not return");
        }

        //test with a not connected peer
        let res = db.in_connection_closed(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 13)));
        if let Err(CommunicationError::PeerConnectionError(
            NetworkConnectionErrorType::PeerInfoNotFoundError(ip_err),
        )) = res
        {
            assert_eq!(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 13)), ip_err);
        } else {
            assert!(false, "PeerInfoNotFoundError error not return");
        }

        db.in_connection_closed(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)))
            .unwrap();
        let res = db.in_connection_closed(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)));
        if let Err(CommunicationError::PeerConnectionError(
            NetworkConnectionErrorType::CloseConnectionWithNoConnectionToClose(ip_err),
        )) = res
        {
            assert_eq!(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)), ip_err);
        } else {
            assert!(false, "ToManyConnectionAttempt error not return");
        }
    }

    #[tokio::test]
    async fn test_out_connection_attempt_failed() {
        let mut network_config = example_network_config();
        network_config.target_out_connections = 5;
        let mut peers: HashMap<IpAddr, PeerInfo> = HashMap::new();

        //add peers
        //peer Ok, return
        let connected_peers1 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)));
        peers.insert(connected_peers1.ip.clone(), connected_peers1);
        let mut connected_peers1 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 12)));
        connected_peers1.bootstrap = true;
        connected_peers1.banned = true;
        peers.insert(connected_peers1.ip.clone(), connected_peers1);

        let wakeup_interval = network_config.wakeup_interval;
        let (saver_watch_tx, mut saver_watch_rx) = watch::channel(peers.clone());

        let saver_join_handle = tokio::spawn(async move {
            loop {
                match saver_watch_rx.changed().await {
                    Ok(()) => (),
                    _ => break,
                }
            }
        });

        let mut db = PeerInfoDatabase {
            cfg: network_config,
            peers,
            saver_join_handle,
            saver_watch_tx,
            active_out_connection_attempts: 0,
            active_out_connections: 0,
            active_in_connections: 0,
            wakeup_interval,
        };

        //test with no connection attempt before
        let res =
            db.out_connection_attempt_failed(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)));
        if let Err(CommunicationError::PeerConnectionError(
            NetworkConnectionErrorType::ToManyConnectionFailure(ip_err),
        )) = res
        {
            assert_eq!(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)), ip_err);
        } else {
            println!("res:{:?}", res);
            assert!(false, "ToManyConnectionFailure error not return");
        }

        db.new_out_connection_attempt(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)))
            .unwrap();

        //peer not found.
        let res =
            db.out_connection_attempt_failed(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 13)));
        if let Err(CommunicationError::PeerConnectionError(
            NetworkConnectionErrorType::PeerInfoNotFoundError(ip_err),
        )) = res
        {
            assert_eq!(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 13)), ip_err);
        } else {
            println!("res:{:?}", res);
            assert!(false, "PeerInfoNotFoundError error not return");
        }
        //peer with no attempt.
        let res =
            db.out_connection_attempt_failed(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 12)));
        if let Err(CommunicationError::PeerConnectionError(
            NetworkConnectionErrorType::ToManyConnectionFailure(ip_err),
        )) = res
        {
            assert_eq!(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 12)), ip_err);
        } else {
            println!("res:{:?}", res);
            assert!(false, "ToManyConnectionFailure error not return");
        }

        //call ok.
        db.out_connection_attempt_failed(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)))
            .expect("out_connection_attempt_failed failed");

        let res =
            db.out_connection_attempt_failed(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)));
        if let Err(CommunicationError::PeerConnectionError(
            NetworkConnectionErrorType::ToManyConnectionFailure(ip_err),
        )) = res
        {
            assert_eq!(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)), ip_err);
        } else {
            assert!(false, "ToManyConnectionFailure error not return");
        }
    }

    #[tokio::test]
    async fn test_try_out_connection_attempt_success() {
        let mut network_config = example_network_config();
        network_config.target_out_connections = 5;
        let mut peers: HashMap<IpAddr, PeerInfo> = HashMap::new();

        //add peers
        //peer Ok, return
        let connected_peers1 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)));
        peers.insert(connected_peers1.ip.clone(), connected_peers1);
        let mut connected_peers1 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 12)));
        connected_peers1.bootstrap = true;
        connected_peers1.banned = true;
        peers.insert(connected_peers1.ip.clone(), connected_peers1);

        let wakeup_interval = network_config.wakeup_interval;
        let (saver_watch_tx, mut saver_watch_rx) = watch::channel(peers.clone());

        let saver_join_handle = tokio::spawn(async move {
            loop {
                match saver_watch_rx.changed().await {
                    Ok(()) => (),
                    _ => break,
                }
            }
        });

        let mut db = PeerInfoDatabase {
            cfg: network_config,
            peers,
            saver_join_handle,
            saver_watch_tx,
            active_out_connection_attempts: 0,
            active_out_connections: 0,
            active_in_connections: 0,
            wakeup_interval,
        };

        //test with no connection attempt before
        let res = db.try_out_connection_attempt_success(&IpAddr::V4(std::net::Ipv4Addr::new(
            169, 202, 0, 11,
        )));
        if let Err(CommunicationError::PeerConnectionError(
            NetworkConnectionErrorType::ToManyConnectionAttempt(ip_err),
        )) = res
        {
            assert_eq!(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)), ip_err);
        } else {
            assert!(false, "ToManyConnectionAttempt error not return");
        }

        db.new_out_connection_attempt(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)))
            .unwrap();

        //peer not found.
        let res = db.try_out_connection_attempt_success(&IpAddr::V4(std::net::Ipv4Addr::new(
            169, 202, 0, 13,
        )));
        if let Err(CommunicationError::PeerConnectionError(
            NetworkConnectionErrorType::PeerInfoNotFoundError(ip_err),
        )) = res
        {
            assert_eq!(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 13)), ip_err);
        } else {
            println!("res:{:?}", res);
            assert!(false, "PeerInfoNotFoundError error not return");
        }

        let res = db
            .try_out_connection_attempt_success(&IpAddr::V4(std::net::Ipv4Addr::new(
                169, 202, 0, 11,
            )))
            .unwrap();
        assert!(res, "try_out_connection_attempt_success failed");

        let res = db.try_out_connection_attempt_success(&IpAddr::V4(std::net::Ipv4Addr::new(
            169, 202, 0, 12,
        )));
        if let Err(CommunicationError::PeerConnectionError(
            NetworkConnectionErrorType::ToManyConnectionAttempt(ip_err),
        )) = res
        {
            assert_eq!(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 12)), ip_err);
        } else {
            assert!(false, "PeerInfoNotFoundError error not return");
        }

        db.new_out_connection_attempt(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 12)))
            .unwrap();
        let res = db
            .try_out_connection_attempt_success(&IpAddr::V4(std::net::Ipv4Addr::new(
                169, 202, 0, 12,
            )))
            .unwrap();
        assert!(!res, "try_out_connection_attempt_success not banned");
    }

    #[tokio::test]
    async fn test_new_out_connection_closed() {
        let mut network_config = example_network_config();
        network_config.max_out_connnection_attempts = 5;
        let mut peers: HashMap<IpAddr, PeerInfo> = HashMap::new();

        //add peers
        //peer Ok, return
        let connected_peers1 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)));
        peers.insert(connected_peers1.ip.clone(), connected_peers1);
        let wakeup_interval = network_config.wakeup_interval;
        let (saver_watch_tx, mut saver_watch_rx) = watch::channel(peers.clone());
        let saver_join_handle = tokio::spawn(async move {
            loop {
                match saver_watch_rx.changed().await {
                    Ok(()) => (),
                    _ => break,
                }
            }
        });

        let mut db = PeerInfoDatabase {
            cfg: network_config,
            peers,
            saver_join_handle,
            saver_watch_tx,
            active_out_connection_attempts: 0,
            active_out_connections: 0,
            active_in_connections: 0,
            wakeup_interval,
        };

        //
        let res = db.out_connection_closed(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)));
        if let Err(CommunicationError::PeerConnectionError(
            NetworkConnectionErrorType::CloseConnectionWithNoConnectionToClose(ip_err),
        )) = res
        {
            assert_eq!(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)), ip_err);
        } else {
            assert!(
                false,
                "CloseConnectionWithNoConnectionToClose error not return"
            );
        }

        //add a new connection attempt
        db.new_out_connection_attempt(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)))
            .unwrap();
        let res = db
            .try_out_connection_attempt_success(&IpAddr::V4(std::net::Ipv4Addr::new(
                169, 202, 0, 11,
            )))
            .unwrap();
        assert!(res, "try_out_connection_attempt_success failed");

        let res = db.out_connection_closed(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 12)));
        if let Err(CommunicationError::PeerConnectionError(
            NetworkConnectionErrorType::PeerInfoNotFoundError(ip_err),
        )) = res
        {
            assert_eq!(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 12)), ip_err);
        } else {
            assert!(false, "PeerInfoNotFoundError error not return");
        }

        db.out_connection_closed(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)))
            .unwrap();
        let res = db.out_connection_closed(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)));
        if let Err(CommunicationError::PeerConnectionError(
            NetworkConnectionErrorType::CloseConnectionWithNoConnectionToClose(ip_err),
        )) = res
        {
            assert_eq!(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)), ip_err);
        } else {
            assert!(
                false,
                "CloseConnectionWithNoConnectionToClose error not return"
            );
        }
    }

    #[tokio::test]
    async fn test_new_out_connection_attempt() {
        let mut network_config = example_network_config();
        network_config.max_out_connnection_attempts = 5;
        let mut peers: HashMap<IpAddr, PeerInfo> = HashMap::new();

        //add peers
        //peer Ok, return
        let connected_peers1 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)));
        peers.insert(connected_peers1.ip.clone(), connected_peers1);
        let wakeup_interval = network_config.wakeup_interval;
        let (saver_watch_tx, _) = watch::channel(peers.clone());
        let saver_join_handle = tokio::spawn(async move {});

        let mut db = PeerInfoDatabase {
            cfg: network_config,
            peers,
            saver_join_handle,
            saver_watch_tx,
            active_out_connection_attempts: 0,
            active_out_connections: 0,
            active_in_connections: 0,
            wakeup_interval,
        };

        //test with no peers.
        let res =
            db.new_out_connection_attempt(&IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 0, 11)));
        if let Err(CommunicationError::InvalidIpError(ip_err)) = res {
            assert_eq!(IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 0, 11)), ip_err);
        } else {
            assert!(false, "InvalidIpError not return");
        }

        let res =
            db.new_out_connection_attempt(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 12)));
        if let Err(CommunicationError::PeerConnectionError(
            NetworkConnectionErrorType::PeerInfoNotFoundError(ip_err),
        )) = res
        {
            assert_eq!(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 12)), ip_err);
        } else {
            assert!(false, "PeerInfoNotFoundError error not return");
        }

        (0..5).for_each(|_| {
            db.new_out_connection_attempt(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)))
                .unwrap()
        });
        let res =
            db.new_out_connection_attempt(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)));
        if let Err(CommunicationError::PeerConnectionError(
            NetworkConnectionErrorType::ToManyConnectionAttempt(ip_err),
        )) = res
        {
            assert_eq!(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)), ip_err);
        } else {
            assert!(false, "ToManyConnectionAttempt error not return");
        }
    }

    #[tokio::test]
    async fn test_get_advertisable_peer_ips() {
        let network_config = example_network_config();
        let mut peers: HashMap<IpAddr, PeerInfo> = HashMap::new();

        //add peers
        //peer Ok, return
        let connected_peers1 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)));
        peers.insert(connected_peers1.ip.clone(), connected_peers1);
        //peer banned not return.
        let mut banned_host1 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 23)));
        banned_host1.bootstrap = true;
        banned_host1.banned = true;
        banned_host1.last_alive = Some(UTime::now().unwrap().checked_sub(1000.into()).unwrap());
        peers.insert(banned_host1.ip.clone(), banned_host1);
        //peer not advertised, not return
        let mut connected_peers1 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 18)));
        connected_peers1.advertised = false;
        peers.insert(connected_peers1.ip.clone(), connected_peers1);
        //peer Ok, return
        let mut connected_peers2 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 13)));
        connected_peers2.last_alive = Some(UTime::now().unwrap().checked_sub(800.into()).unwrap());
        connected_peers2.last_failure =
            Some(UTime::now().unwrap().checked_sub(1000.into()).unwrap());
        peers.insert(connected_peers2.ip.clone(), connected_peers2);
        //peer Ok, connected return
        let mut connected_peers1 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 17)));
        connected_peers1.active_out_connections = 1;
        connected_peers1.last_alive = Some(UTime::now().unwrap().checked_sub(900.into()).unwrap());
        peers.insert(connected_peers1.ip.clone(), connected_peers1);
        //peer failure before alive but to early. return
        let mut connected_peers2 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 14)));
        connected_peers2.last_alive = Some(UTime::now().unwrap().checked_sub(800.into()).unwrap());
        connected_peers2.last_failure =
            Some(UTime::now().unwrap().checked_sub(2000.into()).unwrap());
        peers.insert(connected_peers2.ip.clone(), connected_peers2);

        let wakeup_interval = network_config.wakeup_interval;
        let (saver_watch_tx, _) = watch::channel(peers.clone());
        let saver_join_handle = tokio::spawn(async move {});

        let db = PeerInfoDatabase {
            cfg: network_config,
            peers,
            saver_join_handle,
            saver_watch_tx,
            active_out_connection_attempts: 0,
            active_out_connections: 0,
            active_in_connections: 0,
            wakeup_interval,
        };

        //test with no peers.
        let ip_list = db.get_advertisable_peer_ips();

        assert_eq!(5, ip_list.len());

        assert_eq!(
            IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
            ip_list[0]
        );
        assert_eq!(
            IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 14)),
            ip_list[1]
        );
        assert_eq!(
            IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 13)),
            ip_list[2]
        );
        assert_eq!(
            IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 17)),
            ip_list[3]
        );
        assert_eq!(
            IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)),
            ip_list[4]
        );
    }

    #[tokio::test]
    async fn test_get_out_connection_candidate_ips() {
        let network_config = example_network_config();
        let mut peers: HashMap<IpAddr, PeerInfo> = HashMap::new();

        //add peers
        //peer Ok, return
        let connected_peers1 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)));
        peers.insert(connected_peers1.ip.clone(), connected_peers1);
        //peer failure to early. not return
        let mut connected_peers2 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 12)));
        connected_peers2.last_failure =
            Some(UTime::now().unwrap().checked_sub(900.into()).unwrap());
        peers.insert(connected_peers2.ip.clone(), connected_peers2);
        //peer failure before alive but to early. return
        let mut connected_peers2 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 13)));
        connected_peers2.last_alive = Some(UTime::now().unwrap().checked_sub(900.into()).unwrap());
        connected_peers2.last_failure =
            Some(UTime::now().unwrap().checked_sub(1000.into()).unwrap());
        peers.insert(connected_peers2.ip.clone(), connected_peers2);
        //peer alive no failure. return
        let mut connected_peers1 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 14)));
        connected_peers1.last_alive = Some(UTime::now().unwrap().checked_sub(1000.into()).unwrap());
        peers.insert(connected_peers1.ip.clone(), connected_peers1);
        //peer banned not return.
        let mut banned_host1 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 23)));
        banned_host1.bootstrap = true;
        banned_host1.banned = true;
        banned_host1.last_alive = Some(UTime::now().unwrap().checked_sub(1000.into()).unwrap());
        peers.insert(banned_host1.ip.clone(), banned_host1);
        //peer failure after alive not to early. return
        let mut connected_peers2 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 15)));
        connected_peers2.last_alive =
            Some(UTime::now().unwrap().checked_sub(12000.into()).unwrap());
        connected_peers2.last_failure =
            Some(UTime::now().unwrap().checked_sub(11000.into()).unwrap());
        peers.insert(connected_peers2.ip.clone(), connected_peers2);
        //peer failure after alive to early. not return
        let mut connected_peers2 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 16)));
        connected_peers2.last_alive = Some(UTime::now().unwrap().checked_sub(2000.into()).unwrap());
        connected_peers2.last_failure =
            Some(UTime::now().unwrap().checked_sub(1000.into()).unwrap());
        peers.insert(connected_peers2.ip.clone(), connected_peers2);
        //peer Ok, connected, not return
        let mut connected_peers1 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 17)));
        connected_peers1.active_out_connections = 1;
        peers.insert(connected_peers1.ip.clone(), connected_peers1);
        //peer Ok, not advertised, not return
        let mut connected_peers1 =
            default_peer_info_not_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 18)));
        connected_peers1.advertised = false;
        peers.insert(connected_peers1.ip.clone(), connected_peers1);

        let wakeup_interval = network_config.wakeup_interval;
        let (saver_watch_tx, _) = watch::channel(peers.clone());
        let saver_join_handle = tokio::spawn(async move {});

        let db = PeerInfoDatabase {
            cfg: network_config,
            peers,
            saver_join_handle,
            saver_watch_tx,
            active_out_connection_attempts: 0,
            active_out_connections: 0,
            active_in_connections: 0,
            wakeup_interval,
        };

        //test with no peers.
        let ip_list = db.get_out_connection_candidate_ips().unwrap();
        assert_eq!(4, ip_list.len());

        assert_eq!(
            IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 14)),
            ip_list[0]
        );
        assert_eq!(
            IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)),
            ip_list[1]
        );
        assert_eq!(
            IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 15)),
            ip_list[2]
        );
        assert_eq!(
            IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 13)),
            ip_list[3]
        );
    }
    fn default_peer_info_not_connected(ip: IpAddr) -> PeerInfo {
        PeerInfo {
            ip,
            banned: false,
            bootstrap: true,
            last_alive: None,
            last_failure: None,
            advertised: true,
            active_out_connection_attempts: 0,
            active_out_connections: 0,
            active_in_connections: 0,
        }
    }

    #[tokio::test]
    async fn test_cleanup_peers() {
        let mut network_config = example_network_config();
        network_config.max_banned_peers = 1;
        network_config.max_idle_peers = 1;
        let mut peers = HashMap::new();

        //Call with empty db.
        cleanup_peers(&network_config, &mut peers, None);
        assert!(peers.is_empty());

        let mut connected_peers1 =
            default_peer_info_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)));
        connected_peers1.last_alive = Some(UTime::now().unwrap().checked_sub(1000.into()).unwrap());
        peers.insert(connected_peers1.ip.clone(), connected_peers1);

        let mut connected_peers2 =
            default_peer_info_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 12)));
        connected_peers2.last_alive = Some(UTime::now().unwrap().checked_sub(900.into()).unwrap());
        let same_connected_peer = connected_peers2.clone();

        let non_global =
            default_peer_info_connected(IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 0, 10)));
        let same_host =
            default_peer_info_connected(IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)));

        let mut banned_host1 =
            default_peer_info_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 23)));
        banned_host1.bootstrap = false;
        banned_host1.banned = true;
        banned_host1.active_out_connections = 0;
        banned_host1.last_alive = Some(UTime::now().unwrap().checked_sub(1000.into()).unwrap());
        let mut banned_host2 =
            default_peer_info_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 24)));
        banned_host2.bootstrap = false;
        banned_host2.banned = true;
        banned_host2.active_out_connections = 0;
        banned_host2.last_alive = Some(UTime::now().unwrap().checked_sub(900.into()).unwrap());
        let mut banned_host3 =
            default_peer_info_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 25)));
        banned_host3.bootstrap = false;
        banned_host3.banned = true;
        banned_host3.last_alive = Some(UTime::now().unwrap().checked_sub(900.into()).unwrap());

        let mut advertised_host1 =
            default_peer_info_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 35)));
        advertised_host1.bootstrap = false;
        advertised_host1.advertised = true;
        advertised_host1.active_out_connections = 0;
        advertised_host1.last_alive = Some(UTime::now().unwrap().checked_sub(1000.into()).unwrap());
        let mut advertised_host2 =
            default_peer_info_connected(IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 36)));
        advertised_host2.bootstrap = false;
        advertised_host2.advertised = true;
        advertised_host2.active_out_connections = 0;
        advertised_host2.last_alive = Some(UTime::now().unwrap().checked_sub(900.into()).unwrap());

        peers.insert(advertised_host1.ip.clone(), advertised_host1);
        peers.insert(banned_host1.ip.clone(), banned_host1);
        peers.insert(non_global.ip.clone(), non_global);
        peers.insert(same_connected_peer.ip.clone(), same_connected_peer);
        peers.insert(connected_peers2.ip.clone(), connected_peers2);
        peers.insert(connected_peers1.ip.clone(), connected_peers1);
        peers.insert(advertised_host2.ip.clone(), advertised_host2);
        peers.insert(same_host.ip.clone(), same_host);
        peers.insert(banned_host3.ip.clone(), banned_host3);
        peers.insert(banned_host2.ip.clone(), banned_host2);

        cleanup_peers(&network_config, &mut peers, None);

        assert!(peers.contains_key(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11))));
        assert!(peers.contains_key(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 12))));

        assert!(peers.contains_key(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 23))));
        assert!(!peers.contains_key(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 24))));
        assert!(peers.contains_key(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 25))));

        assert!(!peers.contains_key(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 35))));
        assert!(peers.contains_key(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 36))));

        //test with adversized peers
        let adversized = vec![
            IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 0, 10)),
            IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 43)),
            IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 11)),
            IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 44)),
            IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)),
        ];

        network_config.max_advertise_length = 1;
        network_config.max_idle_peers = 5;

        cleanup_peers(&network_config, &mut peers, Some(&adversized));

        assert!(peers.contains_key(&IpAddr::V4(std::net::Ipv4Addr::new(169, 202, 0, 43))));
    }

    #[tokio::test]
    async fn test() {
        let peer_db = peer_database_example(5);
        let p = peer_db.peers.values().next().unwrap();
        assert_eq!(p.is_active(), false);
    }
    fn default_peer_info_connected(ip: IpAddr) -> PeerInfo {
        PeerInfo {
            ip,
            banned: false,
            bootstrap: true,
            last_alive: None,
            last_failure: None,
            advertised: false,
            active_out_connection_attempts: 0,
            active_out_connections: 1,
            active_in_connections: 0,
        }
    }

    fn example_network_config() -> NetworkConfig {
        use std::net::{Ipv4Addr, SocketAddr};

        NetworkConfig {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080),
            routable_ip: Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))),
            protocol_port: 0,
            connect_timeout: UTime::from(180_000),
            wakeup_interval: UTime::from(10_000),
            peers_file: std::path::PathBuf::new(),
            target_out_connections: 10,
            max_in_connections: 5,
            max_in_connections_per_ip: 2,
            max_out_connnection_attempts: 15,
            max_idle_peers: 3,
            max_banned_peers: 3,
            max_advertise_length: 5,
            peers_file_dump_interval: UTime::from(10_000),
        }
    }

    fn peer_database_example(peers_number: u32) -> PeerInfoDatabase {
        use rand::Rng;

        let mut rng = rand::thread_rng();

        let mut peers: HashMap<IpAddr, PeerInfo> = HashMap::new();
        for i in 0..peers_number {
            let ip: [u8; 4] = [rng.gen(), rng.gen(), rng.gen(), rng.gen()];
            let peer = PeerInfo {
                ip: IpAddr::from(ip),
                banned: (ip[0] % 5) == 0,
                bootstrap: (ip[1] % 2) == 0,
                last_alive: match i % 4 {
                    0 => None,
                    _ => Some(UTime::now().unwrap().checked_sub(50000.into()).unwrap()),
                },
                last_failure: match i % 5 {
                    0 => None,
                    _ => Some(UTime::now().unwrap().checked_sub(60000.into()).unwrap()),
                },
                advertised: (ip[2] % 2) == 0,
                active_out_connection_attempts: 0,
                active_out_connections: 0,
                active_in_connections: 0,
            };
            peers.insert(peer.ip, peer);
        }
        let cfg = example_network_config();
        let wakeup_interval = cfg.wakeup_interval;

        let (saver_watch_tx, _) = watch::channel(peers.clone());
        let saver_join_handle = tokio::spawn(async move {});
        PeerInfoDatabase {
            cfg,
            peers,
            saver_join_handle,
            saver_watch_tx,
            active_out_connection_attempts: 0,
            active_out_connections: 0,
            active_in_connections: 0,
            wakeup_interval,
        }
    }
}