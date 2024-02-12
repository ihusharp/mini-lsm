#![allow(dead_code)] // REMOVE THIS LINE after fully implementing this functionality

mod leveled;
mod simple_leveled;
mod tiered;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Ok, Result};
pub use leveled::{LeveledCompactionController, LeveledCompactionOptions, LeveledCompactionTask};
use serde::{Deserialize, Serialize};
pub use simple_leveled::{
    SimpleLeveledCompactionController, SimpleLeveledCompactionOptions, SimpleLeveledCompactionTask,
};
pub use tiered::{TieredCompactionController, TieredCompactionOptions, TieredCompactionTask};

use crate::iterators::concat_iterator::SstConcatIterator;
use crate::iterators::merge_iterator::MergeIterator;
use crate::iterators::two_merge_iterator::TwoMergeIterator;
use crate::iterators::StorageIterator;
use crate::key::KeySlice;
use crate::lsm_iterator::FusedIterator;
use crate::lsm_storage::{LsmStorageInner, LsmStorageState};
use crate::table::{SsTable, SsTableBuilder, SsTableIterator};

#[derive(Debug, Serialize, Deserialize)]
pub enum CompactionTask {
    Leveled(LeveledCompactionTask),
    Tiered(TieredCompactionTask),
    Simple(SimpleLeveledCompactionTask),
    ForceFullCompaction {
        l0_sstables: Vec<usize>,
        l1_sstables: Vec<usize>,
    },
}

impl CompactionTask {
    fn compact_to_bottom_level(&self) -> bool {
        match self {
            CompactionTask::ForceFullCompaction { .. } => true,
            CompactionTask::Leveled(task) => task.is_lower_level_bottom_level,
            CompactionTask::Simple(task) => task.is_lower_level_bottom_level,
            CompactionTask::Tiered(task) => task.bottom_tier_included,
        }
    }
}

pub(crate) enum CompactionController {
    Leveled(LeveledCompactionController),
    Tiered(TieredCompactionController),
    Simple(SimpleLeveledCompactionController),
    NoCompaction,
}

impl CompactionController {
    pub fn generate_compaction_task(&self, snapshot: &LsmStorageState) -> Option<CompactionTask> {
        match self {
            CompactionController::Leveled(ctrl) => ctrl
                .generate_compaction_task(snapshot)
                .map(CompactionTask::Leveled),
            CompactionController::Simple(ctrl) => ctrl
                .generate_compaction_task(snapshot)
                .map(CompactionTask::Simple),
            CompactionController::Tiered(ctrl) => ctrl
                .generate_compaction_task(snapshot)
                .map(CompactionTask::Tiered),
            CompactionController::NoCompaction => unreachable!(),
        }
    }

    pub fn apply_compaction_result(
        &self,
        snapshot: &LsmStorageState,
        task: &CompactionTask,
        output: &[usize],
    ) -> (LsmStorageState, Vec<usize>) {
        match (self, task) {
            (CompactionController::Leveled(ctrl), CompactionTask::Leveled(task)) => {
                ctrl.apply_compaction_result(snapshot, task, output)
            }
            (CompactionController::Simple(ctrl), CompactionTask::Simple(task)) => {
                ctrl.apply_compaction_result(snapshot, task, output)
            }
            (CompactionController::Tiered(ctrl), CompactionTask::Tiered(task)) => {
                ctrl.apply_compaction_result(snapshot, task, output)
            }
            _ => unreachable!(),
        }
    }
}

impl CompactionController {
    pub fn flush_to_l0(&self) -> bool {
        matches!(
            self,
            Self::Leveled(_) | Self::Simple(_) | Self::NoCompaction
        )
    }
}

#[derive(Debug, Clone)]
pub enum CompactionOptions {
    /// Leveled compaction with partial compaction + dynamic level support (= RocksDB's Leveled
    /// Compaction)
    Leveled(LeveledCompactionOptions),
    /// Tiered compaction (= RocksDB's universal compaction)
    Tiered(TieredCompactionOptions),
    /// Simple leveled compaction
    Simple(SimpleLeveledCompactionOptions),
    /// In no compaction mode (week 1), always flush to L0
    NoCompaction,
}

