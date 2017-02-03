

/// The spent tree stores the location of transactions in the block-tree
///
/// It is tracks the tree of blocks and is used to verify whether a block can be inserted at a
/// certain location in the tree
///
/// A block consists of the chain of records:
///
/// [block-header] <- [transaction] <- [spent-output] <- [spent-output] <- [transaction] <- ...
///
/// One [spent-output] record is added per input of a transaction.
///
/// Blocks are added regardless of they connect to a previous block. If the previous block comes in
/// later the following blocks are readded.
///
/// Verification takes place on *tip-propagation*. The top pointer is moved forward to the next block
/// after all [spent-outputs] in that block have been checked. This entails scanning back the chain.
/// The scan is succesful if the transaction is found and unsuccesful if it is not found or if the
/// same spent-output is found before the transaction.
///
///
///
///

use std::sync::atomic;

use std::mem;

use itertools::Itertools;
use buffer::*;

use config;
use rayon::prelude::*;

#[macro_use]
use slog;

use store::fileptr::FilePtr;
use store::flatfileset::FlatFileSet;

use store::block_content::BlockContent;
use store::hash_index::HashIndex;

use transaction::Transaction;
use simple_parallel;
use crossbeam;

mod params;

pub mod record;
pub use self::record::{Record,RecordPtr};

const MB:                 u32 = 1024 * 1024;
const FILE_SIZE:          u32 = 1024 * MB as u32;
const MAX_CONTENT_SIZE:   u32 = FILE_SIZE - 10 * MB as u32 ;

const SUBPATH: &'static str   = "spent_tree";
const PREFIX:  &'static str   = "st-";


// temporarily we use a vec instead of the memmap
const VEC_SIZE: usize = 500_000_000;

#[derive(Debug, PartialEq)]
pub enum SpendingError {
    OutputNotFound,
    OutputAlreadySpent,
}

pub struct BlockPtr {
    pub start: RecordPtr,
    pub end:   RecordPtr
}


pub struct SpentTree {

    fileset:    FlatFileSet,

    stats: SpentTreeStats
}



#[derive(Debug, Default)]
pub struct SpentTreeStats {
    pub blocks:     i64,
    pub inputs:     i64,
    pub seeks:      i64,
    pub total_move: i64,
    pub jumps:      i64,
    pub use_diff:   [i64; params::SKIP_FIELDS]
}

// Make stats additive
impl ::std::ops::Add for SpentTreeStats {
    type Output = SpentTreeStats;

    fn add(self, other: SpentTreeStats) -> SpentTreeStats {
        // sum use_diff array
        let mut use_diff: [i64; params::SKIP_FIELDS] = Default::default();
        for n in 0..use_diff.len() { use_diff[n] = self.use_diff[n] + other.use_diff[n] };

        SpentTreeStats {
            blocks: self.blocks + other.blocks,
            inputs: self.inputs + other.inputs,
            seeks:  self.seeks +  other.seeks,
            total_move: self.total_move + other.total_move,
            jumps: self.jumps + other.jumps,
            use_diff: use_diff
        }
    }
}



/// This is the primary algorithm to check double-spents and the existence of outputs
fn seek_and_set_inputs(
                       records: &[Record],
                       block: &mut [Record],
                       block_idx: usize,
                       logger: &slog::Logger) -> Result<SpentTreeStats, SpendingError>
{



    let mut results: Vec<Result<SpentTreeStats, SpendingError>> = block

        .par_iter_mut()
        .enumerate()
        .map(|(i,rec)| {

            debug_assert!(rec.ptr.is_transaction() || rec.ptr.is_output());

            rec.seek_and_set(block_idx+i, records, logger)

        })
        .collect();

    results.into_iter().fold_results(Default::default(), |a,b| { a+b } )

}


impl SpentTree {
    pub fn new(cfg: &config::Config) -> SpentTree {

        let dir = &cfg.root.clone().join(SUBPATH);

        let stats: SpentTreeStats = Default::default();



        SpentTree {
            fileset: FlatFileSet::new(
                dir, PREFIX, FILE_SIZE, MAX_CONTENT_SIZE),

            stats: stats
        }
    }





