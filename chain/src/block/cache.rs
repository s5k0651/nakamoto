#[cfg(test)]
pub mod test;

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, VecDeque};

use bitcoin::blockdata::block::BlockHeader;
use bitcoin::consensus::params::Params;
use bitcoin::hash_types::BlockHash;
use bitcoin::network::constants::Network;
use bitcoin::util::hash::BitcoinHash;

use nonempty::NonEmpty;

use nakamoto_common::block::tree::{BlockTree, Branch, Error, ImportResult};
use nakamoto_common::block::{
    self,
    iter::Iter,
    store::Store,
    time::{self, Clock},
    CachedBlock, Height, Target, Time,
};

/// A chain candidate, forking off the active chain.
#[derive(Debug)]
struct Candidate {
    tip: BlockHash,
    headers: Vec<BlockHeader>,
    fork_height: Height,
    fork_hash: BlockHash,
}

/// An implementation of `BlockTree`.
#[derive(Debug, Clone)]
pub struct BlockCache<S: Store> {
    chain: NonEmpty<CachedBlock>,
    headers: HashMap<BlockHash, Height>,
    orphans: HashMap<BlockHash, BlockHeader>,
    checkpoints: BTreeMap<Height, BlockHash>,
    params: Params,
    store: S,
}

impl<S: Store> BlockCache<S> {
    /// Create a new `BlockCache` from a `Store`, consensus parameters, and checkpoints.
    pub fn from(
        store: S,
        params: Params,
        checkpoints: &[(Height, BlockHash)],
    ) -> Result<Self, Error> {
        let genesis = store.genesis();
        let length = store.len()?;
        let orphans = HashMap::new();
        let checkpoints = checkpoints.iter().cloned().collect();

        let chain = NonEmpty::from((
            CachedBlock {
                height: 0,
                hash: genesis.bitcoin_hash(),
                header: genesis,
            },
            Vec::with_capacity(length - 1),
        ));
        let mut headers = HashMap::with_capacity(length);
        // Insert genesis in the headers map, but skip it during iteration.
        headers.insert(chain.head.hash, 0);

        let mut cache = Self {
            chain,
            headers,
            orphans,
            params,
            checkpoints,
            store,
        };

        for result in cache.store.iter().skip(1) {
            let (height, header) = result?;
            let hash = header.bitcoin_hash();

            cache.extend_chain(height, hash, header);
        }

        assert_eq!(length, cache.chain.len());
        assert_eq!(length, cache.headers.len());

        Ok(cache)
    }

