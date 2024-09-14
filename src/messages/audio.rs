use bevy::{math::DVec3, prelude::Event};
use serde::{Deserialize, Serialize};

use fmc_protocol_derive::ClientBound;

/// Play sounds on client
#[derive(ClientBound, Event, Serialize, Deserialize, Debug, Clone, Default)]
pub struct Sound {
    /// Position the sound should be emitted from. If "None", the sound will be heard uniformly from
    /// all directions.
    pub position: Option<DVec3>,
    // TODO: Make this into an integer id to save bandwidth.
    //
    /// Sound that should be played.
    pub sound: String,
}

/// For responsiveness the client is able to play the sound of walking on/in blocks, this allows the server
/// to decide if it should be enabled or not.
#[derive(ClientBound, Event, Serialize, Deserialize, Debug, Clone, Default)]
pub struct EnableClientAudio(pub bool);
