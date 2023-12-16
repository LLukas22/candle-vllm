use std::{
    collections::HashMap,
    hash::Hash,
    marker::PhantomData,
    ops::Deref,
    sync::{Arc, Mutex, MutexGuard},
};

use super::sequence::{Sequence, SequenceGroup};

pub struct LogicalTokenBlock {
    tokens: Vec<usize>,
    block_id: usize,
    block_size: usize,
    num_tokens: usize,
}

impl LogicalTokenBlock {
    pub fn new(block_id: usize, block_size: usize) -> Self {
        Self {
            tokens: vec![0].repeat(block_size),
            block_id,
            block_size,
            num_tokens: 0,
        }
    }

    pub fn is_full(&self) -> bool {
        self.num_tokens == self.block_size
    }

    pub fn append_token_id(&mut self, token: usize) {
        assert!(!self.is_full());
        self.tokens[self.num_tokens] = token;
        self.num_tokens += 1;
    }

    pub fn append_tokens(&mut self, tokens: &[usize]) {
        for token in tokens {
            self.append_token_id(*token);
        }
    }
}

#[derive(Hash, PartialEq, Eq)]
pub struct _PhysicalTokenBlock {
    pub block_id: usize,
    block_size: usize,
    refcount: usize,
    is_gpu: bool,
}

pub struct PhysicalTokenBlock(pub Mutex<_PhysicalTokenBlock>);

impl PhysicalTokenBlock {
    pub fn deref_mut(&self) -> MutexGuard<'_, _PhysicalTokenBlock> {
        loop {
            if let Ok(v) = self.0.try_lock() {
                return v;
            }
        }
    }
}

impl PartialEq for PhysicalTokenBlock {
    fn eq(&self, other: &Self) -> bool {
        *self.deref() == *other.deref()
    }
}

impl Hash for PhysicalTokenBlock {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.deref().hash(state)
    }
}

impl Eq for PhysicalTokenBlock {}

type BlockTable = Vec<Arc<PhysicalTokenBlock>>;
struct GPUAllocator;
struct CPUAllocator;

struct GPUAllocatorWrapper(usize);
struct CPUAllocatorWrapper(usize);