    /// Iterate over a range of blocks.
    pub fn range<'a>(
        &'a self,
        range: std::ops::Range<Height>,
    ) -> impl Iterator<Item = &CachedBlock> + 'a {
        self.chain
            .iter()
            .skip(range.start as usize)
            .take((range.end - range.start) as usize)
    }

    /// Get the median time past for the blocks leading up to the given height.
    ///
    /// Panics if height is `0`.
    ///
    pub fn median_time_past(&self, height: Height) -> Time {
        assert!(height != 0, "height must be > 0");

        let mut times = [0 as Time; time::MEDIAN_TIME_SPAN as usize];

        let start = height.saturating_sub(time::MEDIAN_TIME_SPAN);
        let end = height;

        for (i, blk) in self.range(start..end).enumerate() {
            times[i] = blk.time;
        }

        // Gracefully handle the case where `height` < `MEDIUM_TIME_SPAN`.
        let available = &mut times[0..(end - start) as usize];

        available.sort();
        available[available.len() / 2]
    }

    /// Import a block into the tree. Performs header validation.
    fn import_block(
        &mut self,
        header: BlockHeader,
        clock: &impl Clock,
    ) -> Result<ImportResult, Error> {
        let hash = header.bitcoin_hash();
        let tip = self.chain.last();
        let best = tip.hash;

        // Block extends the active chain.
        if header.prev_blockhash == best {
            let height = tip.height + 1;

            self.validate(&tip, &header, clock)?;
            self.extend_chain(height, hash, header);
            self.store.put(std::iter::once(header))?;
        } else if self.headers.contains_key(&hash) || self.orphans.contains_key(&hash) {
            return Err(Error::DuplicateBlock(hash));
        } else {
            if let Some(height) = self.headers.get(&header.prev_blockhash) {
                // Don't accept any forks from the main chain, prior to the last checkpoint.
                if *height < self.last_checkpoint() {
                    return Err(Error::InvalidBlockHeight(*height + 1));
                }
            }

            // Validate that the block's PoW (1) is valid against its difficulty target, and (2)
            // is greater than the minimum allowed for this network.
            //
            // We do this because it's cheap to verify and prevents flooding attacks.
            let target = header.target();
            match header.validate_pow(&target) {
                Ok(_) => {
                    let limit = self.params.pow_limit;
                    if target > limit {
                        return Err(Error::InvalidBlockTarget(target, limit));
                    }
                }
                Err(bitcoin::util::Error::BlockBadProofOfWork) => {
                    return Err(Error::InvalidBlockPoW);
                }
                Err(bitcoin::util::Error::BlockBadTarget) => {
                    // The only way to get a 'bad target' error is to pass a different target
                    // than the one specified in the header.
                    unreachable!();
                }
                Err(_) => {
                    // We've handled all possible errors above.
                    unreachable!();
                }
            }
            self.orphans.insert(hash, header);
        }

        // Activate the chain with the most work.

        let candidates = self.chain_candidates(clock);

        // TODO: What are we trying to do here? We're saying that if there are no
        // forks, and this header has no parent, we return an error. But:
        //
        // If there are forks, it doesn't mean this header is part of one. It could
        // be a fork that already existed before this header was received.
        //
        // What we should do is simply: if the block has no parent (is orphan), we
        // know it's a no-op, ie. we won't discover a better branch. So we always
        // return the error without even checking for candidates. Otherwise, if
        // it *does* have a parent, we check for candidates.
        if candidates.is_empty()
            && !self.headers.contains_key(&header.prev_blockhash)
            && !self.orphans.contains_key(&header.prev_blockhash)
        {
            return Err(Error::BlockMissing(header.prev_blockhash));
        }

        // TODO: Don't switch multiple times. Switch to the best branch in one go.
        for branch in candidates.iter() {
            let candidate_work = Branch(&branch.headers).work();
            let main_work = Branch(self.chain_suffix(branch.fork_height)).work();

            // TODO: Validate branch before switching to it.
            if candidate_work > main_work {
                self.switch_to_fork(branch)?;
            } else if self.params.network != Network::Bitcoin {
                if candidate_work == main_work {
                    // Nb. We intend here to compare the hashes as integers, and pick the lowest
                    // hash as the winner. However, the `PartialEq` on `BlockHash` is implemented on
                    // the underlying `[u8]` array, and does something different (lexographical
                    // comparison). Since this code isn't run on Mainnet, it's okay, as it serves
                    // its purpose of being determinstic when choosing the active chain.
                    if branch.tip < self.chain.last().hash {
                        self.switch_to_fork(branch)?;
                    }
                }
            }
        }

        let (hash, _) = self.tip();
        if hash != best {
            Ok(ImportResult::TipChanged(hash, self.height(), vec![]))
        } else {
            Ok(ImportResult::TipUnchanged)
        }
    }

    fn chain_candidates(&self, clock: &impl Clock) -> Vec<Candidate> {
        let mut branches = Vec::new();

        for tip in self.orphans.keys() {
            if let Some(branch) = self.branch(tip) {
                if self.validate_branch(&branch, clock).is_ok() {
                    branches.push(branch);
                }
            }
        }
        branches
    }

    fn branch(&self, tip: &BlockHash) -> Option<Candidate> {
        let tip = *tip;

        let mut headers = VecDeque::new();
        let mut cursor = tip;

        while let Some(header) = self.orphans.get(&cursor) {
            cursor = header.prev_blockhash;
            headers.push_front(*header);
        }

        if let Some(height) = self.headers.get(&cursor) {
            return Some(Candidate {
                tip,
                fork_height: *height,
                fork_hash: cursor,
                headers: headers.into(),
            });
        }
        None
    }

    fn validate_branch(&self, candidate: &Candidate, clock: &impl Clock) -> Result<(), Error> {
        let fork_header = self
            .get_block_by_height(candidate.fork_height)
            .expect("the given candidate must fork from a known block");
        let mut tip = CachedBlock {
            height: candidate.fork_height,
            hash: candidate.fork_hash,
            header: *fork_header,
        };

        for header in candidate.headers.iter() {
            self.validate(&tip, header, clock)?;

            tip = CachedBlock {
                height: tip.height + 1,
                hash: header.bitcoin_hash(),
                header: *header,
            };
        }
        Ok(())
    }

    fn validate(
        &self,
        tip: &CachedBlock,
        header: &BlockHeader,
        clock: &impl Clock,
    ) -> Result<(), Error> {
        assert_eq!(tip.hash, header.prev_blockhash);

        let target = if self.params.allow_min_difficulty_blocks
            && (tip.height + 1) % self.params.difficulty_adjustment_interval() != 0
        {
            if header.time > tip.time + self.params.pow_target_spacing as Time * 2 {
                self.params.pow_limit
            } else {
                self.next_min_difficulty_target(&self.params)
            }
        } else {
            self.next_difficulty_target(tip.height, tip.time, tip.target(), &self.params)
        };

        // Convert the target back and forth to make sure it has 32 bits of precision instead of
        // 256 bits, since the block header target only has 32 bits.
        let compact_target =
            block::target_from_bits(BlockHeader::compact_target_from_u256(&target));

        match header.validate_pow(&compact_target) {
            Err(bitcoin::util::Error::BlockBadProofOfWork) => {
                return Err(Error::InvalidBlockPoW);
            }
            Err(bitcoin::util::Error::BlockBadTarget) => {
                return Err(Error::InvalidBlockTarget(header.target(), compact_target));
            }
            Err(_) => unreachable!(),
            Ok(_) => {}
        }

        // Validate against block checkpoints.
        let height = tip.height + 1;

        if let Some(checkpoint) = self.checkpoints.get(&height) {
            let hash = header.bitcoin_hash();

            if &hash != checkpoint {
                return Err(Error::InvalidBlockHash(hash, height));
            }
        }

        // A timestamp is accepted as valid if it is greater than the median timestamp of
        // the previous MEDIAN_TIME_SPAN blocks, and less than the network-adjusted
        // time + MAX_FUTURE_BLOCK_TIME.
        if header.time <= self.median_time_past(height) {
            return Err(Error::InvalidTimestamp(header.time, Ordering::Less));
        }
        if header.time > clock.time() + time::MAX_FUTURE_BLOCK_TIME {
            return Err(Error::InvalidTimestamp(header.time, Ordering::Greater));
        }

        Ok(())
    }

    fn last_checkpoint(&self) -> Height {
        let height = self.height();

        self.checkpoints
            .iter()
            .rev()
            .map(|(h, _)| *h)
            .find(|h| *h <= height)
            .unwrap_or(0)
    }

    fn next_min_difficulty_target(&self, params: &Params) -> Target {
        let pow_limit_bits = block::pow_limit_bits(&params.network);

        for (height, header) in self.iter().rev() {
            if header.bits != pow_limit_bits
                || height % self.params.difficulty_adjustment_interval() == 0
            {
                return header.target();
            }
        }
        block::target_from_bits(pow_limit_bits)
    }

    /// Rollback active chain to the given height. Returns the list of rolled-back headers.
    fn rollback(&mut self, height: Height) -> Result<Vec<BlockHeader>, Error> {
        let mut stale = Vec::new();

        for block in self.chain.tail.drain(height as usize..) {
            stale.push(block.header);

            self.headers.remove(&block.hash);
            self.orphans.insert(block.hash, block.header);
        }
        self.store.rollback(height)?;

        Ok(stale)
    }

    /// Activate a fork candidate. Returns the list of rolled-back (stale) headers.
    fn switch_to_fork(&mut self, branch: &Candidate) -> Result<Vec<BlockHeader>, Error> {
        let stale = self.rollback(branch.fork_height)?;

        for (i, header) in branch.headers.iter().enumerate() {
            self.extend_chain(
                branch.fork_height + i as Height + 1,
                header.bitcoin_hash(),
                *header,
            );
        }
        self.store.put(branch.headers.iter().cloned())?;

        Ok(stale)
    }

    /// Extend the active chain with a block.
    fn extend_chain(&mut self, height: Height, hash: BlockHash, header: BlockHeader) {
        assert_eq!(header.prev_blockhash, self.chain.last().hash);

        self.headers.insert(hash, height);
        self.orphans.remove(&hash);
        self.chain.push(CachedBlock {
            height,
            hash,
            header,
        });
    }

    // TODO: Doctest.
    fn chain_suffix(&self, height: Height) -> &[CachedBlock] {
        &self.chain.tail[height as usize..]
    }
}

