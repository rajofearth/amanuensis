use std::path::Path;

use sherpa_onnx::{
    OnlineRecognizer, OnlineRecognizerConfig, OnlineStream, OnlineTransducerModelConfig,
};

use super::model::ModelPaths;
use super::{AsrBackend, provider, threads};

const SAMPLE_RATE: i32 = 16000;
const FEATURE_DIM: i32 = 128;
const FINALIZE_SILENCE_SAMPLES: usize = 9600;

pub struct NemotronBackend {
    recognizer: OnlineRecognizer,
    stream: Option<OnlineStream>,
}

impl NemotronBackend {
    pub fn load(
        model: &ModelPaths,
        preferred_provider: Option<&str>,
        preferred_threads: i32,
    ) -> Option<Self> {
        let mut config = OnlineRecognizerConfig::default();
        config.feat_config.sample_rate = SAMPLE_RATE;
        config.feat_config.feature_dim = FEATURE_DIM;
        config.model_config.transducer = OnlineTransducerModelConfig {
            encoder: Some(path_string(&model.encoder)),
            decoder: Some(path_string(&model.decoder)),
            joiner: Some(path_string(&model.joiner)),
        };
        config.model_config.tokens = Some(path_string(&model.tokens));
        config.model_config.num_threads = threads(preferred_threads);
        if let Some(provider) = provider(preferred_provider) {
            config.model_config.provider = Some(provider);
        }
        config.decoding_method = Some("greedy_search".to_string());
        config.enable_endpoint = false;
        OnlineRecognizer::create(&config).map(|recognizer| Self {
            recognizer,
            stream: None,
        })
    }
}

impl AsrBackend for NemotronBackend {
    fn start_session(&mut self) {
        self.stream = Some(self.recognizer.create_stream());
    }

    fn feed_audio(&mut self, samples: &[f32]) {
        let Some(stream) = self.stream.as_ref() else {
            return;
        };
        stream.accept_waveform(SAMPLE_RATE, samples);
        while self.recognizer.is_ready(stream) {
            self.recognizer.decode(stream);
        }
    }

    fn partial(&self) -> Option<String> {
        let text = self.recognizer.get_result(self.stream.as_ref()?)?.text;
        (!text.is_empty()).then_some(text)
    }

    fn finalize(&mut self) -> String {
        let Some(stream) = self.stream.take() else {
            return String::new();
        };
        stream.accept_waveform(SAMPLE_RATE, &[0.0; FINALIZE_SILENCE_SAMPLES]);
        stream.input_finished();
        while self.recognizer.is_ready(&stream) {
            self.recognizer.decode(&stream);
        }
        self.recognizer
            .get_result(&stream)
            .map(|result| result.text)
            .unwrap_or_default()
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
