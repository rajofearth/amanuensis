use amanuensis::asr::{DownloadProgress, ModelKind};

pub(crate) enum HotkeyMessage {
    ToggleRecording,
    DebugReload,
}

pub(crate) enum PasteResult {
    Pasted(usize),
    Failed(String),
}

pub(crate) enum DownloadMessage {
    Progress {
        generation: u64,
        progress: DownloadProgress,
    },
    Finished {
        generation: u64,
        result: Result<ModelKind, String>,
    },
    Cancelled {
        generation: u64,
    },
}

pub(crate) enum UiMessage {
    OpenSettings,
    TrayToggleRecording,
    TraySetEnabled(bool),
    Quit,
    PanelClosed,
    StartDownload {
        captured_model: &'static str,
        purge: bool,
    },
    CancelDownload,
    FinishOnboarding {
        captured_model: &'static str,
    },
    DeleteRequest {
        captured_model: &'static str,
    },
    ResetSetup {
        captured_model: &'static str,
    },
    DeleteFinished(Result<(), String>),
    PillDiscard,
    PillFinish,
}