impl Deref for GPUAllocatorWrapper {
    type Target = usize;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Deref for CPUAllocatorWrapper {
    type Target = usize;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

struct Allocator<T> {
    block_size: usize,
    free_blocks: BlockTable,
    _ghost: PhantomData<T>,
}

impl<T> Allocator<T> {
    fn allocate(&mut self) -> Arc<PhysicalTokenBlock> {
        let mut block = self.free_blocks.pop().unwrap();
        block.deref_mut().refcount = 1;
        block
    }

    fn free_block(&mut self, mut block: Arc<PhysicalTokenBlock>) {
        if block.deref_mut().refcount == 0 {
            panic!(
                "PhysicalTokenBlock with id {} experienced a double free!",
                block.deref_mut().block_id
            );
        }
        block.deref_mut().refcount -= 1;
        if block.deref_mut().refcount == 0 {
            self.free_blocks.push(block);
        }
    }
}

impl Allocator<GPUAllocator> {
    fn new(block_size: usize, num_blocks: usize) -> Self {
        let mut free_blocks = Vec::new();
        for id in 0..num_blocks {
            free_blocks.push(Arc::new(PhysicalTokenBlock(Mutex::new(
                _PhysicalTokenBlock {
                    block_id: id,
                    block_size,
                    refcount: 0,
                    is_gpu: true,
                },
            ))))
        }
        Allocator {
            block_size,
            free_blocks,
            _ghost: PhantomData,
        }
    }

    fn get_num_free_blocks(&self) -> GPUAllocatorWrapper {
        GPUAllocatorWrapper(self.free_blocks.len())
    }

    #[inline(always)]
    const fn is_gpu(&self) -> bool {
        true
    }
}

impl Allocator<CPUAllocator> {
    fn new(block_size: usize, num_blocks: usize) -> Self {
        let mut free_blocks = Vec::new();
        for id in 0..num_blocks {
            free_blocks.push(Arc::new(PhysicalTokenBlock(Mutex::new(
                _PhysicalTokenBlock {
                    block_id: id,
                    block_size,
                    refcount: 0,
                    is_gpu: true,
                },
            ))))
        }
        Allocator {
            block_size,
            free_blocks,
            _ghost: PhantomData,
        }
    }

    fn get_num_free_blocks(&self) -> CPUAllocatorWrapper {
        CPUAllocatorWrapper(self.free_blocks.len())
    }

    #[inline(always)]
    const fn is_gpu(&self) -> bool {
        false
    }
}

pub enum AllocStatus {
    Ok,
    Later,
    Impossible,
}

type SeqID = usize;

/// A BlockEngine maps eachs Sequence (identified by its SeqID), to physical token blocks.
/// The physical token blocks may not match the logical token blocks because during
/// scheduling, physical blocks are allocated to accomodate the new tokens generated.
/// These new tokens will be added to the logical token block for each sequence.
pub struct BlockEngine {
    block_size: usize,
    num_gpu_blocks: usize,
    num_cpu_blocks: usize,
    gpu_allocator: Allocator<GPUAllocator>,
    cpu_allocator: Allocator<CPUAllocator>,
    pub block_tables: HashMap<SeqID, BlockTable>,
}

impl BlockEngine {
    pub fn new(block_size: usize, num_gpu_blocks: usize, num_cpu_blocks: usize) -> Self {
        Self {
            block_size,
            num_gpu_blocks,
            num_cpu_blocks,
            gpu_allocator: Allocator::<GPUAllocator>::new(block_size, num_gpu_blocks),
            cpu_allocator: Allocator::<CPUAllocator>::new(block_size, num_cpu_blocks),
            block_tables: HashMap::new(),
        }
    }

    pub fn can_allocate(&self, seq_group: &SequenceGroup) -> AllocStatus {
        let num_required_blocks = seq_group.get_total_logical_token_blocks();
        let num_free_gpu_blocks = self.gpu_allocator.get_num_free_blocks();

        if self.num_gpu_blocks > *num_free_gpu_blocks + num_required_blocks {
            AllocStatus::Later
        } else if self.num_gpu_blocks < num_required_blocks {
            AllocStatus::Impossible
        } else {
            AllocStatus::Ok
        }
    }

    pub fn allocate(&mut self, seq_group: &SequenceGroup) {
        let mut block_table = Vec::new();
        for logcical_idx in 0..seq_group.get_total_logical_token_blocks() {
            block_table.push(self.gpu_allocator.allocate());
        }
        for (seq_id, _) in seq_group.get_seqs() {
            self.block_tables.insert(*seq_id, block_table.clone());
        }
    }

    pub fn can_append_token_to_seq(&self, seq_group: &SequenceGroup) -> bool {
        let free_blocks = self.gpu_allocator.get_num_free_blocks();
        // Physical blocks = logical blocks
        seq_group.total_blocks_to_add_new_tok() <= *free_blocks
    }

    pub fn free_sequence(&mut self, sequence: &Sequence) {
        let block_table = self
            .block_tables
            .get(&sequence.deref_mut().get_id())
            .unwrap();

        // Free from block table
        for block in block_table {
            if block.deref_mut().is_gpu {
                self.gpu_allocator.free_block(block.clone())
            } else {
                self.cpu_allocator.free_block(block.clone())
            }
        }

        self.block_tables.remove(&sequence.deref_mut().get_id());
    }

    pub fn can_swap_out_seq_group(&self, seq_group: &SequenceGroup) -> bool {
        let blocks_required: usize = self
            .block_tables
            .iter()
            .filter(|(id, _)| seq_group.get_seqs().contains_key(id))
            .map(|(_, table)| table.len())
            .sum();
        blocks_required <= self.cpu_allocator.free_blocks.len()
    }

    /// Update the block table so that the sequence does no longer reserve any GPU
    /// physical blocks, and only has CPU physical blocks.
    pub fn swap_out(&mut self, seq_group: &SequenceGroup) -> HashMap<usize, usize> {
        // GPU block to a CPU block
        let mut new_mapping: HashMap<Arc<PhysicalTokenBlock>, Arc<PhysicalTokenBlock>> =
            HashMap::new();
        for (seq_id, seq) in seq_group.get_seqs() {
            let mut new_block_table = Vec::new();
            let block_table = self.block_tables.get(seq_id).unwrap();

            for gpu_block in block_table {
                let cpu_block = if new_mapping.contains_key(gpu_block) {
                    // Reuse a block
                    let mut cpu_block: Arc<PhysicalTokenBlock> =
                        new_mapping.get(gpu_block).unwrap().clone();
                    cpu_block.deref_mut().refcount += 1;
                    cpu_block
                } else {
                    // Create a new block
                    let cpu_block = self.cpu_allocator.allocate();
                    new_mapping.insert(gpu_block.clone(), cpu_block.clone());
                    cpu_block
                };
                new_block_table.push(cpu_block);
                self.gpu_allocator.free_block(gpu_block.clone());
            }
            self.block_tables.insert(*seq_id, new_block_table);
        }

        new_mapping
            .iter()
            .map(|(k, v)| (k.deref_mut().block_id, v.deref_mut().block_id))
            .collect::<HashMap<_, _>>()
    }

    // Returns the COW mapping (src, dst).
    // COW is performed if there are multiple references to the last phyiscal block.
    pub fn append_token_slot_to_seq(&mut self, sequence: &Sequence) -> Option<(usize, usize)> {
        let table = self
            .block_tables
            .get_mut(&sequence.deref_mut().get_id())
            .unwrap();

        match sequence.deref_mut().blocks_to_add_new_tok() {
            1 => {
                table.push(self.gpu_allocator.allocate());
                None
            }
            0 => {
                let last_block = table.last_mut().unwrap();
                assert!(last_block.deref_mut().is_gpu);
                if last_block.deref_mut().refcount == 1 {
                    None
                } else {
                    // We would be writing into shared, so COW.
                    let new_block = self.gpu_allocator.allocate();
                    self.gpu_allocator.free_block(last_block.clone());
                    let old_number = last_block.deref_mut().block_id;
                    let new_number = new_block.deref_mut().block_id;
                    *last_block = new_block;
                    Some((old_number, new_number))
                }
            }
            _ => {
                unreachable!()
            }
        }
    }

    pub fn can_swap_in_seq_group(&self, seq_group: &SequenceGroup) -> bool {
        let blocks_required: usize = self
            .block_tables
            .iter()
            .filter(|(id, _)| seq_group.get_seqs().contains_key(id))
            .map(|(_, table)| table.len())
            .sum();
        blocks_required <= self.gpu_allocator.free_blocks.len()
    }

    /// Update the block table so that the sequence does no longer reserve any CPU
    /// physical blocks, and only has GPU physical blocks.
    pub fn swap_in(&mut self, seq_group: &SequenceGroup) -> HashMap<usize, usize> {
        // CPU block to a GPU block
        let mut new_mapping: HashMap<Arc<PhysicalTokenBlock>, Arc<PhysicalTokenBlock>> =
            HashMap::new();
        for (seq_id, seq) in seq_group.get_seqs() {
            let mut new_block_table = Vec::new();
            let block_table = self.block_tables.get(seq_id).unwrap();

            for cpu_block in block_table {
                let gpu_block = if new_mapping.contains_key(cpu_block) {
                    // Reuse a block
                    let gpu_block: Arc<PhysicalTokenBlock> =
                        new_mapping.get(cpu_block).unwrap().clone();
                    gpu_block.deref_mut().refcount += 1;
                    gpu_block
                } else {
                    // Create a new block
                    let gpu_block = self.cpu_allocator.allocate();
                    new_mapping.insert(cpu_block.clone(), gpu_block.clone());
                    gpu_block
                };
                new_block_table.push(gpu_block);
                self.gpu_allocator.free_block(cpu_block.clone());
            }
            self.block_tables.insert(*seq_id, new_block_table);
        }

        new_mapping
            .iter()
            .map(|(k, v)| (k.deref_mut().block_id, v.deref_mut().block_id))
            .collect::<HashMap<_, _>>()
    }
}
