use super::*;

#[derive(Debug)]
pub struct Block {
    pub block_type: BlockType,
    pub flags: BlockFlags,
    pub crc_type: CrcType,
    pub data_offset: usize,
    pub data_len: usize,
}
