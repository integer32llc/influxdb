use std::collections::BTreeSet;
use std::sync::Arc;

use mutable_buffer::chunk::Chunk as MBChunk;
use parquet_file::chunk::Chunk as ParquetChunk;
use read_buffer::Database as ReadBufferDb;

use super::{InternalChunkState, Result};

/// The state a Chunk is in and what its underlying backing storage is
#[derive(Debug)]
pub enum ChunkState {
    /// An invalid chunk state that should not be externally observed
    ///
    /// Used internally to allow moving data between enum variants
    Invalid,

    /// Chunk can accept new writes
    Open(MBChunk),

    /// Chunk can still accept new writes, but will likely be closed soon
    Closing(MBChunk),

    /// Chunk is closed for new writes and has become read only
    Closed(Arc<MBChunk>),

    /// Chunk is closed for new writes, and is actively moving to the read
    /// buffer
    Moving(Arc<MBChunk>),

    /// Chunk has been completely loaded in the read buffer
    Moved(Arc<ReadBufferDb>), // todo use read buffer chunk here

    /// Chunk has been completely loaded in the object store
    ObjectStore(Arc<ParquetChunk>),
}

impl ChunkState {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Invalid => "Invalid",
            Self::Open(_) => "Open",
            Self::Closing(_) => "Closing",
            Self::Closed(_) => "Closed",
            Self::Moving(_) => "Moving",
            Self::Moved(_) => "Moved",
            Self::ObjectStore(_) => "ObjectStore",
        }
    }
}

/// The catalog representation of a Chunk in IOx. Note that a chunk
/// may exist in several physical locations at any given time (e.g. in
/// mutable buffer and in read buffer)
#[derive(Debug)]
pub struct Chunk {
    /// What partition does the chunk belong to?
    partition_key: Arc<String>,

    /// The ID of the chunk
    id: u32,

    /// The state of this chunk
    state: ChunkState,
}

macro_rules! unexpected_state {
    ($SELF: expr, $OP: expr, $EXPECTED: expr, $STATE: expr) => {
        InternalChunkState {
            partition_key: $SELF.partition_key.as_str(),
            chunk_id: $SELF.id,
            operation: $OP,
            expected: $EXPECTED,
            actual: $STATE.name(),
        }
        .fail()
    };
}

impl Chunk {
    /// Create a new chunk in the Open state
    pub(crate) fn new(partition_key: impl Into<String>, id: u32, state: ChunkState) -> Self {
        Self {
            partition_key: Arc::new(partition_key.into()),
            id,
            state,
        }
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn key(&self) -> &str {
        self.partition_key.as_ref()
    }

    pub fn state(&self) -> &ChunkState {
        &self.state
    }

    /// Returns true if this chunk contains a table with the provided name
    pub fn has_table(&self, table_name: &str) -> bool {
        match &self.state {
            ChunkState::Invalid => false,
            ChunkState::Open(chunk) | ChunkState::Closing(chunk) => chunk.has_table(table_name),
            ChunkState::Moving(chunk) | ChunkState::Closed(chunk) => chunk.has_table(table_name),
            ChunkState::Moved(db) => {
                db.has_table(self.partition_key.as_str(), table_name, &[self.id])
            }
            ChunkState::ObjectStore(chunk) => chunk.has_table(table_name),
        }
    }

    /// Collects the chunk's table names into `names`
    pub fn table_names(&self, names: &mut BTreeSet<String>) {
        match &self.state {
            ChunkState::Invalid => {}
            ChunkState::Open(chunk) | ChunkState::Closing(chunk) => chunk.all_table_names(names),
            ChunkState::Moving(chunk) | ChunkState::Closed(chunk) => chunk.all_table_names(names),
            ChunkState::Moved(db) => {
                db.all_table_names(self.partition_key.as_str(), &[self.id], names)
            }
            ChunkState::ObjectStore(chunk) => chunk.all_table_names(names),
        }
    }

    /// Returns a mutable reference to the mutable buffer storage for
    /// chunks in the Open or Closing state
    ///
    /// Must be in open or closing state
    pub fn mutable_buffer(&mut self) -> Result<&mut MBChunk> {
        match &mut self.state {
            ChunkState::Open(chunk) => Ok(chunk),
            ChunkState::Closing(chunk) => Ok(chunk),
            state => unexpected_state!(self, "mutable buffer reference", "Open or Closing", state),
        }
    }

    /// Set the chunk to the Closing state
    pub fn set_closing(&mut self) -> Result<()> {
        let mut s = ChunkState::Invalid;
        std::mem::swap(&mut s, &mut self.state);

        match s {
            ChunkState::Open(mut s) | ChunkState::Closing(mut s) => {
                s.mark_closing();
                self.state = ChunkState::Closing(s);
                Ok(())
            }
            state => {
                self.state = state;
                unexpected_state!(self, "setting closing", "Open or Closing", &self.state)
            }
        }
    }

    /// Set the chunk to the Moving state, returning a handle to the underlying
    /// storage
    pub fn set_moving(&mut self) -> Result<Arc<MBChunk>> {
        let mut s = ChunkState::Invalid;
        std::mem::swap(&mut s, &mut self.state);

        match s {
            ChunkState::Open(chunk) | ChunkState::Closing(chunk) => {
                let chunk = Arc::new(chunk);
                self.state = ChunkState::Moving(Arc::clone(&chunk));
                Ok(chunk)
            }
            ChunkState::Closed(chunk) => {
                self.state = ChunkState::Moving(Arc::clone(&chunk));
                Ok(chunk)
            }
            state => {
                self.state = state;
                unexpected_state!(
                    self,
                    "setting moving",
                    "Open, Closing or Closed",
                    &self.state
                )
            }
        }
    }

    /// Set the chunk in the Moved state, setting the underlying
    /// storage handle to db, and discarding the underlying mutable buffer
    /// storage.
    pub fn set_moved(&mut self, db: Arc<ReadBufferDb>) -> Result<()> {
        let mut s = ChunkState::Invalid;
        std::mem::swap(&mut s, &mut self.state);

        match s {
            ChunkState::Moving(_) => {
                self.state = ChunkState::Moved(db);
                Ok(())
            }
            state => {
                self.state = state;
                unexpected_state!(self, "setting moved", "Moving", &self.state)
            }
        }
    }

    /// Set the chunk to the ObjectStore state, returning a handle to the
    /// underlying storage
    pub fn set_object_store(&mut self, chunk: Arc<ParquetChunk>) -> Result<Arc<ParquetChunk>> {
        let mut s = ChunkState::Invalid;
        std::mem::swap(&mut s, &mut self.state);

        // TODO: Need to see from which state we can persist to object store
        match s {
            ChunkState::Closed(_) => {
                self.state = ChunkState::ObjectStore(Arc::clone(&chunk));
                Ok(chunk)
            }
            state => {
                self.state = state;
                unexpected_state!(self, "setting object store", "Closed", &self.state)
            }
        }
    }
}
