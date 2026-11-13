//! Converting a source with FFmpeg, and delivering what it wrote to the library.

mod builder;
mod chunks;
mod delivery;
mod operation;
mod processor;
mod subtitle_streams;
#[cfg(test)]
mod test_support;

pub use builder::FFmpegCommandBuilder;
pub use chunks::{chunk_dir_for, plan_chunks, Chunk, Chunking, CHUNK_SECONDS, MIN_CHUNKED_SECONDS};
pub use delivery::staging_path_for;
pub use processor::FFmpegProcessor;
pub use subtitle_streams::{
    is_bitmap_subtitle, select_subtitle_streams, SubtitleSelection, SubtitleStream,
};
