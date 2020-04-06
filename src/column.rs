// Copyright 2015-2020 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use parking_lot::RwLock;
use crate::{
	error::{Error, Result},
	table::{TableId as ValueTableId, ValueTable, Key, Value, Address},
	log::{Log, LogOverlays, LogReader, LogWriter, LogAction},
	display::hex,
	index::{IndexTable, TableId as IndexTableId, PlanOutcome},
};

const START_BITS: u8 = 16;
const MAX_REBALANCE_BATCH: u32 = 1024;

pub type ColId = u8;

struct Tables {
	index: IndexTable,
	value: [ValueTable; 16],
}

struct Rebalance {
	queue: VecDeque<IndexTable>,
	progress: AtomicU64,
}

pub struct Column {
	tables: RwLock<Tables>,
	rebalance: RwLock<Rebalance>,
	path: std::path::PathBuf,
}

impl Column {
	pub fn get(&self, key: &Key, log: &LogOverlays) -> Result<Option<Value>> {
		let tables = self.tables.read();
		if let Some(value) = Self::get_in_index(key, &tables.index, &*tables, log)? {
			return Ok(Some(value));
		}
		for r in &self.rebalance.read().queue {
			if let Some(value) = Self::get_in_index(key, &r, &*tables, log)? {
				return Ok(Some(value));
			}
		}
		Ok(None)
	}

	fn get_in_index(key: &Key, index: &IndexTable, tables: &Tables, log: &LogOverlays) -> Result<Option<Value>> {
		let (mut entry, mut sub_index) = index.get(key, 0, log);
		while !entry.is_empty() {
			let size_tier = entry.address().size_tier() as usize;
			match tables.value[size_tier].get(key, entry.address().offset(), log)? {
				Some(value) => return Ok(Some(value)),
				None =>  {
					let (next_entry, next_index) = index.get(key, sub_index + 1, log);
					entry = next_entry;
					sub_index = next_index;
				}
			}
		}
		Ok(None)
	}
	pub fn open(col: ColId, path: &std::path::Path) -> Result<Column> {
		let (index, rebalancing) = Self::open_index(path, col)?;
		let tables = Tables {
			index,
			value: [
				Self::open_table(path, col, 0, Some(96))?,
				Self::open_table(path, col, 1, Some(128))?,
				Self::open_table(path, col, 2, Some(192))?,
				Self::open_table(path, col, 3, Some(256))?,
				Self::open_table(path, col, 4, Some(320))?,
				Self::open_table(path, col, 5, Some(512))?,
				Self::open_table(path, col, 6, Some(768))?,
				Self::open_table(path, col, 7, Some(1024))?,
				Self::open_table(path, col, 8, Some(1536))?,
				Self::open_table(path, col, 9, Some(2048))?,
				Self::open_table(path, col, 10, Some(3072))?,
				Self::open_table(path, col, 11, Some(4096))?,
				Self::open_table(path, col, 12, Some(8192))?,
				Self::open_table(path, col, 13, Some(16384))?,
				Self::open_table(path, col, 14, Some(32768))?,
				Self::open_table(path, col, 15, None)?,
			],
		};
		Ok(Column {
			tables: RwLock::new(tables),
			rebalance: RwLock::new(Rebalance {
				queue: rebalancing,
				progress: AtomicU64::new(0),
			}),
			path: path.into(),
		})
	}

	fn open_index(path: &std::path::Path, col: ColId) -> Result<(IndexTable, VecDeque<IndexTable>)> {
		let mut rebalancing = VecDeque::new();
		let mut top = None;
		for bits in (START_BITS .. 65).rev() {
			let id = IndexTableId::new(col, bits);
			if let Some(table) = IndexTable::open_existing(path, id)? {
				if top.is_none() {
					top = Some(table);
				} else {
					rebalancing.push_front(table);
				}
			}
		}
		let table = match top {
			Some(table) => table,
			None => IndexTable::create_new(path, IndexTableId::new(col, START_BITS)),
		};
		Ok((table, rebalancing))
	}

	fn open_table(path: &std::path::Path, col: ColId, tier: u8, entry_size: Option<u16>) -> Result<ValueTable> {
		let id = ValueTableId::new(col, tier);
		ValueTable::open(path, id, entry_size)
	}

	fn trigger_rebalance(
		tables: parking_lot::RwLockUpgradableReadGuard<Tables>,
		rebalance: &RwLock<Rebalance>,
		path: &std::path::Path,
	) {
		let mut tables = parking_lot::RwLockUpgradableReadGuard::upgrade(tables);
		let mut rebalance = rebalance.write();
		log::info!(
			target: "parity-db",
			"Started reindex for {} at {}/{} full",
			tables.index.id,
			tables.index.num_entries(),
			tables.index.id.total_entries(),
		);
		// Start rebalance
		let new_index_id = IndexTableId::new(
			tables.index.id.col(),
			tables.index.id.index_bits() + 1
		);
		let new_table = IndexTable::create_new(path, new_index_id);
		let old_table = std::mem::replace(&mut tables.index, new_table);
		rebalance.queue.push_back(old_table);
	}