    /// Converts the set of block_content-fileptrs
    /// into a set of records to be stored in the spent_tree
    ///
    pub fn create_block(blockheader: FilePtr, file_ptrs: Vec<FilePtr>) -> Vec<Record> {

        let mut result: Vec<Record> = Vec::with_capacity(file_ptrs.len()+2);

        result.push(Record::new(blockheader.to_guardblock()));

        for ptr in file_ptrs.iter() {

            let mut r = Record::new(*ptr);
            result.push(r);
        };

        let mut rec_end = Record::new(blockheader.to_block());
        result.push(rec_end);
        result
    }


    /// Retrieves the data pointed to by the spent-tree record at `ptr`
    /// This resolves the indirection: The passed ptr points to the spent-tree record
    /// This record points to the block_content
    pub fn load_data_from_spent_tree_ptr<'a>(&'a mut self, block_content: &'a mut BlockContent, ptr: FilePtr) -> &[u8] {
        let rec: &Record = self.fileset.read_fixed(ptr);
        let ptr = rec.ptr;

        block_content.read(ptr)
    }

    /// Stores a block in the spent_tree. The block will be initially orphan.
    ///
    /// The result is a pointer to the first and last record
    pub fn store_block(&mut self, blockheader: FilePtr, file_ptrs: Vec<FilePtr>) -> BlockPtr {

        let block = SpentTree::create_block(blockheader, file_ptrs);


        let result_ptr = self.fileset.write_all(&block);
        let end_ptr = result_ptr.offset(((block.len()-1) * mem::size_of::<Record>()) as i32);

        BlockPtr {
            start: RecordPtr::new(result_ptr),
            end:   RecordPtr::new(end_ptr)
        }
    }



    pub fn find_end(&mut self, target_start: RecordPtr) -> RecordPtr {
        let mut this_ptr = target_start;


        loop {
            this_ptr = this_ptr.next_in_block();

            let record = self.fileset.read_fixed::<Record>(this_ptr.ptr);

            if record.ptr.is_blockheader() {
                this_ptr.set_previous(&mut self.fileset, Some(this_ptr.prev_in_block()));
                return this_ptr;
            }
            else {
                record.skips = [-1;params::SKIP_FIELDS];
            }
        }
    }

    /// If an orphan block is stored in the spent-tree, some transaction-inputs might not be resolved
    /// to their outputs. These will still be null-pointers instead of output-pointers
    ///
    /// This looks up the corresponding outputs; needs to be called before connect_block
    pub fn revolve_orphan_pointers(&mut self,
                                   block_content: &mut BlockContent,
                                   hash_index:    &mut HashIndex,
                                   target_start:  RecordPtr) {

        let mut tx_ptr = FilePtr::null();
        let mut input_idx = 0;

        let mut this_ptr = target_start;
        debug_assert!(target_start.ptr.is_guard_blockheader());

        loop {

            this_ptr = this_ptr.next_in_block();

            let ptr = this_ptr.get_content_ptr(&mut self.fileset);

            if ptr.is_null() {

                let bytes =  block_content.read(tx_ptr);
                let mut buf = Buffer::new(bytes);
                let tx = Transaction::parse(&mut buf).unwrap();

                let input = &tx.txs_in[input_idx];

                let input_ptr = hash_index
                    .get(input.prev_tx_out)
                    .iter()
                    .find(|ptr| ptr.is_transaction())
                    .expect("Could not find input; this must have been resolved before connecting the block")
                    .to_output(input.prev_tx_out_idx);

                this_ptr.set_content_ptr(&mut self.fileset, input_ptr);

                input_idx += 1;
            } else if ptr.is_transaction() {
                tx_ptr = ptr;
                input_idx = 0;
            }
            else if ptr.is_blockheader() {

                return;
            }
            else {
                input_idx += 1;
            }

        }

    }



    /// Verifies of each output in the block at target_start
    /// Then lays the connection between previous_end and target_start
    pub fn connect_block(&mut self,
                         logger: &slog::Logger,
                         previous_end: RecordPtr,
                         target_start: RecordPtr) -> Result<RecordPtr, SpendingError> {

        let timer = ::std::time::Instant::now();

        info!(logger, "start scan");

        // we can now make the actual connection
        target_start.set_previous(&mut self.fileset, Some(previous_end));

        let mut this_ptr = target_start;

        // count the records
        let blocksize = this_ptr.iter(&mut self.fileset).count();

        println!("size: {:?}", blocksize);

        let block_idx              = this_ptr.to_index() + 1;
        let block:   &mut [Record] = self.fileset.read_mut_slice(this_ptr.next_in_block().ptr, blocksize);

        for r in block {
            r.set_prev_minus_one();
        }

        let block:   &mut [Record] = self.fileset.read_mut_slice(this_ptr.next_in_block().ptr, blocksize);

        let records: &[Record] = self.fileset.read_mut_slice(FilePtr::new(0,16), 150_000_000);

        let stats = seek_and_set_inputs(records, block, block_idx, logger)?;

        //let mut error: Option<SpendingError> = None;

        let end_ptr: RecordPtr   = this_ptr.offset(1+blocksize as i32);

        // the blockheader at the end just points to the record before itself:
        end_ptr.set_previous(&mut self.fileset, Some(end_ptr.prev_in_block()));

        // we can now make the actual connection
        target_start.set_previous(&mut self.fileset, Some(previous_end));

        let elaps : isize = timer.elapsed().as_secs() as isize * 1000 +
            timer.elapsed().subsec_nanos() as isize / 1_000_000 as isize;


        info!(logger, "scan_stats";
            "stats" => format!("{:?}", stats),
            "inputs" => stats.inputs,
            "ms/inputs" => (elaps+1) as f64 / stats.inputs as f64,
            "seek_avg" => stats.seeks / (stats.inputs+1)
        );


        Ok(end_ptr)
    }

}





