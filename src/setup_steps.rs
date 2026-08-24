#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupStep {
    Welcome,
    Downloading,
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepEvent {
    StartDownload,
    DownloadFinished,
    DownloadCancelled,
    DownloadFailed,
}

pub fn next_step(step: SetupStep, event: StepEvent) -> Option<SetupStep> {
    match (step, event) {
        (SetupStep::Welcome, StepEvent::StartDownload) => Some(SetupStep::Downloading),
        (SetupStep::Downloading, StepEvent::DownloadFinished) => Some(SetupStep::Ready),
        (SetupStep::Downloading, StepEvent::DownloadCancelled) => Some(SetupStep::Welcome),
        (SetupStep::Downloading, StepEvent::DownloadFailed) => Some(SetupStep::Welcome),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_walks_all_steps() {
        assert_eq!(
            next_step(SetupStep::Welcome, StepEvent::StartDownload),
            Some(SetupStep::Downloading)
        );
        assert_eq!(
            next_step(SetupStep::Downloading, StepEvent::DownloadFinished),
            Some(SetupStep::Ready)
        );
    }

    #[test]
    fn cancel_and_failure_return_to_welcome() {
        assert_eq!(
            next_step(SetupStep::Downloading, StepEvent::DownloadCancelled),
            Some(SetupStep::Welcome)
        );
        assert_eq!(
            next_step(SetupStep::Downloading, StepEvent::DownloadFailed),
            Some(SetupStep::Welcome)
        );
    }

    #[test]
    fn invalid_transitions_are_rejected() {
        assert_eq!(
            next_step(SetupStep::Welcome, StepEvent::DownloadFinished),
            None
        );
        assert_eq!(next_step(SetupStep::Ready, StepEvent::StartDownload), None);
        assert_eq!(
            next_step(SetupStep::Ready, StepEvent::DownloadCancelled),
            None
        );
    }
}