impl<S: Store> BlockTree for BlockCache<S> {
    fn import_blocks<I: Iterator<Item = BlockHeader>, C: Clock>(
        &mut self,
        chain: I,
        context: &C,
    ) -> Result<ImportResult, Error> {
        let mut result = None;

        for (i, header) in chain.enumerate() {
            match self.import_block(header, context) {
                Ok(r) => result = Some(r),
                Err(Error::DuplicateBlock(hash)) => log::trace!("Duplicate block {}", hash),
                Err(Error::BlockMissing(hash)) => log::trace!("Missing block {}", hash),
                Err(err) => return Err(Error::BlockImportAborted(err.into(), i, self.height())),
            }
        }
        Ok(result.unwrap_or(ImportResult::TipUnchanged))
    }

    fn extend_tip<C: Clock>(
        &mut self,
        header: BlockHeader,
        clock: &C,
    ) -> Result<ImportResult, Error> {
        let tip = self.chain.last();
        let hash = header.bitcoin_hash();

        if header.prev_blockhash == tip.hash {
            let height = tip.height + 1;

            self.validate(&tip, &header, clock)?;
            self.extend_chain(height, hash, header);
            self.store.put(std::iter::once(header))?;

            Ok(ImportResult::TipChanged(hash, height, vec![]))
        } else {
            Ok(ImportResult::TipUnchanged)
        }
    }

