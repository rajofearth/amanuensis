use std::path::Path;

use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig};

use super::model::ModelPaths;
use super::{AsrBackend, provider, threads};

const SAMPLE_RATE: i32 = 16000;
const FEATURE_DIM: i32 = 80;

pub struct MoonshineBackend {
    recognizer: OfflineRecognizer,
    session: Option<Vec<f32>>,
}

impl MoonshineBackend {
    pub fn load(
        paths: &ModelPaths,
        preferred_provider: Option<&str>,
        preferred_threads: i32,
    ) -> Option<Self> {
        let ModelPaths::Moonshine {
            preprocessor,
            encoder,
            uncached_decoder,
            cached_decoder,
            tokens,
        } = paths
        else {
            return None;
        };
        let mut config = OfflineRecognizerConfig::default();
        config.feat_config.sample_rate = SAMPLE_RATE;
        config.feat_config.feature_dim = FEATURE_DIM;
        config.model_config.moonshine.preprocessor = Some(path_string(preprocessor));
        config.model_config.moonshine.encoder = Some(path_string(encoder));
        config.model_config.moonshine.uncached_decoder = Some(path_string(uncached_decoder));
        config.model_config.moonshine.cached_decoder = Some(path_string(cached_decoder));
        config.model_config.tokens = Some(path_string(tokens));
        config.model_config.num_threads = threads(preferred_threads);
        if let Some(provider) = provider(preferred_provider) {
            config.model_config.provider = Some(provider);
        }
        OfflineRecognizer::create(&config).map(|recognizer| Self {
            recognizer,
            session: Some(Vec::new()),
        })
    }
}

impl AsrBackend for MoonshineBackend {
    fn start_session(&mut self) {
        self.session = Some(Vec::new());
    }

    fn feed_audio(&mut self, samples: &[f32]) {
        if let Some(session) = self.session.as_mut() {
            session.extend_from_slice(samples);
        }
    }

    fn partial(&self) -> Option<String> {
        None
    }

    fn finalize(&mut self) -> String {
        let Some(samples) = self.session.take() else {
            return String::new();
        };
        if samples.is_empty() {
            return String::new();
        }
        let stream = self.recognizer.create_stream();
        stream.accept_waveform(SAMPLE_RATE, &samples);
        self.recognizer.decode(&stream);
        stream
            .get_result()
            .map(|result| result.text)
            .unwrap_or_default()
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