#[cfg(test)]
mod tests {

    extern crate tempdir;
    use store::fileptr::FilePtr;


    use config;
    use slog_term;
    use slog;
    use slog::DrainExt;

    use super::*;

    /// Macro to create a block for the spent_tree tests
    /// blockheaders and txs are unqiue numbers (fileptrs but where they point to doesn't matter
    /// for the spent_tree).
    ///
    /// Construct a block as
    ///
    /// ```
    /// (blk 1 =>                 /* blocknr */
    ///   [tx 2],                /* tx with no inputs  */
    ///   [tx 3 => (2;0),(2;1)]  /* tx with two inputs referencing tx 2 ouput 0 and 1
    /// )
    ///
    macro_rules! block {

        (blk $header:expr =>
          $( [tx $tx:expr  $( => $(   ($tx_in:expr;$tx_in_idx:expr) ),* ),* ] ),*
        )
        =>
        (  ( FilePtr::new(0,$header), vec![
               $( FilePtr::new(0,$tx)  $( ,  $( FilePtr::new(0,$tx_in).to_output($tx_in_idx) ),* ),* ),*
            ])
        )

    }

    impl SpentTree {
        // wrapper around store_block that accepts a tuple instead of two params
        // for easier testing with block! macros
        fn store(&mut self, tuple: (FilePtr, Vec<FilePtr>)) -> BlockPtr {
            self.store_block(tuple.0, tuple.1)
        }
    }

