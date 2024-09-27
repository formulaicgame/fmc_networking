#![deny(
    //missing_docs,
    missing_debug_implementations,
    // why does it need this
    //missing_copy_implementations,
    trivial_casts,
    trivial_numeric_casts,
    unsafe_code,
    unstable_features,
    unused_import_braces,
    unused_qualifications,
    clippy::unwrap_used
)]
#![allow(clippy::type_complexity)]

mod network_message;

pub mod messages;
pub use network_message::{ClientBound, ServerBound};

// TODO: I want to increase block ids from u16 to u32. Doubling the memory size is bad. Instead
// replace the blocks a chunk holds with substitutes, and keep a mapping from substitute values to
// block ids. Then you can have 'lookup: Vec<BlockId>' and 'blocks: Vec<u16>', take the value you want
// from 'blocks' cast it to usize and index into the lookup with it, you now have the block id.
// This may even allow reducing the in-transit size by using even smaller types. I need to measure
// but I assume most chunks don't consist of more than a handful of blocks. Maybe it can go all the
// way down to 4 bits per block for most chunks, in which case keeping it in memory is a good trade
// off for not having to build the representation each time it is sent.
//
// TODO: Should probably define BlockState here too, to avoid hard to parse u16's and easier to
// change data type.
//
/// Storage type of blocks.
/// Used by both server and client.
type BlockId = u16;

#[derive(Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    ClientIdentification,
    ClientReady,
    AssetRequest,
    AssetResponse,
    Disconnect,
    ServerConfig,
    Time,
    Chunk,
    BlockUpdates,
    DeleteModel,
    ModelPlayAnimation,
    ModelUpdateAsset,
    ModelUpdateTransform,
    NewModel,
    SpawnCustomModel,
    LeftClick,
    RightClick,
    RenderDistance,
    PlayerAabb,
    PlayerCameraPosition,
    PlayerCameraRotation,
    PlayerPosition,
    InterfaceEquipItem,
    InterfaceInteraction,
    InterfaceItemBoxUpdate,
    InterfaceNodeVisibilityUpdate,
    InterfaceTextInput,
    InterfaceTextUpdate,
    InterfaceVisibilityUpdate,
    EnableClientAudio,
    Sound,
    // XXX: Always keep this at the bottom, occupies highest discriminant spot, so that when you
    // deserialize a MessageType, you can know that only values below 'MessageType::MAX as u8' are
    // valid.
    MAX,
}