impl LsmStorageInner {
    fn compact_generate_sst_from_iter(
        &self,
        mut iter: impl for<'a> StorageIterator<KeyType<'a> = KeySlice<'a>>,
        compact_to_bottom_level: bool,
    ) -> Result<Vec<Arc<SsTable>>> {
        let mut new_ssts = Vec::new();
        // compact the iterators
        let mut builder = None;
        while iter.is_valid() {
            if builder.is_none() {
                builder = Some(SsTableBuilder::new(self.options.block_size));
            }
            let builder_inner = builder.as_mut().unwrap();
            if compact_to_bottom_level {
                if !iter.value().is_empty() {
                    builder_inner.add(iter.key(), iter.value());
                }
            } else {
                builder_inner.add(iter.key(), iter.value());
            }

            iter.next()?;

            if builder_inner.estimated_size() >= self.options.target_sst_size {
                println!("compact_generate_sst_from_iter");
                let sst_id = self.next_sst_id();
                let builder = builder.take().unwrap();
                let new_sst = Arc::new(builder.build(
                    sst_id,
                    Some(self.block_cache.clone()),
                    self.path_of_sst(sst_id),
                )?);
                new_ssts.push(new_sst);
            }
        }

        // put last sst if exists builder
        if let Some(builder) = builder {
            println!("compact_generate_sst_from_iter put last");
            let sst_id = self.next_sst_id(); // lock dropped here
            let sst = Arc::new(builder.build(
                sst_id,
                Some(self.block_cache.clone()),
                self.path_of_sst(sst_id),
            )?);
            new_ssts.push(sst);
        }
        Ok(new_ssts)
    }

    fn compact(&self, task: &CompactionTask) -> Result<Vec<Arc<SsTable>>> {
        let snapshot = {
            let state = self.state.read();
            state.clone()
        };

        match task {
            CompactionTask::ForceFullCompaction {
                l0_sstables,
                l1_sstables,
            } => {
                // create l0_sstables
                let mut l0_iters = Vec::with_capacity(l0_sstables.len());
                for sst_id in l0_sstables.iter() {
                    let sst = snapshot.sstables.get(sst_id).unwrap();
                    let iter = SsTableIterator::create_and_seek_to_first(sst.clone())?;
                    l0_iters.push(Box::new(iter));
                }

                // create l1_sstables
                let mut l1_iters = Vec::with_capacity(l1_sstables.len());
                for sst_id in l1_sstables.iter() {
                    let sst = snapshot.sstables.get(sst_id).unwrap();
                    l1_iters.push(sst.clone());
                }

                // merge l0_sstables and l1_sstables
                let iter = FusedIterator::new(TwoMergeIterator::create(
                    MergeIterator::create(l0_iters),
                    SstConcatIterator::create_and_seek_to_first(l1_iters)?,
                )?);
                self.compact_generate_sst_from_iter(iter, task.compact_to_bottom_level())
            }
            CompactionTask::Simple(SimpleLeveledCompactionTask {
                upper_level,
                upper_level_sst_ids,
                lower_level_sst_ids,
                ..
            }) => {
                match upper_level {
                    Some(_) => {
                        // create iterators for upper and lower level sstables
                        let mut upper_ssts = Vec::with_capacity(upper_level_sst_ids.len());
                        for id in upper_level_sst_ids.iter() {
                            upper_ssts.push(snapshot.sstables.get(id).unwrap().clone());
                        }
                        let upper_iter = SstConcatIterator::create_and_seek_to_first(upper_ssts)?;
                        let mut lower_ssts = Vec::with_capacity(upper_level_sst_ids.len());
                        for id in lower_level_sst_ids.iter() {
                            lower_ssts.push(snapshot.sstables.get(id).unwrap().clone());
                        }
                        let lower_iter = SstConcatIterator::create_and_seek_to_first(lower_ssts)?;
                        let iter = TwoMergeIterator::create(upper_iter, lower_iter)?;
                        self.compact_generate_sst_from_iter(iter, task.compact_to_bottom_level())
                    }
                    // because it is L0 compaction, we can not use concat iterator which is for ordered sstables
                    None => {
                        // create iterators for upper and lower level sstables
                        let mut upper_iters = Vec::with_capacity(upper_level_sst_ids.len());
                        for id in upper_level_sst_ids.iter() {
                            let iter = SsTableIterator::create_and_seek_to_first(
                                snapshot.sstables.get(id).unwrap().clone(),
                            )?;
                            upper_iters.push(Box::new(iter));
                        }
                        let upper_merge_iter = MergeIterator::create(upper_iters);
                        let mut lower_ssts = Vec::with_capacity(upper_level_sst_ids.len());
                        for id in lower_level_sst_ids.iter() {
                            lower_ssts.push(snapshot.sstables.get(id).unwrap().clone());
                        }
                        let lower_iter = SstConcatIterator::create_and_seek_to_first(lower_ssts)?;
                        let iter = TwoMergeIterator::create(upper_merge_iter, lower_iter)?;
                        self.compact_generate_sst_from_iter(iter, task.compact_to_bottom_level())
                    }
                }
            }
            _ => unimplemented!(),
        }
    }

