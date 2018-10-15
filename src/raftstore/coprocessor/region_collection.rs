// Copyright 2018 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::BTreeMap;
use std::collections::Bound::{Excluded, Unbounded};
use std::fmt::{Display, Formatter, Result as FmtResult};
use std::sync::{mpsc, Arc, Mutex};
use std::usize;

use super::{
    Coprocessor, CoprocessorHost, ObserverContext, RegionChangeEvent, RegionChangeObserver,
    RoleObserver,
};
use kvproto::metapb::Region;
use raft::StateRole;
use raftstore::store::keys::{data_end_key, data_key, origin_key, DATA_MAX_KEY};
use raftstore::store::msg::{SeekRegionCallback, SeekRegionFilter, SeekRegionResult};
use storage::engine::{RegionInfoProvider, Result as EngineResult};
use util::collections::HashMap;
use util::escape;
use util::worker::{Builder as WorkerBuilder, Runnable, Scheduler, Worker};

const CHANNEL_BUFFER_SIZE: usize = usize::MAX; // Unbounded

/// `RegionCollection` is used to collect all regions on this TiKV into a collection so that other
/// parts of TiKV can get region information from it. It registers a observer to raftstore, which
/// is named `EventSender`, and it simply send some specific types of events through a channel.
/// In the mean time, `RegionCollectionWorker` keeps fetching messages from the channel, and mutate
/// the collection according tho the messages. When an accessor method of `RegionCollection` is
/// called, it also simply send a message to `RegionCollectionWorker`, and the result will be send
/// back through as soon as it's finished.
/// In fact, the channel mentioned above is actually a `util::worker::Worker`.

/// `RaftStoreEvent` Represents events dispatched from raftstore coprocessor.
#[derive(Debug)]
enum RaftStoreEvent {
    CreateRegion { region: Region },
    UpdateRegion { region: Region },
    DestroyRegion { region: Region },
    RoleChange { region: Region, role: StateRole },
}

#[derive(Clone, Debug)]
pub struct RegionInfo {
    pub region: Region,
    pub role: StateRole,
    pub outdated: bool,
}

impl RegionInfo {
    pub fn new(region: Region, role: StateRole, outdated: bool) -> Self {
        Self {
            region,
            role,
            outdated,
        }
    }
}

type RegionsMap = HashMap<u64, RegionInfo>;
type RegionRangesMap = BTreeMap<Vec<u8>, u64>;

/// `RegionCollection` has its own thread (namely RegionCollectionWorker). Queries and updates are
/// done by sending commands to the thread.
enum RegionCollectionMsg {
    RaftStoreEvent(RaftStoreEvent),
    SeekRegion {
        from: Vec<u8>,
        filter: SeekRegionFilter,
        limit: u32,
        callback: SeekRegionCallback,
    },
    /// Get all contents from the collection. Only used for testing.
    DebugDump(mpsc::Sender<(RegionsMap, RegionRangesMap)>),
}

impl Display for RegionCollectionMsg {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        match self {
            RegionCollectionMsg::RaftStoreEvent(e) => write!(f, "RaftStoreEvent({:?})", e),
            RegionCollectionMsg::SeekRegion { from, limit, .. } => {
                write!(f, "SeekRegion(from: {}, limit: {})", escape(from), limit)
            }
            RegionCollectionMsg::DebugDump(_) => write!(f, "DebugDump"),
        }
    }
}

/// `EventSender` implements observer traits. It simply send the events that we are interested in
/// through the `scheduler`.
#[derive(Clone)]
struct EventSender {
    scheduler: Scheduler<RegionCollectionMsg>,
}

impl Coprocessor for EventSender {}

impl RegionChangeObserver for EventSender {
    fn on_region_changed(&self, context: &mut ObserverContext, event: RegionChangeEvent) {
        let region = context.region().clone();
        let event = match event {
            RegionChangeEvent::Create => RaftStoreEvent::CreateRegion { region },
            RegionChangeEvent::Update => RaftStoreEvent::UpdateRegion { region },
            RegionChangeEvent::Destroy => RaftStoreEvent::DestroyRegion { region },
        };
        self.scheduler
            .schedule(RegionCollectionMsg::RaftStoreEvent(event))
            .unwrap();
    }
}

impl RoleObserver for EventSender {
    fn on_role_change(&self, context: &mut ObserverContext, role: StateRole) {
        let region = context.region().clone();
        let event = RaftStoreEvent::RoleChange { region, role };
        self.scheduler
            .schedule(RegionCollectionMsg::RaftStoreEvent(event))
            .unwrap();
    }
}