	pub fn write_index_plan(&self, key: &Key, address: Address, log: &mut LogWriter) -> Result<PlanOutcome> {
		let tables = self.tables.upgradable_read();
		match tables.index.write_insert_plan(key, address, None, log)? {
			PlanOutcome::NeedRebalance => {
				log::debug!(target: "parity-db", "{}: Index chunk full {}", tables.index.id, hex(key));
				Self::trigger_rebalance(tables, &self.rebalance, self.path.as_path());
				self.write_index_plan(key, address, log)?;
				return Ok(PlanOutcome::NeedRebalance);
			}
			_ => {
				return Ok(PlanOutcome::Written);
			}
		}
	}

	pub fn write_plan(&self, key: &Key, value: &Option<Value>, log: &mut LogWriter) -> Result<PlanOutcome> {
		//TODO: return sub-chunk position in index.get
		let tables = self.tables.upgradable_read();
		if let &Some(ref val) = value {
			let target_tier = tables.value.iter().position(|t| val.len() <= t.value_size() as usize);
			let target_tier = match target_tier {
				Some(tier) => tier as usize,
				None => {
					log::trace!(target: "parity-db", "Inserted blob {}", hex(key));
					15
				}
			};

			let (mut existing_entry, mut sub_index) = tables.index.get(key, 0, log);
			while !existing_entry.is_empty() {
				let existing_address = existing_entry.address();
				let existing_tier = existing_address.size_tier() as usize;
				let replace = tables.value[existing_tier].has_key_at(existing_address.offset(), &key, log)?;
				if replace {
					if existing_tier == target_tier {
						log::trace!(target: "parity-db", "{}: Replacing {}", tables.index.id, hex(key));
						tables.value[target_tier].write_replace_plan(existing_address.offset(), key, val, log)?;
						return Ok(PlanOutcome::Written);
					} else {
						log::trace!(target: "parity-db", "{}: Replacing in a new table {}", tables.index.id, hex(key));
						tables.value[existing_tier].write_remove_plan(existing_address.offset(), log)?;
						let new_offset = tables.value[target_tier].write_insert_plan(key, val, log)?;
						let new_address = Address::new(new_offset, target_tier as u8);
						return tables.index.write_insert_plan(key, new_address, Some(sub_index), log);
					}
				} else {
					// Fall thorough to insertion
					log::debug!(
						target: "parity-db",
						"{}: Index chunk conflict {} vs {:?}",
						tables.index.id,
						hex(key),
						hex(&tables.value[existing_tier].partial_key_at(existing_address.offset(), log).unwrap().unwrap()),
					);
				}
				let (next_entry, next_index) = tables.index.get(key, sub_index + 1, log);
				existing_entry = next_entry;
				sub_index = next_index;
			}

			log::trace!(target: "parity-db", "{}: Inserting new index {}", tables.index.id, hex(key));
			let offset = tables.value[target_tier].write_insert_plan(key, val, log)?;
			let address = Address::new(offset, target_tier as u8);
			match tables.index.write_insert_plan(key, address, None, log)? {
				PlanOutcome::NeedRebalance => {
					log::debug!(target: "parity-db", "{}: Index chunk full {}", tables.index.id, hex(key));
					Self::trigger_rebalance(tables, &self.rebalance, self.path.as_path());
					self.write_plan(key, value, log)?;
					return Ok(PlanOutcome::NeedRebalance);
				}
				_ => {
					return Ok(PlanOutcome::Written);
				}
			}
		} else {
			// Deletion
			let (mut existing_entry, mut sub_index) = tables.index.get(key, 0, log);
			while !existing_entry.is_empty() {
				let existing_tier = existing_entry.address().size_tier() as usize;
				// TODO: Remove this check? Highly unlikely.
				if tables.value[existing_tier].has_key_at(existing_entry.address().offset(), &key, log)? {
					log::trace!(target: "parity-db", "{}: Deleting {}", tables.index.id, hex(key));
					tables.value[existing_tier].write_remove_plan(existing_entry.address().offset(), log)?;
					tables.index.write_remove_plan(key, sub_index, log)?;
					return Ok(PlanOutcome::Written);
				}
				let (next_entry, next_index) = tables.index.get(key, sub_index + 1, log);
				existing_entry = next_entry;
				sub_index = next_index;
			}
		}
		Ok(PlanOutcome::Skipped)
	}