    pub fn force_full_compaction(&self) -> Result<()> {
        let snapshot = {
            let state = self.state.read();
            state.clone()
        };

        let l0_sstables = snapshot.l0_sstables.clone();
        let l1_sstables = snapshot.levels[0].1.clone();
        // compact the l0_sstables and l1_sstables to get compacted SSTs
        println!(
            "force full compaction with l0_sstables: {:?}, l1_sstables: {:?}",
            l0_sstables, l1_sstables
        );
        let new_ssts = self.compact(&CompactionTask::ForceFullCompaction {
            l0_sstables: l0_sstables.clone(),
            l1_sstables: l1_sstables.clone(),
        })?;

        // update the state
        let ids;
        {
            let _state_lock = self.state_lock.lock();
            let mut state = self.state.read().as_ref().clone();

            // remove all participants of the compaction from the state
            for sst in l0_sstables.iter().chain(l1_sstables.iter()) {
                state.sstables.remove(sst);
            }

            // remove old l0_sstables from the state
            let mut l0_sstables_map = l0_sstables.iter().copied().collect::<HashSet<_>>();
            state.l0_sstables = state
                .l0_sstables
                .iter()
                .filter(|x| !l0_sstables_map.remove(x))
                .copied()
                .collect::<Vec<_>>();
            assert!(l0_sstables_map.is_empty());

            ids = new_ssts.iter().map(|x| x.sst_id()).collect::<Vec<_>>();
            state.levels[0].1 = ids.clone();
            // insert new SSTs to sstables
            for sst in new_ssts.iter() {
                state.sstables.insert(sst.sst_id(), sst.clone());
            }
            *self.state.write() = Arc::new(state);
        };

        for sst in l0_sstables.iter().chain(l1_sstables.iter()) {
            std::fs::remove_file(self.path_of_sst(*sst))?;
        }
        println!("force full compaction done, new SSTs: {:?}", ids);
        Ok(())
    }

    fn trigger_compaction(&self) -> Result<()> {
        let snapshot = {
            let state = self.state.read();
            state.clone()
        };

        let task = self
            .compaction_controller
            .generate_compaction_task(&snapshot);
        if let Some(task) = task {
            self.dump_structure();
            println!("running compaction task: {:?}", task);
            let new_ssts = self.compact(&task)?;
            let output = new_ssts.iter().map(|x| x.sst_id()).collect::<Vec<_>>();
            let mut snapshot = self.state.read().as_ref().clone();
            // insert new SSTs to sstables
            for ssts_to_add in new_ssts {
                let result = snapshot.sstables.insert(ssts_to_add.sst_id(), ssts_to_add);
                assert!(result.is_none());
            }
            let (mut new_snapshot, files_to_remove) = self
                .compaction_controller
                .apply_compaction_result(&snapshot, &task, &output);
            // remove old SSTs from sstables
            let mut ssts_to_remove = Vec::with_capacity(files_to_remove.len());
            for file_to_remove in &files_to_remove {
                let result = new_snapshot.sstables.remove(file_to_remove);
                assert!(result.is_some(), "cannot remove {}.sst", file_to_remove);
                ssts_to_remove.push(result.unwrap());
            }
            let mut state = self.state.write();
            *state = Arc::new(new_snapshot);
            drop(state);

            println!(
                "compaction finished: {} files removed, {} files added, output={:?}",
                ssts_to_remove.len(),
                output.len(),
                output
            );
            for sst in ssts_to_remove.iter() {
                std::fs::remove_file(self.path_of_sst(sst.sst_id()))?;
            }
        }
        Ok(())
    }

    pub(crate) fn spawn_compaction_thread(
        self: &Arc<Self>,
        rx: crossbeam_channel::Receiver<()>,
    ) -> Result<Option<std::thread::JoinHandle<()>>> {
        if let CompactionOptions::Leveled(_)
        | CompactionOptions::Simple(_)
        | CompactionOptions::Tiered(_) = self.options.compaction_options
        {
            let this = self.clone();
            let handle = std::thread::spawn(move || {
                let ticker = crossbeam_channel::tick(Duration::from_millis(50));
                loop {
                    crossbeam_channel::select! {
                        recv(ticker) -> _ => if let Err(e) = this.trigger_compaction() {
                            eprintln!("compaction failed: {}", e);
                        },
                        recv(rx) -> _ => return
                    }
                }
            });
            return Ok(Some(handle));
        }
        Ok(None)
    }

    fn trigger_flush(&self) -> Result<()> {
        let res = {
            let state = self.state.read();
            state.imm_memtables.len() >= self.options.num_memtable_limit
        };
        if res {
            self.force_flush_next_imm_memtable()?;
        }

        Ok(())
    }

    pub(crate) fn spawn_flush_thread(
        self: &Arc<Self>,
        rx: crossbeam_channel::Receiver<()>,
    ) -> Result<Option<std::thread::JoinHandle<()>>> {
        let this = self.clone();
        let handle = std::thread::spawn(move || {
            let ticker = crossbeam_channel::tick(Duration::from_millis(50));
            loop {
                crossbeam_channel::select! {
                    recv(ticker) -> _ => if let Err(e) = this.trigger_flush() {
                        eprintln!("flush failed: {}", e);
                    },
                    recv(rx) -> _ => return
                }
            }
        });
        Ok(Some(handle))
    }
}
