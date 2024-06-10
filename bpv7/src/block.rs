use super::*;

#[derive(Debug)]
pub struct Block {
    pub block_type: prelude::BlockType,
    pub flags: prelude::BlockFlags,
    pub crc_type: prelude::CrcType,
    pub data_offset: usize,
    pub data_len: usize,
}
