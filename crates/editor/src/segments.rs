use cap_audio::AudioData;
use cap_project::ProjectConfiguration;
use std::{path::Path, sync::Arc};

use crate::{
    SegmentMedia,
    audio::{AudioSegment, AudioSegmentTrack},
};

pub fn load_music_track(
    project_path: &Path,
    config: &ProjectConfiguration,
) -> Option<Arc<AudioData>> {
    let file_name = config.audio.music_path.as_ref()?;
    let path = project_path.join(file_name);
    match AudioData::from_file(&path) {
        Ok(data) => Some(Arc::new(data)),
        Err(e) => {
            tracing::warn!(?e, ?path, "Failed to load background music, skipping");
            None
        }
    }
}

pub fn get_audio_segments(segments: &[SegmentMedia]) -> Vec<AudioSegment> {
    segments
        .iter()
        .map(|s| AudioSegment {
            tracks: [
                s.audio.clone().map(|a| {
                    AudioSegmentTrack::new(
                        a,
                        |c| c.mic_volume_db,
                        |c| match c.mic_stereo_mode {
                            cap_project::StereoMode::Stereo => cap_audio::StereoMode::Stereo,
                            cap_project::StereoMode::MonoL => cap_audio::StereoMode::MonoL,
                            cap_project::StereoMode::MonoR => cap_audio::StereoMode::MonoR,
                        },
                        |o| o.mic,
                    )
                }),
                s.system_audio.clone().map(|a| -> AudioSegmentTrack {
                    AudioSegmentTrack::new(
                        a,
                        |c| c.system_volume_db,
                        |_| cap_audio::StereoMode::Stereo,
                        |o| o.system_audio,
                    )
                }),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>(),
        })
        .collect::<Vec<_>>()
}