	pub fn enact_plan(&self, action: LogAction, log: &mut LogReader) -> Result<()> {
		let tables = self.tables.read();
		let rebalance = self.rebalance.read();
		match action {
			LogAction::InsertIndex(record) => {
				if tables.index.id == record.table {
					tables.index.enact_plan(record.index, log)?;
				} else if let Some(table) = rebalance.queue.iter().find(|r|r.id == record.table) {
					table.enact_plan(record.index, log)?;
				}
				else {
					log::warn!(
						target: "parity-db",
						"Missing table {}",
						record.table,
					);
					return Err(Error::Corruption("Missing table".into()));
				}
			},
			LogAction::InsertValue(record) => {
				tables.value[record.table.size_tier() as usize].enact_plan(record.index, log)?;
			}
			_ => panic!("Unexpected log action"),
		}
		Ok(())
	}

	pub fn validate_plan(&self, action: LogAction, log: &mut LogReader) -> Result<()> {
		let tables = self.tables.read();
		let rebalance = self.rebalance.read();
		match action {
			LogAction::InsertIndex(record) => {
				if tables.index.id == record.table {
					tables.index.validate_plan(record.index, log)?;
				} else if let Some(table) = rebalance.queue.iter().find(|r|r.id == record.table) {
					table.validate_plan(record.index, log)?;
				}
				else {
					return Err(Error::Corruption("Missing table".into()));
				}
			},
			LogAction::InsertValue(record) => {
				tables.value[record.table.size_tier() as usize].validate_plan(record.index, log)?;
			}
			_ => panic!("Unexpected log action"),
		}
		Ok(())
	}

	pub fn complete_plan(&self, log: &mut LogWriter) -> Result<()> {
		let tables = self.tables.read();
		for t in tables.value.iter() {
			t.complete_plan(log)?;
		}
		Ok(())
	}

	pub fn refresh_metadata(&self) -> Result<()> {
		let tables = self.tables.read();
		for t in tables.value.iter() {
			t.refresh_metadata()?;
		}
		Ok(())
	}

	pub fn rebalance(&self, _log: &Log) -> Result<(Option<IndexTableId>, Vec<(Key, Address)>)> {
		// TODO: handle overlay
		let tables = self.tables.read();
		let rebalance = self.rebalance.read();
		let mut plan = Vec::new();
		let mut drop_index = None;
		if let Some(source) = rebalance.queue.front() {
			let progress = rebalance.progress.load(Ordering::Relaxed);
			if progress != source.id.total_chunks() {
				let mut source_index = progress;
				let mut count = 0;
				if source_index % 50 == 0 {
					log::info!(target: "parity-db", "{}: Reindexing at {}/{}", tables.index.id, source_index, source.id.total_chunks());
				}
				log::debug!(target: "parity-db", "{}: Continue rebalance at {}/{}", tables.index.id, source_index, source.id.total_chunks());
				let shift_key_bits = source.id.index_bits() - 16;
				while source_index < source.id.total_chunks() && count < MAX_REBALANCE_BATCH {
					log::trace!(target: "parity-db", "{}: Rebalancing {}", source.id, source_index);
					let entries = source.raw_entries(source_index);
					for entry in entries.iter() {
						if entry.is_empty() {
							continue;
						}
						let mut key = {
							tables.value[entry.address().size_tier() as usize]
							.raw_partial_key_at(entry.address().offset())?
							.ok_or_else(|| Error::Corruption("Bad value table key".into()))?
						};
						// restore 16 high bits
						&mut key[0..2].copy_from_slice(&((source_index >> shift_key_bits) as u16).to_be_bytes());
						log::trace!(target: "parity-db", "{}: Reinserting {}", source.id, hex(&key));
						plan.push((key, entry.address()))
					}
					count += 1;
					source_index += 1;
				}
				log::trace!(target: "parity-db", "{}: End rebalance batch {} ({})", tables.index.id, source_index, count);
				rebalance.progress.store(source_index, Ordering::Relaxed);
				if source_index == source.id.total_chunks() {
					log::info!(target: "parity-db", "Completed rebalance into {}", tables.index.id);
					drop_index = Some(source.id);
				}
			}
		}
		Ok((drop_index, plan))
	}

	pub fn drop_index(&self, id: IndexTableId) -> Result<()> {
		log::debug!(target: "parity-db", "Dropping {}", id);
		let mut rebalance = self.rebalance.write();
		if rebalance.queue.front_mut().map_or(false, |index| index.id == id) {
			let table = rebalance.queue.pop_front();
			rebalance.progress.store(0, Ordering::Relaxed);
			table.unwrap().drop_file()?;
		} else {
			log::warn!(target: "parity-db", "Dropping invalid index {}", id);
			return Ok(());
		}
		log::debug!(target: "parity-db", "Dropped {}", id);
		Ok(())
	}
}
