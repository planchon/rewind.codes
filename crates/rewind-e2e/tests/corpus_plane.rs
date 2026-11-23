//! One test per case of `corpus/`, run by the run plane's actors in the driver: live, against
//! the committed journal byte for byte, replayed. See the crate's documentation.

include!(concat!(env!("OUT_DIR"), "/corpus_plane.rs"));
