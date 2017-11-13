
use network_encoding::*;
use db::*;
use util;
use hash::*;
use Header;
pub enum BlockAddHeaderOk {
    Invalid,
    Orphan,

}

pub enum VerifyFlags {
    NoVerifySignatures,
    VerifyAll
}



pub enum HeaderAddResult {
    Ok,
    AlreadyExists,
    Invalid,
    Orphan([u8;32])
}
/// Adds a header
///
pub fn header_add(db: &mut Db, hash: &[u8;32], header: Header) -> Result<HeaderAddResult, DbError> {

    if let Some(_) = db_header::get(db, &hash)? {
        Ok(HeaderAddResult::AlreadyExists)

    } else if let Some((parent_ptr, parent)) = db_header::get(db, &header.prev_hash)? {

        let db_header = db_header::DbHeader::new(parent, parent_ptr, header);
        db_header::write_header(db, hash, db_header)?;
        Ok(HeaderAddResult::Ok)

    } else {

        Ok(HeaderAddResult::Orphan(header.prev_hash))
    }
}


pub enum BlockExistsOk {
    NotFound,
    FoundHeaderOrphan,
    FoundHeader,
    FoundHeaderAndData
}

pub fn block_add_transactions(db: &mut Db, block_data: &[u8], validate: bool) -> Result<(), DbError>
{
    Ok(())
}



pub fn block_exists(blockhash: &[u8;32]) -> Result<BlockExistsOk, DbError> {
    unimplemented!()
}

/// Returns the hash of the block header with the most accumulated work
pub fn header_get_best(db: &mut Db) -> Result<[u8;32], DbError> {

    Ok(db_header::get_best(db)?)
}

/// Returns the hash of the block header with the most accumulated work
pub fn block_get_best(db: &mut Db) -> Result<[u8;32], DbError> {

    Ok(db_header::get_best(db)?)
}


pub fn header_get(db: &mut Db, hash: &[u8;32]) -> Result<Option<Header>, DbError> {
    Ok(db_header::get(db, hash)?.map(|(_, db_hdr)| db_hdr.header))
}


/// Constructs a locator object for the given block hash
///
/// This consists of the blockhash and at most 32 hashes ancestor hashes,
/// ending in Genesis
pub fn block_get_locator(db: &mut Db, blockhash: &[u8;32]) -> Result<Vec<[u8; 32]>, DbError> {

    Ok(db_header::get_locator(db, blockhash)?)
}


