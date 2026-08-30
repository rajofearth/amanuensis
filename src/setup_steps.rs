#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupStep {
    Welcome,
    HowItWorks,
    Shortcuts,
    MicCheck,
    Downloading,
    DetectHardware,
    Measuring,
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepEvent {
    NavNext,
    NavBack,
    StartDownload,
    DownloadFinished,
    DownloadCancelled,
    DownloadFailed,
}

pub fn next_step(step: SetupStep, event: StepEvent) -> Option<SetupStep> {
    match (step, event) {
        (SetupStep::Welcome, StepEvent::NavNext) => Some(SetupStep::HowItWorks),
        (SetupStep::HowItWorks, StepEvent::NavNext) => Some(SetupStep::Shortcuts),
        (SetupStep::HowItWorks, StepEvent::NavBack) => Some(SetupStep::Welcome),
        (SetupStep::Shortcuts, StepEvent::NavNext) => Some(SetupStep::MicCheck),
        (SetupStep::Shortcuts, StepEvent::NavBack) => Some(SetupStep::HowItWorks),
        (SetupStep::MicCheck, StepEvent::NavBack) => Some(SetupStep::Shortcuts),
        (SetupStep::MicCheck, StepEvent::StartDownload) => Some(SetupStep::Downloading),
        (SetupStep::Downloading, StepEvent::DownloadFinished) => Some(SetupStep::DetectHardware),
        (SetupStep::Downloading, StepEvent::DownloadCancelled) => Some(SetupStep::MicCheck),
        (SetupStep::Downloading, StepEvent::DownloadFailed) => Some(SetupStep::MicCheck),
        (SetupStep::DetectHardware, StepEvent::NavNext) => Some(SetupStep::Measuring),
        (SetupStep::DetectHardware, StepEvent::NavBack) => Some(SetupStep::MicCheck),
        (SetupStep::Measuring, StepEvent::NavNext) => Some(SetupStep::Ready),
        (SetupStep::Measuring, StepEvent::NavBack) => Some(SetupStep::DetectHardware),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tour_navigation_walks_forward_and_back() {
        assert_eq!(
            next_step(SetupStep::Welcome, StepEvent::NavNext),
            Some(SetupStep::HowItWorks)
        );
        assert_eq!(
            next_step(SetupStep::HowItWorks, StepEvent::NavNext),
            Some(SetupStep::Shortcuts)
        );
        assert_eq!(
            next_step(SetupStep::Shortcuts, StepEvent::NavNext),
            Some(SetupStep::MicCheck)
        );
        assert_eq!(
            next_step(SetupStep::MicCheck, StepEvent::NavBack),
            Some(SetupStep::Shortcuts)
        );
        assert_eq!(
            next_step(SetupStep::Shortcuts, StepEvent::NavBack),
            Some(SetupStep::HowItWorks)
        );
        assert_eq!(
            next_step(SetupStep::HowItWorks, StepEvent::NavBack),
            Some(SetupStep::Welcome)
        );
    }

    #[test]
    fn mic_check_start_walks_download_to_ready_via_bench_steps() {
        assert_eq!(
            next_step(SetupStep::MicCheck, StepEvent::StartDownload),
            Some(SetupStep::Downloading)
        );
        assert_eq!(
            next_step(SetupStep::Downloading, StepEvent::DownloadFinished),
            Some(SetupStep::DetectHardware)
        );
        assert_eq!(
            next_step(SetupStep::DetectHardware, StepEvent::NavNext),
            Some(SetupStep::Measuring)
        );
        assert_eq!(
            next_step(SetupStep::Measuring, StepEvent::NavNext),
            Some(SetupStep::Ready)
        );
    }

    #[test]
    fn hardware_and_measuring_navigate_back() {
        assert_eq!(
            next_step(SetupStep::DetectHardware, StepEvent::NavBack),
            Some(SetupStep::MicCheck)
        );
        assert_eq!(
            next_step(SetupStep::Measuring, StepEvent::NavBack),
            Some(SetupStep::DetectHardware)
        );
    }

    #[test]
    fn cancel_and_failure_return_to_mic_check() {
        assert_eq!(
            next_step(SetupStep::Downloading, StepEvent::DownloadCancelled),
            Some(SetupStep::MicCheck)
        );
        assert_eq!(
            next_step(SetupStep::Downloading, StepEvent::DownloadFailed),
            Some(SetupStep::MicCheck)
        );
    }

    #[test]
    fn invalid_transitions_are_rejected() {
        assert_eq!(
            next_step(SetupStep::Welcome, StepEvent::DownloadFinished),
            None
        );
        assert_eq!(
            next_step(SetupStep::Welcome, StepEvent::StartDownload),
            None
        );
        assert_eq!(next_step(SetupStep::MicCheck, StepEvent::NavNext), None);
        assert_eq!(next_step(SetupStep::Ready, StepEvent::StartDownload), None);
        assert_eq!(
            next_step(SetupStep::Ready, StepEvent::DownloadCancelled),
            None
        );
        assert_eq!(next_step(SetupStep::Ready, StepEvent::NavNext), None);
        assert_eq!(
            next_step(SetupStep::Measuring, StepEvent::DownloadFinished),
            None
        );
        assert_eq!(
            next_step(SetupStep::DetectHardware, StepEvent::StartDownload),
            None
        );
        assert_eq!(next_step(SetupStep::Downloading, StepEvent::NavBack), None);
    }
}
