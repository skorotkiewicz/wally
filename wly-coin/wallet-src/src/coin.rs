use bitcoin::{Block, CompactTarget, Target, consensus};
pub const PORT: u16 = 19335;
pub const MAGIC: [u8; 4] = [144, 160, 218, 130];
pub const P2PKH: u8 = 99;
pub const P2SH: u8 = 39;
pub const TIMESPAN: u64 = 120960;
pub const PEERS: &[&str] = &[];
pub fn genesis() -> Block {
    consensus::deserialize(&hex::decode("010000000000000000000000000000000000000000000000000000000000000000000000cc570b56d0e5602ea81278c2c081135d727a57d1d0fb5248faae4048c169c95f6d0ac56affff001f93d201000101000000010000000000000000000000000000000000000000000000000000000000000000ffffffff3704ffff001d01042f57616c6c792028574c59292067656e6573697320313739313239383135372038616334306636323936376461333261ffffffff0100f2052a01000000434104678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5fac00000000").expect("Generated genesis hex"))
        .expect("Generated genesis block")
}
pub fn pow_limit() -> Target {
    CompactTarget::from_consensus(520159231).into()
}
pub fn params() -> consensus::Params {
    let mut params = consensus::Params::MAINNET;
    params.max_attainable_target = pow_limit();
    params.pow_target_spacing = 60;
    params.pow_target_timespan = TIMESPAN;
    params
}
