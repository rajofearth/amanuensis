mod model;
mod nemotron;
pub mod worker;

pub use worker::{Command, Event, spawn_worker};

trait AsrBackend {
    fn feed_audio(&mut self, samples: &[f32]);
    fn partial(&self) -> Option<String>;
    fn finalize(&mut self) -> String;
}