    fn get_block(&self, hash: &BlockHash) -> Option<(Height, &BlockHeader)> {
        self.headers
            .get(hash)
            .and_then(|height| self.chain.get(*height as usize))
            .map(|blk| (blk.height, &blk.header))
    }

    fn get_block_by_height(&self, height: Height) -> Option<&BlockHeader> {
        self.chain.get(height as usize).map(|b| &b.header)
    }

    fn tip(&self) -> (BlockHash, BlockHeader) {
        (self.chain.last().hash, self.chain.last().header)
    }

    fn genesis(&self) -> &BlockHeader {
        &self.chain.first().header
    }

    /// Iterate over the longest chain, starting from genesis.
    fn iter<'a>(&'a self) -> Box<dyn DoubleEndedIterator<Item = (Height, BlockHeader)> + 'a> {
        Box::new(Iter::new(&self.chain).map(|(i, h)| (i, h.header)))
    }

    /// Return the height of the longest chain.
    fn height(&self) -> Height {
        self.chain.tail.len() as Height
    }

    /// Check whether this block hash is known.
    fn is_known(&self, hash: &BlockHash) -> bool {
        self.headers.contains_key(hash) || self.orphans.contains_key(hash)
    }

    /// Check whether this block hash is part of the active chain.
    fn contains(&self, hash: &BlockHash) -> bool {
        self.headers.contains_key(hash)
    }

    /// Get the locator hashes for the active chain, starting at the given height.
    ///
    /// *Panics* if the given starting height is out of bounds.
    ///
    fn locator_hashes(&self, from: Height) -> Vec<BlockHash> {
        let mut hashes = Vec::new();

        assert!(from <= self.height());

        let last_checkpoint = self.last_checkpoint();

        for height in block::locators_indexes(from).into_iter() {
            if height < last_checkpoint {
                // Don't go past the latest checkpoint. We never want to accept a fork
                // older than our last checkpoint.
                break;
            }
            if let Some(blk) = self.chain.get(height as usize) {
                hashes.push(blk.hash);
            }
        }
        hashes
    }
}