/// Create an `EventSender` and register it to given coprocessor host.
fn register_raftstore_event_sender(
    host: &mut CoprocessorHost,
    scheduler: Scheduler<RegionCollectionMsg>,
) {
    let event_sender = EventSender { scheduler };

    host.registry
        .register_role_observer(1, box event_sender.clone());
    host.registry
        .register_region_change_observer(1, box event_sender.clone());
}

/// `RegionCollectionWorker` is the underlying runner of `RegionCollection`. It listens on events
/// sent by the `EventSender` and maintains the collection of all regions. Role of each region
/// are also tracked.
struct RegionCollectionWorker {
    // region_id -> (Region, State)
    regions: HashMap<u64, RegionInfo>,
    // 'z' + end_key -> region_id
    region_ranges: BTreeMap<Vec<u8>, u64>,
}

impl RegionCollectionWorker {
    fn new() -> Self {
        Self {
            regions: HashMap::default(),
            region_ranges: BTreeMap::default(),
        }
    }

    fn handle_create_region(&mut self, region: Region) {
        if self.regions.get(&region.get_id()).is_some() {
            warn!(
                "region_collection: trying to create new region {} but it already exists. \
                 try to update it.",
                region.get_id(),
            );
            self.handle_update_region(region);
            return;
        }

        self.region_ranges
            .insert(data_end_key(region.get_end_key()), region.get_id());
        // TODO: Should we set it follower?
        self.regions.insert(
            region.get_id(),
            RegionInfo::new(region, StateRole::Follower, false),
        );
    }

    fn handle_update_region(&mut self, region: Region) {
        let mut is_new_region = true;
        if let Some(ref mut old_region_info) = self.regions.get_mut(&region.get_id()) {
            let old_region = &mut old_region_info.region;
            is_new_region = false;
            assert_eq!(old_region.get_id(), region.get_id());

            // If the end_key changed, the old item in `region_ranges` should be removed.
            // However it shouldn't be removed if it was already updated by another region. In this
            // case, let `old_end_key = old_region.get_end_key`, then
            // `self.region_ranges[old_end_key]` should be another region's id.
            if old_region.get_end_key() != region.get_end_key() {
                // The region's end_key has changed.
                // Remove the old entry in `self.region_ranges` if it haven't been updated by
                // other items in `regions`.
                let old_end_key = data_end_key(old_region.get_end_key());
                if let Some(old_id) = self.region_ranges.get(&old_end_key).cloned() {
                    // If they are not equal, we shouldn't remove it because it was updated by
                    // another region.
                    if old_id == region.get_id() {
                        self.region_ranges.remove(&old_end_key);
                    }
                }
            }

            // If the region already exists, update it and keep the original role.
            *old_region = region.clone();
        }

        if is_new_region {
            warn!(
                "region_collection: trying to update region {} but it doesn't exist.",
                region.get_id()
            );
            // If it's a new region, set it to follower state.
            // TODO: Should we set it follower?
            self.regions.insert(
                region.get_id(),
                RegionInfo::new(region.clone(), StateRole::Follower, false),
            );
        }

        // If the end_key changed or the region didn't exist previously, insert a new item;
        // otherwise, update the old item. All regions in param `regions` must have unique
        // end_keys, so it won't conflict with each other.
        self.region_ranges
            .insert(data_end_key(region.get_end_key()), region.get_id());
    }

    fn handle_destroy_region(&mut self, region: Region) {
        if let Some(removed_region_info) = self.regions.remove(&region.get_id()) {
            let removed_region = removed_region_info.region;
            assert_eq!(removed_region.get_id(), region.get_id());
            let end_key = data_end_key(removed_region.get_end_key());

            // The entry may be updated by other regions.
            if let Some(id) = self.region_ranges.get(&end_key).cloned() {
                if id == region.get_id() {
                    self.region_ranges.remove(&end_key);
                }
            }
        } else {
            warn!(
                "region_collection: destroying region {} but it doesn't exist",
                region.get_id()
            )
        }
    }

    fn handle_role_change(&mut self, region: Region, new_role: StateRole) {
        let region_id = region.get_id();
        if self.regions.get(&region_id).is_none() {
            warn!("region_collection: role change on region {} but the region doesn't exist. create it.", region_id);
            self.handle_create_region(region);
        }

        let role = &mut self.regions.get_mut(&region_id).unwrap().role;
        *role = new_role;
    }

