//! Import photos and videos from microscope SD cards, and stitch the camera's
//! fixed-length video segments back into whole recordings.

pub mod app;
pub mod devices;
pub mod estimate;
pub mod import;
pub mod paths;
pub mod stitch;
pub mod udisks;
pub mod ui;
pub mod util;
