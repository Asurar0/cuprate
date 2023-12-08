use std::{
    collections::{HashMap, HashSet},
    future::Future,
    panic,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use futures::{stream::FuturesUnordered, FutureExt, StreamExt};
use pin_project::pin_project;
use tokio::{
    task::JoinHandle,
    time::{interval, sleep, Interval, MissedTickBehavior, Sleep},
};
use tower::Service;

use cuprate_common::{tower_utils::InstaFuture, PruningSeed};
use monero_p2p::{
    client::InternalPeerID,
    handles::ConnectionHandle,
    services::{AddressBookRequest, AddressBookResponse, ZoneSpecificPeerListEntryBase},
    NetZoneAddress, NetworkZone,
};

use crate::{peer_list::PeerList, store::save_peers_to_disk, AddressBookError, Config};

#[cfg(test)]
mod tests;

/// An entry in the connected list.
pub struct ConnectionPeerEntry<Z: NetworkZone> {
    addr: Option<Z::Addr>,
    id: u64,
    handle: ConnectionHandle,
    /// The peers pruning seed
    pruning_seed: PruningSeed,
    /// The peers port.
    rpc_port: u16,
    /// The peers rpc credits per hash
    rpc_credits_per_hash: u32,
}

/// A future that resolves when a peer is unbanned.
#[pin_project(project = EnumProj)]
pub struct BanedPeerFut<Addr: NetZoneAddress>(Addr::BanID, #[pin] Sleep);

impl<Addr: NetZoneAddress> Future for BanedPeerFut<Addr> {
    type Output = Addr::BanID;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        match this.1.poll_unpin(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(_) => Poll::Ready(*this.0),
        }
    }
}

pub struct AddressBook<Z: NetworkZone> {
    /// Our white peers - the peers we have previously connected to.
    white_list: PeerList<Z>,
    /// Our gray peers - the peers we have been told about but haven't connected to.
    gray_list: PeerList<Z>,
    /// Our anchor peers - on start up will contain a list of peers we were connected to before shutting down
    /// after that will contain a list of peers currently connected to that we can reach.
    anchor_list: HashSet<Z::Addr>,
    /// The currently connected peers.
    connected_peers: HashMap<InternalPeerID<Z::Addr>, ConnectionPeerEntry<Z>>,
    connected_peers_ban_id: HashMap<<Z::Addr as NetZoneAddress>::BanID, HashSet<Z::Addr>>,
    /// The currently banned peers
    banned_peers: HashSet<<Z::Addr as NetZoneAddress>::BanID>,

    banned_peers_fut: FuturesUnordered<BanedPeerFut<Z::Addr>>,

    peer_save_task_handle: Option<JoinHandle<std::io::Result<()>>>,
    peer_save_interval: Interval,

    cfg: Config,
}

impl<Z: NetworkZone> AddressBook<Z> {
    pub fn new(
        cfg: Config,
        white_peers: Vec<ZoneSpecificPeerListEntryBase<Z::Addr>>,
        gray_peers: Vec<ZoneSpecificPeerListEntryBase<Z::Addr>>,
        anchor_peers: Vec<Z::Addr>,
    ) -> Self {
        let white_list = PeerList::new(white_peers);
        let gray_list = PeerList::new(gray_peers);
        let anchor_list = HashSet::from_iter(anchor_peers);

        // TODO: persist banned peers
        let banned_peers = HashSet::new();
        let banned_peers_fut = FuturesUnordered::new();
        let connected_peers = HashMap::new();

        let mut peer_save_interval = interval(cfg.peer_save_period);
        peer_save_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        Self {
            white_list,
            gray_list,
            anchor_list,
            connected_peers,
            connected_peers_ban_id: HashMap::new(),
            banned_peers,
            banned_peers_fut,
            peer_save_task_handle: None,
            peer_save_interval,
            cfg,
        }
    }

    fn poll_save_to_disk(&mut self, cx: &mut Context<'_>) {
        if let Some(handle) = &mut self.peer_save_task_handle {
            // if we have already spawned a task to save the peer list wait for that to complete.
            match handle.poll_unpin(cx) {
                Poll::Pending => return,
                Poll::Ready(Ok(Err(e))) => {
                    tracing::error!("Could not save peer list to disk, got error: {}", e)
                }
                Poll::Ready(Err(e)) => {
                    if e.is_panic() {
                        panic::resume_unwind(e.into_panic())
                    }
                }
                _ => (),
            }
        }
        // the task is finished.
        self.peer_save_task_handle = None;

        let Poll::Ready(_) = self.peer_save_interval.poll_tick(cx) else {
            return;
        };

        self.peer_save_task_handle = Some(save_peers_to_disk(
            &self.cfg,
            &self.white_list,
            &self.gray_list,
        ));
    }

    fn poll_unban_peers(&mut self, cx: &mut Context<'_>) {
        while let Poll::Ready(Some(ban_id)) = self.banned_peers_fut.poll_next_unpin(cx) {
            self.banned_peers.remove(&ban_id);
        }
    }

    fn poll_connected_peers(&mut self) {
        let mut internal_addr_disconnected = Vec::new();
        let mut addrs_to_ban = Vec::new();

        for (internal_addr, peer) in &mut self.connected_peers {
            if let Some(time) = peer.handle.check_should_ban() {
                match internal_addr {
                    InternalPeerID::KnownAddr(addr) => addrs_to_ban.push((*addr, time.0)),
                    // If we don't know the peers address all we can do is disconnect.
                    InternalPeerID::Unknown(_) => peer.handle.send_close_signal(),
                }
            }

            if peer.handle.is_closed() {
                internal_addr_disconnected.push(*internal_addr);
            }
        }

        for (addr, time) in addrs_to_ban.into_iter() {
            self.ban_peer(addr, time);
        }

        for disconnected_addr in internal_addr_disconnected {
            self.connected_peers.remove(&disconnected_addr);
            if let InternalPeerID::KnownAddr(addr) = disconnected_addr {
                // remove the peer from the connected peers with this ban ID.
                self.connected_peers_ban_id
                    .get_mut(&addr.ban_id())
                    .unwrap()
                    .remove(&addr);

                // If the amount of peers with this ban id is 0 remove the whole set.
                if self
                    .connected_peers_ban_id
                    .get(&addr.ban_id())
                    .unwrap()
                    .is_empty()
                {
                    self.connected_peers_ban_id.remove(&addr.ban_id());
                }
                // remove the peer from the anchor list.
                self.anchor_list.remove(&addr);
            }
        }
    }

    fn ban_peer(&mut self, addr: Z::Addr, time: Duration) {
        if self.banned_peers.contains(&addr.ban_id()) {
            return;
        }

        if let Some(connected_peers_with_ban_id) = self.connected_peers_ban_id.get(&addr.ban_id()) {
            for peer in connected_peers_with_ban_id.iter().map(|addr| {
                tracing::debug!("Banning peer: {}, for: {:?}", addr, time);

                self.connected_peers
                    .get(&InternalPeerID::KnownAddr(*addr))
                    .expect("Peer must be in connected list if in connected_peers_with_ban_id")
            }) {
                // The peer will get removed from our connected list once we disconnect
                peer.handle.send_close_signal();
                // Remove the peer now from anchors so we don't accidentally persist a bad anchor peer to disk.
                self.anchor_list.remove(&addr);
            }
        }

        self.white_list.remove_peers_with_ban_id(&addr.ban_id());
        self.gray_list.remove_peers_with_ban_id(&addr.ban_id());

        self.banned_peers.insert(addr.ban_id());
        self.banned_peers_fut
            .push(BanedPeerFut(addr.ban_id(), sleep(time)))
    }

    /// adds a peer to the gray list.
    fn add_peer_to_gray_list(&mut self, mut peer: ZoneSpecificPeerListEntryBase<Z::Addr>) {
        if self.white_list.contains_peer(&peer.adr) {
            return;
        };
        if !self.gray_list.contains_peer(&peer.adr) {
            peer.last_seen = 0;
            self.gray_list.add_new_peer(peer);
        }
    }

    /// Checks if a peer is banned.
    fn is_peer_banned(&self, peer: &Z::Addr) -> bool {
        self.banned_peers.contains(&peer.ban_id())
    }

    fn handle_incoming_peer_list(
        &mut self,
        mut peer_list: Vec<ZoneSpecificPeerListEntryBase<Z::Addr>>,
    ) {
        tracing::debug!("Received new peer list, length: {}", peer_list.len());

        peer_list.retain(|peer| {
            if !peer.adr.should_add_to_peer_list() {
                false
            } else {
                !self.is_peer_banned(&peer.adr)
            }
            // TODO: check rpc/ p2p ports not the same
        });

        for peer in peer_list {
            self.add_peer_to_gray_list(peer);
        }
        // The gray list has no peers we need to keep in the list so just pass an empty HashSet.
        self.gray_list
            .reduce_list(&HashSet::new(), self.cfg.max_gray_list_length);
    }

    fn get_random_white_peer(
        &self,
        block_needed: Option<u64>,
    ) -> Option<ZoneSpecificPeerListEntryBase<Z::Addr>> {
        tracing::debug!("Retrieving random white peer");
        self.white_list
            .get_random_peer(&mut rand::thread_rng(), block_needed)
            .copied()
    }

    fn get_random_gray_peer(
        &self,
        block_needed: Option<u64>,
    ) -> Option<ZoneSpecificPeerListEntryBase<Z::Addr>> {
        tracing::debug!("Retrieving random gray peer");
        self.gray_list
            .get_random_peer(&mut rand::thread_rng(), block_needed)
            .copied()
    }

    fn get_white_peers(&self, len: usize) -> Vec<ZoneSpecificPeerListEntryBase<Z::Addr>> {
        tracing::debug!("Retrieving white peers, maximum: {}", len);
        self.white_list
            .get_random_peers(&mut rand::thread_rng(), len)
    }

    /// Updates an entry in the white list, if the peer is not found then
    /// the peer will be added to the white list.
    fn update_white_list_peer_entry(
        &mut self,
        peer: &ConnectionPeerEntry<Z>,
    ) -> Result<(), AddressBookError> {
        let Some(addr) = &peer.addr else {
            // If the peer isn't reachable we shouldn't add it too our address book.
            return Ok(());
        };

        if let Some(peb) = self.white_list.get_peer_mut(addr) {
            if peb.pruning_seed != peer.pruning_seed {
                return Err(AddressBookError::PeersDataChanged("Pruning seed"));
            }
            if Z::CHECK_NODE_ID && peb.id != peer.id {
                return Err(AddressBookError::PeersDataChanged("peer ID"));
            }
            // TODO: cuprate doesn't need last seen timestamps but should we have them anyway?
            peb.last_seen = 0;
            peb.rpc_port = peer.rpc_port;
            peb.rpc_credits_per_hash = peer.rpc_credits_per_hash;
        } else {
            // if the peer is reachable add it to our white list
            let peb = ZoneSpecificPeerListEntryBase {
                id: peer.id,
                adr: *addr,
                last_seen: 0,
                rpc_port: peer.rpc_port,
                rpc_credits_per_hash: peer.rpc_credits_per_hash,
                pruning_seed: peer.pruning_seed,
            };
            self.white_list.add_new_peer(peb);
        }
        Ok(())
    }

    fn handle_new_connection(
        &mut self,
        internal_peer_id: InternalPeerID<Z::Addr>,
        peer: ConnectionPeerEntry<Z>,
    ) -> Result<(), AddressBookError> {
        if self.connected_peers.contains_key(&internal_peer_id) {
            return Err(AddressBookError::PeerAlreadyConnected);
        }

        // If we know the address then check if it's banned.
        if let InternalPeerID::KnownAddr(addr) = &internal_peer_id {
            if self.is_peer_banned(addr) {
                return Err(AddressBookError::PeerIsBanned);
            }
            // although the peer may not be readable still add it to the connected peers with ban ID.
            self.connected_peers_ban_id
                .entry(addr.ban_id())
                .or_default()
                .insert(*addr);
        }

        // if the address is Some that means we can reach it from our node.
        if let Some(addr) = peer.addr {
            // remove the peer from the gray list as we know it's active.
            self.gray_list.remove_peer(&addr);
            // The peer is reachable, update our white list and add it to the anchor connections.
            self.update_white_list_peer_entry(&peer)?;
            self.white_list
                .reduce_list(&self.anchor_list, self.cfg.max_white_list_length);
            self.anchor_list.insert(addr);
        }

        self.connected_peers.insert(internal_peer_id, peer);
        Ok(())
    }
}

impl<Z: NetworkZone> Service<AddressBookRequest<Z>> for AddressBook<Z> {
    type Response = AddressBookResponse<Z>;
    type Error = AddressBookError;
    type Future = InstaFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.poll_unban_peers(cx);
        self.poll_save_to_disk(cx);
        self.poll_connected_peers();
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: AddressBookRequest<Z>) -> Self::Future {
        let span = tracing::info_span!("AddressBook");
        let _guard = span.enter();

        let response = match req {
            AddressBookRequest::NewConnection {
                addr,
                internal_peer_id,
                handle,
                id,
                pruning_seed,
                rpc_port,
                rpc_credits_per_hash,
            } => self
                .handle_new_connection(
                    internal_peer_id,
                    ConnectionPeerEntry {
                        addr,
                        id,
                        handle,
                        pruning_seed,
                        rpc_port,
                        rpc_credits_per_hash,
                    },
                )
                .map(|_| AddressBookResponse::Ok),
            AddressBookRequest::BanPeer(addr, time) => {
                self.ban_peer(addr, time);
                Ok(AddressBookResponse::Ok)
            }
            AddressBookRequest::IncomingPeerList(peer_list) => {
                self.handle_incoming_peer_list(peer_list);
                Ok(AddressBookResponse::Ok)
            }
            AddressBookRequest::GetRandomWhitePeer { height } => self
                .get_random_white_peer(height)
                .map(AddressBookResponse::Peer)
                .ok_or(AddressBookError::PeerNotFound),
            AddressBookRequest::GetRandomGrayPeer { height } => self
                .get_random_gray_peer(height)
                .map(AddressBookResponse::Peer)
                .ok_or(AddressBookError::PeerNotFound),
            AddressBookRequest::GetWhitePeers(len) => {
                Ok(AddressBookResponse::Peers(self.get_white_peers(len)))
            }
        };

        InstaFuture::from(response)
    }
}