    fn handle_seek_region(
        &self,
        from_key: Vec<u8>,
        filter: SeekRegionFilter,
        mut limit: u32,
        callback: SeekRegionCallback,
    ) {
        assert!(limit > 0);

        let from_key = data_key(&from_key);
        for (end_key, region_id) in self.region_ranges.range((Excluded(from_key), Unbounded)) {
            let RegionInfo {
                region,
                role,
                outdated,
            } = &self.regions[region_id];
            if !outdated && filter(region, *role) {
                callback(SeekRegionResult::Found(region.clone()));
                return;
            }

            limit -= 1;
            if limit == 0 {
                // `origin_key` does not handle `DATA_MAX_KEY`, but we can return `Ended` rather
                // than `LimitExceeded`.
                if end_key.as_slice() >= DATA_MAX_KEY {
                    break;
                }

                callback(SeekRegionResult::LimitExceeded {
                    next_key: origin_key(end_key).to_vec(),
                });
                return;
            }
        }
        callback(SeekRegionResult::Ended);
    }

    fn handle_raftstore_event(&mut self, event: RaftStoreEvent) {
        match event {
            RaftStoreEvent::CreateRegion { region } => {
                self.handle_create_region(region);
            }
            RaftStoreEvent::UpdateRegion { region } => {
                self.handle_update_region(region);
            }
            RaftStoreEvent::DestroyRegion { region } => {
                self.handle_destroy_region(region);
            }
            RaftStoreEvent::RoleChange { region, role } => {
                self.handle_role_change(region, role);
            }
        }
    }
}

impl Runnable<RegionCollectionMsg> for RegionCollectionWorker {
    fn run(&mut self, task: RegionCollectionMsg) {
        match task {
            RegionCollectionMsg::RaftStoreEvent(event) => {
                self.handle_raftstore_event(event);
            }
            RegionCollectionMsg::SeekRegion {
                from,
                filter,
                limit,
                callback,
            } => {
                self.handle_seek_region(from, filter, limit, callback);
            }
            RegionCollectionMsg::DebugDump(tx) => {
                tx.send((self.regions.clone(), self.region_ranges.clone()))
                    .unwrap();
            }
        }
    }
}

/// `RegionCollection` keeps all region information separately from raftstore itself.
#[derive(Clone)]
pub struct RegionCollection {
    worker: Arc<Mutex<Worker<RegionCollectionMsg>>>,
    scheduler: Scheduler<RegionCollectionMsg>,
}

impl RegionCollection {
    /// Create a new `RegionCollection` and register to `host`.
    /// `RegionCollection` doesn't need, and should not be created more than once. If it's needed
    /// in different places, just clone it, and their contents are shared.
    pub fn new(host: &mut CoprocessorHost) -> Self {
        let worker = WorkerBuilder::new("region-collection-worker")
            .pending_capacity(CHANNEL_BUFFER_SIZE)
            .create();
        let scheduler = worker.scheduler();

        register_raftstore_event_sender(host, scheduler.clone());

        Self {
            worker: Arc::new(Mutex::new(worker)),
            scheduler,
        }
    }

    /// Start the `RegionCollection`. It should be started before raftstore.
    pub fn start(&self) {
        self.worker
            .lock()
            .unwrap()
            .start(RegionCollectionWorker::new())
            .unwrap();
    }

    /// Stop the `RegionCollection`. It should be stopped after raftstore.
    pub fn stop(&self) {
        self.worker.lock().unwrap().stop().unwrap().join().unwrap();
    }

    /// Get all content from the collection. Only used for testing.
    pub fn debug_dump(&self) -> (RegionsMap, RegionRangesMap) {
        let (tx, rx) = mpsc::channel();
        self.scheduler
            .schedule(RegionCollectionMsg::DebugDump(tx))
            .unwrap();
        rx.recv().unwrap()
    }
}