    #[test]
    fn test_spent_tree_connect() {
        let mut log = slog::Logger::root(slog_term::streamer().compact().build().fuse(), o!());

        let mut st  = SpentTree::new(&config::Config::new_test());

        let block1 = st.store(block!(blk 1 =>
            [tx 2]
        ));

        let block2a = st.store(block!(blk 3 =>
            [tx 4 => (2;0)]
        ));

        let block2b = st.store(block!(blk 5 =>
            [tx 6 => (2;0)]
        ));


        // create a tree, both 2a and 2b attached to 1
        st.find_end(block1.start);
        st.connect_block(&log, block1.end, block2a.start).unwrap();
        st.connect_block(&log, block1.end, block2b.start).unwrap();

        // this one should only "fit" onto 2b
        let block3b = st.store(block!(blk 7 =>
            [tx 8 => (6;1)],
            [tx 9 => (2;1)]
        ));


        assert_eq!(
            st.connect_block(&log, block2a.end, block3b.start).unwrap_err(),
            SpendingError::OutputNotFound);

        st.connect_block(&log, block2b.end, block3b.start).unwrap();

        // now this should only fir on 2a and not on 3b as at 3b it is already spent
        let block4a = st.store(block!(blk 10 =>
            [tx 11 => (2;1)],
            [tx 12 => (2;2)]
        ));
        assert_eq!(
            st.connect_block(&log, block3b.end, block4a.start).unwrap_err(),
            SpendingError::OutputAlreadySpent);
        st.connect_block(&log, block2b.end, block4a.start).unwrap();

    }

    #[test]
    fn test_spent_tree1() {
        let log = slog::Logger::root(slog_term::streamer().compact().build().fuse(), o!());


        let block1 = block!(blk 1 =>
            [tx 2 => (2;1),(2;0)],
            [tx 3]
        );


        let mut st  = SpentTree::new(& config::Config::new_test() );

        let block_ptr = st.store_block(block1.0, block1.1);

        let block2 = block!(blk 4 =>
            [tx 5 => (2;2),(2;3)],
            [tx 6 ]
        );

        let block_ptr2 = st.store_block(block2.0, block2.1);

        println!("{:?}", block_ptr.start);
        st.find_end(block_ptr.start);

        st.connect_block(&log, block_ptr.end, block_ptr2.start).unwrap();

        // we browse backwards and test all values
        let p = block_ptr2.end;
        assert!   (p.get_content_ptr(&mut st.fileset).is_blockheader());
        assert_eq!(p.get_content_ptr(&mut st.fileset).file_pos(), 4);

        let p = { p.prev(&mut st.fileset) };
        assert!(   p.get_content_ptr(&mut st.fileset).is_transaction());
        assert_eq!(p.get_content_ptr(&mut st.fileset).file_pos(), 6);

        let p = { p.prev(&mut st.fileset) };
        assert!   (p.get_content_ptr(&mut st.fileset).is_output());
        assert_eq!(p.get_content_ptr(&mut st.fileset).file_pos(), 2);
        assert_eq!(p.get_content_ptr(&mut st.fileset).output_index(), 3);

        let p = { p.prev(&mut st.fileset) };
        assert!(   p.get_content_ptr(&mut st.fileset).is_output());
        assert_eq!(p.get_content_ptr(&mut st.fileset).file_pos(), 2);
        assert_eq!(p.get_content_ptr(&mut st.fileset).output_index(), 2);

        let p = { p.prev(&mut st.fileset) };
        assert!(   p.get_content_ptr(&mut st.fileset).is_transaction());
        assert_eq!(p.get_content_ptr(&mut st.fileset).file_pos(), 5);

        let p = { p.prev(&mut st.fileset) };
        assert!   (p.get_content_ptr(&mut st.fileset).is_guard_blockheader());
        assert_eq!(p.get_content_ptr(&mut st.fileset).file_pos(), 4);


        let p = { p.prev(&mut st.fileset) };
        assert!   (p.get_content_ptr(&mut st.fileset).is_blockheader());
        assert_eq!(p.get_content_ptr(&mut st.fileset).file_pos(), 1);

    }


    #[test]
    fn test_orphan_block() {
        // care must be taken that when a block is added to the spent-tree before one of its
        // predecessors, it may not be complete.
        // This because in the spent tree, the inputs are stored as the outputs they reference,
        // but these inputs may not have been available.

        // The resolve orphan block checks and fixes these "left-out" references.


    }
}