impl RegionInfoProvider for RegionCollection {
    fn seek_region(
        &self,
        from: &[u8],
        filter: SeekRegionFilter,
        limit: u32,
    ) -> EngineResult<SeekRegionResult> {
        let (tx, rx) = mpsc::channel();
        let msg = RegionCollectionMsg::SeekRegion {
            from: from.to_vec(),
            filter,
            limit,
            callback: box move |res| {
                tx.send(res).unwrap_or_else(|e| {
                    panic!(
                        "region collection failed to send result back to caller: {:?}",
                        e
                    )
                })
            },
        };
        self.scheduler
            .schedule(msg)
            .map_err(|e| box_err!("failed to send request to region collection: {:?}", e))
            .and_then(|_| {
                rx.recv().map_err(|e| {
                    box_err!(
                        "failed to receive seek region result from region collection: {:?}",
                        e
                    )
                })
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_region(id: u64, start_key: &[u8], end_key: &[u8]) -> Region {
        let mut region = Region::default();
        region.set_id(id);
        region.set_start_key(start_key.to_vec());
        region.set_end_key(end_key.to_vec());
        region
    }

    fn check_collection(c: &RegionCollectionWorker, regions: &[(Region, StateRole)]) {
        let region_ranges: Vec<_> = regions
            .iter()
            .map(|(r, _)| (data_end_key(r.get_end_key()), r.get_id()))
            .collect();

        let mut is_regions_equal = c.regions.len() == regions.len();

        if is_regions_equal {
            for (expect_region, expect_role) in regions {
                is_regions_equal = is_regions_equal
                    && c.regions.get(&expect_region.get_id()).map_or(
                        false,
                        |RegionInfo {
                             region,
                             role,
                             outdated,
                         }| {
                            !*outdated && expect_region == region && expect_role == role
                        },
                    );

                if !is_regions_equal {
                    break;
                }
            }
        }
        if !is_regions_equal {
            panic!("regions: expect {:?}, but got {:?}", regions, c.regions);
        }

        let mut is_ranges_equal = c.region_ranges.len() == region_ranges.len();
        is_ranges_equal = is_ranges_equal
            && c.region_ranges.iter().zip(region_ranges.iter()).all(
                |((actual_key, actual_id), (expect_key, expect_id))| {
                    actual_key == expect_key && actual_id == expect_id
                },
            );
        if !is_ranges_equal {
            panic!(
                "region_ranges: expect {:?}, but got {:?}",
                region_ranges, c.region_ranges
            );
        }
    }

    /// Add a set of regions to an empty collection and check if it's successfully loaded.
    fn must_load_regions(c: &mut RegionCollectionWorker, regions: &[Region]) {
        assert!(c.regions.is_empty());
        assert!(c.region_ranges.is_empty());

        for region in regions {
            must_create_region(c, &region);
        }

        let expected_regions: Vec<_> = regions
            .iter()
            .map(|r| (r.clone(), StateRole::Follower))
            .collect();
        check_collection(&c, &expected_regions);
    }

    fn must_create_region(c: &mut RegionCollectionWorker, region: &Region) {
        assert!(c.regions.get(&region.get_id()).is_none());

        c.handle_create_region(region.clone());

        assert_eq!(&c.regions[&region.get_id()].region, region);
        assert_eq!(
            c.region_ranges[&data_end_key(region.get_end_key())],
            region.get_id()
        );
    }

    fn must_update_region(c: &mut RegionCollectionWorker, region: &Region) {
        assert!(c.regions.get(&region.get_id()).is_some());
        let old_end_key = c.regions[&region.get_id()].region.get_end_key().to_vec();

        c.handle_update_region(region.clone());

        assert_eq!(&c.regions[&region.get_id()].region, region);
        assert_eq!(
            c.region_ranges[&data_end_key(region.get_end_key())],
            region.get_id()
        );
        // If end_key is updated and the region_id corresponding to the `old_end_key` doesn't equals
        // to `region_id`, it shouldn't be removed since it was used by another region.
        if old_end_key.as_slice() != region.get_end_key() {
            assert!(
                c.region_ranges
                    .get(&data_end_key(&old_end_key))
                    .map_or(true, |id| *id != region.get_id())
            );
        }
    }

    fn must_destroy_region(c: &mut RegionCollectionWorker, id: u64) {
        let end_key = c.regions[&id].region.get_end_key().to_vec();

        c.handle_destroy_region(new_region(id, b"", b""));

        assert!(c.regions.get(&id).is_none());
        // If the region_id corresponding to the end_key doesn't equals to `id`, it shouldn't be
        // removed since it was used by another region.
        assert!(
            c.region_ranges
                .get(&data_end_key(&end_key))
                .map_or(true, |r| *r != id)
        );
    }

    fn must_change_role(c: &mut RegionCollectionWorker, region: &Region, role: StateRole) {
        assert!(c.regions.get(&region.get_id()).is_some());

        c.handle_role_change(region.clone(), role);

        assert_eq!(c.regions[&region.get_id()].role, role);
    }

    #[test]
    fn test_basic_updating() {
        let mut c = RegionCollectionWorker::new();
        let init_regions = &[
            new_region(1, b"", b"k1"),
            new_region(2, b"k1", b"k9"),
            new_region(3, b"k9", b""),
        ];

        must_load_regions(&mut c, init_regions);

        // end_key changed
        must_update_region(&mut c, &new_region(2, b"k2", b"k8"));
        // end_key changed (previous end_key is empty)
        must_update_region(&mut c, &new_region(3, b"k9", b"k99"));
        // end_key not changed
        must_update_region(&mut c, &new_region(1, b"k0", b"k1"));
        check_collection(
            &c,
            &[
                (new_region(1, b"k0", b"k1"), StateRole::Follower),
                (new_region(2, b"k2", b"k8"), StateRole::Follower),
                (new_region(3, b"k9", b"k99"), StateRole::Follower),
            ],
        );

        must_change_role(&mut c, &new_region(1, b"k0", b"k1"), StateRole::Candidate);
        must_create_region(&mut c, &new_region(5, b"k99", b""));
        must_change_role(&mut c, &new_region(2, b"k2", b"k8"), StateRole::Leader);
        must_update_region(&mut c, &new_region(2, b"k3", b"k7"));
        must_create_region(&mut c, &new_region(4, b"k1", b"k3"));
        check_collection(
            &c,
            &[
                (new_region(1, b"k0", b"k1"), StateRole::Candidate),
                (new_region(4, b"k1", b"k3"), StateRole::Follower),
                (new_region(2, b"k3", b"k7"), StateRole::Leader),
                (new_region(3, b"k9", b"k99"), StateRole::Follower),
                (new_region(5, b"k99", b""), StateRole::Follower),
            ],
        );

        must_destroy_region(&mut c, 4);
        must_destroy_region(&mut c, 3);
        check_collection(
            &c,
            &[
                (new_region(1, b"k0", b"k1"), StateRole::Candidate),
                (new_region(2, b"k3", b"k7"), StateRole::Leader),
                (new_region(5, b"k99", b""), StateRole::Follower),
            ],
        );
    }

    /// Simulate splitting a region into 3 regions, and the region with old id will be the
    /// `derive_index`-th region of them. The events are triggered in order indicated by `seq`.
    /// This is to ensure the collection is correct, no matter what the events' order to happen is.
    /// Values in `seq` and of `derive_index` start from 1.
    fn test_split_impl(derive_index: usize, seq: &[usize]) {
        let mut c = RegionCollectionWorker::new();
        let init_regions = &[
            new_region(1, b"", b"k1"),
            new_region(2, b"k1", b"k9"),
            new_region(3, b"k9", b""),
        ];
        must_load_regions(&mut c, init_regions);

        let mut final_regions = vec![
            new_region(1, b"", b"k1"),
            new_region(4, b"k1", b"k3"),
            new_region(5, b"k3", b"k6"),
            new_region(6, b"k6", b"k9"),
            new_region(3, b"k9", b""),
        ];
        // `derive_index` starts from 1
        final_regions[derive_index].set_id(2);

        for idx in seq {
            if *idx == derive_index {
                must_update_region(&mut c, &final_regions[*idx]);
            } else {
                must_create_region(&mut c, &final_regions[*idx]);
            }
        }

        let final_regions = final_regions
            .into_iter()
            .map(|r| (r, StateRole::Follower))
            .collect::<Vec<_>>();
        check_collection(&c, &final_regions);
    }

    #[test]
    fn test_split() {
        let indices = &[1, 2, 3];
        let orders = &[
            &[1, 2, 3],
            &[1, 3, 2],
            &[2, 1, 3],
            &[2, 3, 1],
            &[3, 1, 2],
            &[3, 2, 1],
        ];

        for index in indices {
            for order in orders {
                test_split_impl(*index, *order);
            }
        }
    }

    fn test_merge_impl(to_left: bool, update_first: bool) {
        let mut c = RegionCollectionWorker::new();
        let init_regions = &[
            new_region(1, b"", b"k1"),
            new_region(2, b"k1", b"k2"),
            new_region(3, b"k2", b"k3"),
            new_region(4, b"k3", b""),
        ];
        must_load_regions(&mut c, init_regions);

        let (mut updating_region, destroying_region_id) = if to_left {
            (init_regions[1].clone(), init_regions[2].get_id())
        } else {
            (init_regions[2].clone(), init_regions[1].get_id())
        };
        updating_region.set_start_key(b"k1".to_vec());
        updating_region.set_end_key(b"k3".to_vec());

        if update_first {
            must_update_region(&mut c, &updating_region);
            must_destroy_region(&mut c, destroying_region_id);
        } else {
            must_destroy_region(&mut c, destroying_region_id);
            must_update_region(&mut c, &updating_region);
        }

        let final_regions = &[
            (new_region(1, b"", b"k1"), StateRole::Follower),
            (updating_region, StateRole::Follower),
            (new_region(4, b"k3", b""), StateRole::Follower),
        ];
        check_collection(&c, final_regions);
    }

    #[test]
    fn test_merge() {
        test_merge_impl(false, false);
        test_merge_impl(false, true);
        test_merge_impl(true, false);
        test_merge_impl(true, true);
    }
}
